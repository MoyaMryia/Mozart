//! 桥共享状态（SPEC-05）：开关、字幕计数、翻译/播报计数、降级原因，控制线程只读快照。
//! 开关以 TOML 为基线，运行时命令覆盖并原子写状态文件存档（SPEC-04 模型甲）；
//! 桥是开关的活权威，状态文件只是快照，启动恢复一次，运行时不对账。
//! 待翻译/待播报队列是有界内存队列：待播报队满丢最旧并计数（SPEC-05 播报投递），
//! 待翻译队满拒收由字幕线程记降级；SIGTERM 退出时丢弃。

use mozart_core::ctl::schema::BridgeStatus;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 版本号，Hello 与 GetStatus 一致上报。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 运行时状态文件名：开关覆盖的存档快照，放持久目录（与字幕同目录）。
pub const STATE_FILE_NAME: &str = "bridge-state.json";

/// 状态文件路径：从 subtitles_path 父目录推导（持久存储，重启保留），
/// 不放 run_dir（tmpfs，重启丢失）。v1 仍由桥写盘。
pub fn state_path_for(subtitles_path: &Path) -> PathBuf {
    subtitles_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(STATE_FILE_NAME)
}

/// 当前 Unix 毫秒，供字幕时间戳与降级时间。
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 开关快照：TOML 基线与运行时覆盖共用同一结构。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Switches {
    pub translate_enabled: bool,
    pub speech_enabled: bool,
}

/// 读状态文件：缺失返回基线；损坏报错停止，不静默回落基线。
pub fn load_switches(path: &Path, baseline: Switches) -> Result<Switches, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(baseline),
        Err(err) => return Err(format!("读状态文件 {}：{err}", path.display())),
    };
    serde_json::from_str(&text)
        .map_err(|err| format!("解析状态文件 {}：{err}", path.display()))
}

/// 原子写：临时文件 + rename，崩溃不留半截文件。
pub fn save_switches(path: &Path, switches: &Switches) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(switches).expect("switches are JSON-serializable");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// 降级条目：原因与发生时间，经 GetStatus 可查询。
struct DegradedEntry {
    reason: String,
    at_unix_ms: u64,
}

/// 翻译组计数（SPEC-05 GetStatus）：翻译线程写，控制线程只读。
/// llama_connected 反映最近一次请求的连通结果（每请求一连，无长连接）。
#[derive(Default)]
pub struct TranslateStats {
    translated: AtomicU64,
    failed: AtomicU64,
    retried: AtomicU64,
    qc_rejected: AtomicU64,
    llama_connected: AtomicBool,
    last_ok_unix_ms: AtomicU64,
    last_fail_unix_ms: AtomicU64,
    last_fail_reason: Mutex<Option<String>>,
}

impl TranslateStats {
    /// 一次 HTTP 推理成功。
    pub fn attempt_ok(&self) {
        self.llama_connected.store(true, Relaxed);
        self.last_ok_unix_ms.store(unix_ms(), Relaxed);
    }

    /// 一次 HTTP 推理失败：记录时间与原因，供 GetStatus 查询。
    pub fn attempt_fail(&self, reason: &str) {
        self.llama_connected.store(false, Relaxed);
        self.last_fail_unix_ms.store(unix_ms(), Relaxed);
        *self
            .last_fail_reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason.to_string());
    }

    /// 整句成功（质检通过并落字幕）。
    pub fn add_translated(&self) {
        self.translated.fetch_add(1, Relaxed);
    }
    /// 重试耗尽后整句失败。
    pub fn add_failed(&self) {
        self.failed.fetch_add(1, Relaxed);
    }
    /// 一次实际发起的重试。
    pub fn add_retry(&self) {
        self.retried.fetch_add(1, Relaxed);
    }
    pub fn add_qc_rejected(&self) {
        self.qc_rejected.fetch_add(1, Relaxed);
    }
}

/// 播报组计数（SPEC-05 GetStatus）：投递线程写，控制线程只读。
/// delivered 计送达并收到受理应答的任务；受理结果再分 submitted/skipped/failed。
#[derive(Default)]
pub struct SpeechStats {
    delivered: AtomicU64,
    submitted: AtomicU64,
    skipped: AtomicU64,
    failed: AtomicU64,
    tts_connected: AtomicBool,
}

impl SpeechStats {
    pub fn add_delivered(&self) {
        self.delivered.fetch_add(1, Relaxed);
    }
    pub fn add_submitted(&self) {
        self.submitted.fetch_add(1, Relaxed);
    }
    pub fn add_skipped(&self) {
        self.skipped.fetch_add(1, Relaxed);
    }
    pub fn add_failed(&self) {
        self.failed.fetch_add(1, Relaxed);
    }
    pub fn set_connected(&self, connected: bool) {
        self.tts_connected.store(connected, Relaxed);
    }
}

pub struct Shared {
    pub subtitles_path: PathBuf,
    state_path: PathBuf,
    translate_enabled: AtomicBool,
    speech_enabled: AtomicBool,
    subtitles_published: AtomicU64,
    records_in_memory: AtomicU32,
    last_write_unix_ms: AtomicU64,
    translate_stats: TranslateStats,
    speech_stats: SpeechStats,
    degraded: Mutex<Vec<DegradedEntry>>,
}

impl Shared {
    pub fn new(switches: Switches, subtitles_path: PathBuf, state_path: PathBuf) -> Self {
        Self {
            subtitles_path,
            state_path,
            translate_enabled: AtomicBool::new(switches.translate_enabled),
            speech_enabled: AtomicBool::new(switches.speech_enabled),
            subtitles_published: AtomicU64::new(0),
            records_in_memory: AtomicU32::new(0),
            last_write_unix_ms: AtomicU64::new(0),
            translate_stats: TranslateStats::default(),
            speech_stats: SpeechStats::default(),
            degraded: Mutex::new(Vec::new()),
        }
    }

    pub fn translate_enabled(&self) -> bool {
        self.translate_enabled.load(Relaxed)
    }
    pub fn speech_enabled(&self) -> bool {
        self.speech_enabled.load(Relaxed)
    }

    pub fn translate_stats(&self) -> &TranslateStats {
        &self.translate_stats
    }
    pub fn speech_stats(&self) -> &SpeechStats {
        &self.speech_stats
    }

    /// 运行时开关覆盖：先存档再改内存；写盘失败返回错误，内存开关不变，命令整体被拒。
    pub fn set_switches(&self, switches: Switches) -> Result<(), String> {
        if let Err(err) = save_switches(&self.state_path, &switches) {
            self.degrade("state file write failed");
            return Err(format!("写状态文件 {}：{err}", self.state_path.display()));
        }
        self.translate_enabled
            .store(switches.translate_enabled, Relaxed);
        self.speech_enabled.store(switches.speech_enabled, Relaxed);
        self.resolve("state file write failed");
        Ok(())
    }

    /// 已发布句数按 utterance_id 计，不按 JSONL 行数（沿用旧实现）。
    pub fn add_published(&self) {
        self.subtitles_published.fetch_add(1, Relaxed);
    }
    pub fn set_records_in_memory(&self, count: u32) {
        self.records_in_memory.store(count, Relaxed);
    }
    pub fn set_last_write_unix_ms(&self, ms: u64) {
        self.last_write_unix_ms.store(ms, Relaxed);
    }

    /// 记录降级原因（同一原因只留首次时间）；只在降级路径调用。
    pub fn degrade(&self, reason: &str) {
        let mut degraded = self
            .degraded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if degraded.iter().any(|entry| entry.reason == reason) {
            return;
        }
        degraded.push(DegradedEntry {
            reason: reason.to_string(),
            at_unix_ms: unix_ms(),
        });
    }

    /// 降级恢复后移除对应原因。
    pub fn resolve(&self, reason: &str) {
        let mut degraded = self
            .degraded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        degraded.retain(|entry| entry.reason != reason);
    }

    fn degraded_strings(&self) -> Vec<String> {
        self.degraded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|entry| format!("{} (unix_ms={})", entry.reason, entry.at_unix_ms))
            .collect()
    }

    /// 组装 GetStatus 快照：四组字段全部来自原子计数与队列深度，只读。
    pub fn status(&self, queues: &Queues) -> BridgeStatus {
        let translate = &self.translate_stats;
        let speech = &self.speech_stats;
        BridgeStatus {
            translate_enabled: self.translate_enabled(),
            speech_enabled: self.speech_enabled(),
            translate_queue_depth: queues.translate_depth(),
            translated: translate.translated.load(Relaxed),
            translate_failed: translate.failed.load(Relaxed),
            translate_retried: translate.retried.load(Relaxed),
            qc_rejected: translate.qc_rejected.load(Relaxed),
            llama_connected: translate.llama_connected.load(Relaxed),
            last_translate_ok_unix_ms: translate.last_ok_unix_ms.load(Relaxed),
            last_translate_fail_unix_ms: translate.last_fail_unix_ms.load(Relaxed),
            last_translate_fail_reason: translate
                .last_fail_reason
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            speech_queue_depth: queues.speech_depth(),
            speech_delivered: speech.delivered.load(Relaxed),
            speech_dropped: queues.speech_dropped(),
            speech_submitted: speech.submitted.load(Relaxed),
            speech_skipped: speech.skipped.load(Relaxed),
            speech_failed: speech.failed.load(Relaxed),
            tts_connected: speech.tts_connected.load(Relaxed),
            subtitles_published: self.subtitles_published.load(Relaxed),
            records_in_memory: self.records_in_memory.load(Relaxed),
            subtitles_path: self.subtitles_path.display().to_string(),
            last_write_unix_ms: self.last_write_unix_ms.load(Relaxed),
            degraded: self.degraded_strings(),
            version: VERSION.to_string(),
        }
    }
}

/// 待翻译句：final 原文，翻译线程消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateJob {
    pub utterance_id: String,
    pub text: String,
}

/// 待播报句：合格译文，投递线程消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechJob {
    pub utterance_id: String,
    pub text: String,
}

/// 有界内存队列（SPEC-05）：容量来自配置。
/// 待翻译队满拒收，由字幕线程记录降级；待播报队满丢最旧并计数（播报投递条款）。
/// 接收端放锁里：工作线程取件与 speech stop 清空共用；
/// 深度用原子计数（入队 +1、取件 -1），控制线程只读，不跨阻塞持有锁。
pub struct Queues {
    translate_tx: SyncSender<TranslateJob>,
    translate_rx: Mutex<Receiver<TranslateJob>>,
    translate_depth: AtomicU32,
    speech_tx: SyncSender<SpeechJob>,
    speech_rx: Mutex<Receiver<SpeechJob>>,
    speech_depth: AtomicU32,
    speech_dropped: AtomicU64,
}

impl Queues {
    /// 容量已由配置校验保证 ≥ 1。
    pub fn new(translate_capacity: usize, speech_capacity: usize) -> Self {
        let (translate_tx, translate_rx) = mpsc::sync_channel(translate_capacity);
        let (speech_tx, speech_rx) = mpsc::sync_channel(speech_capacity);
        Self {
            translate_tx,
            translate_rx: Mutex::new(translate_rx),
            translate_depth: AtomicU32::new(0),
            speech_tx,
            speech_rx: Mutex::new(speech_rx),
            speech_depth: AtomicU32::new(0),
            speech_dropped: AtomicU64::new(0),
        }
    }

    /// final 入待翻译队列；队满返回 false，调用方记录降级。
    pub fn push_translate(&self, job: TranslateJob) -> bool {
        if self.translate_tx.try_send(job).is_err() {
            return false;
        }
        self.translate_depth.fetch_add(1, Relaxed);
        true
    }

    /// 翻译线程取件；超时返回 Timeout，深度随取件递减。
    pub fn recv_translate(&self, timeout: Duration) -> Result<TranslateJob, RecvTimeoutError> {
        let rx = self
            .translate_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let job = rx.recv_timeout(timeout)?;
        self.translate_depth.fetch_sub(1, Relaxed);
        Ok(job)
    }

    /// 译文入待播报队列；队满丢最旧并计数（SPEC-05），返回 false 仅当投递线程已退出。
    pub fn push_speech(&self, job: SpeechJob) -> bool {
        match self.speech_tx.try_send(job) {
            Ok(()) => {
                self.speech_depth.fetch_add(1, Relaxed);
                true
            }
            Err(TrySendError::Full(job)) => {
                let rx = self
                    .speech_rx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // 丢最旧腾位；取件竞态下可能已被消费，此时队列已有空位。
                let dropped = rx.try_recv().is_ok();
                if dropped {
                    self.speech_dropped.fetch_add(1, Relaxed);
                }
                if self.speech_tx.try_send(job).is_ok() {
                    if !dropped {
                        self.speech_depth.fetch_add(1, Relaxed);
                    }
                    true
                } else {
                    if dropped {
                        self.speech_depth.fetch_sub(1, Relaxed);
                    }
                    false
                }
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// 投递线程取件；超时返回 Timeout，深度随取件递减。
    pub fn recv_speech(&self, timeout: Duration) -> Result<SpeechJob, RecvTimeoutError> {
        let rx = self
            .speech_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let job = rx.recv_timeout(timeout)?;
        self.speech_depth.fetch_sub(1, Relaxed);
        Ok(job)
    }

    pub fn translate_depth(&self) -> u32 {
        self.translate_depth.load(Relaxed)
    }

    pub fn speech_depth(&self) -> u32 {
        self.speech_depth.load(Relaxed)
    }

    /// 待播报队满丢弃累计（不含 speech stop 清空）。
    pub fn speech_dropped(&self) -> u64 {
        self.speech_dropped.load(Relaxed)
    }

    /// speech stop：清空待播报队列，返回丢弃数。
    pub fn clear_speech(&self) -> u32 {
        let rx = self
            .speech_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut cleared = 0;
        while rx.try_recv().is_ok() {
            cleared += 1;
        }
        self.speech_depth.fetch_sub(cleared, Relaxed);
        cleared
    }

    /// SIGTERM 收尾：丢弃在途队列（SPEC-05 桥收尾）。
    pub fn discard(&self) {
        {
            let rx = self
                .translate_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut cleared = 0;
            while rx.try_recv().is_ok() {
                cleared += 1;
            }
            self.translate_depth.fetch_sub(cleared, Relaxed);
        }
        self.clear_speech();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mozart-bridge-state-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_path_derives_from_subtitles_parent_not_run_dir() {
        // 持久目录（字幕同目录），而非 run_dir（tmpfs）。
        let subtitles = Path::new("/home/u/.local/state/mozart/subtitles.jsonl");
        assert_eq!(
            state_path_for(subtitles),
            Path::new("/home/u/.local/state/mozart/bridge-state.json")
        );
        // 与 run_dir 无关：即使 run_dir 不同，状态文件仍随字幕目录。
        assert_ne!(
            state_path_for(subtitles),
            Path::new("/run/mozart/bridge-state.json")
        );
        // 裸文件名（无目录）回落当前目录。
        assert_eq!(
            state_path_for(Path::new("subtitles.jsonl")),
            Path::new("bridge-state.json")
        );
    }

    #[test]
    fn switches_roundtrip_through_state_file() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(STATE_FILE_NAME);
        let baseline = Switches {
            translate_enabled: true,
            speech_enabled: true,
        };
        // 缺失文件回落基线。
        assert_eq!(load_switches(&path, baseline).unwrap(), baseline);

        let overrides = Switches {
            translate_enabled: false,
            speech_enabled: true,
        };
        save_switches(&path, &overrides).unwrap();
        assert_eq!(load_switches(&path, baseline).unwrap(), overrides);
        // 原子写不留临时文件。
        assert!(!path.with_extension("json.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_file_is_rejected() {
        let dir = temp_dir("corrupt");
        let path = dir.join(STATE_FILE_NAME);
        let baseline = Switches {
            translate_enabled: true,
            speech_enabled: false,
        };
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_switches(&path, baseline).is_err());
        std::fs::write(&path, r#"{"translate_enabled":false}"#).unwrap();
        assert!(load_switches(&path, baseline).is_err());
        std::fs::write(&path, r#"{"translate_enabled":false,"speech_enabled":true,"x":1}"#)
            .unwrap();
        assert!(load_switches(&path, baseline).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_switches_persists_and_rejects_on_write_failure() {
        let dir = temp_dir("set");
        let path = dir.join(STATE_FILE_NAME);
        let shared = Shared::new(
            Switches {
                translate_enabled: true,
                speech_enabled: true,
            },
            dir.join("subtitles.jsonl"),
            path.clone(),
        );
        shared
            .set_switches(Switches {
                translate_enabled: false,
                speech_enabled: false,
            })
            .unwrap();
        assert!(!shared.translate_enabled());
        assert!(!shared.speech_enabled());
        assert_eq!(
            load_switches(&path, Switches { translate_enabled: true, speech_enabled: true })
                .unwrap(),
            Switches { translate_enabled: false, speech_enabled: false }
        );

        // 状态文件路径指向目录：写盘失败，内存开关不变，降级可查。
        let bad = Shared::new(
            Switches {
                translate_enabled: true,
                speech_enabled: true,
            },
            dir.join("subtitles.jsonl"),
            dir.clone(),
        );
        assert!(bad
            .set_switches(Switches {
                translate_enabled: false,
                speech_enabled: false,
            })
            .is_err());
        assert!(bad.translate_enabled());
        assert!(bad.speech_enabled());
        assert!(bad
            .status(&Queues::new(1, 1))
            .degraded
            .iter()
            .any(|d| d.starts_with("state file write failed")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn degraded_dedupes_and_resolves() {
        let dir = temp_dir("degraded");
        let shared = Shared::new(
            Switches { translate_enabled: true, speech_enabled: true },
            dir.join("subtitles.jsonl"),
            dir.join(STATE_FILE_NAME),
        );
        shared.degrade("asr events disconnected");
        shared.degrade("asr events disconnected");
        let status = shared.status(&Queues::new(1, 1));
        assert_eq!(status.degraded.len(), 1);
        assert!(status.degraded[0].starts_with("asr events disconnected (unix_ms="));
        shared.resolve("asr events disconnected");
        assert!(shared.status(&Queues::new(1, 1)).degraded.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queues_push_depth_and_clear() {
        let queues = Queues::new(2, 2);
        assert_eq!(queues.translate_depth(), 0);
        assert_eq!(queues.speech_depth(), 0);

        assert!(queues.push_translate(TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
        }));
        assert!(queues.push_translate(TranslateJob {
            utterance_id: "s:2".to_string(),
            text: "世界".to_string(),
        }));
        // 待翻译队满拒收。
        assert!(!queues.push_translate(TranslateJob {
            utterance_id: "s:3".to_string(),
            text: "满".to_string(),
        }));
        assert_eq!(queues.translate_depth(), 2);

        assert!(queues.push_speech(SpeechJob {
            utterance_id: "s:1".to_string(),
            text: "hello".to_string(),
        }));
        assert_eq!(queues.speech_depth(), 1);
        assert_eq!(queues.clear_speech(), 1);
        assert_eq!(queues.speech_depth(), 0);
        assert_eq!(queues.clear_speech(), 0);
        // speech stop 清空不计队满丢弃。
        assert_eq!(queues.speech_dropped(), 0);

        queues.discard();
        assert_eq!(queues.translate_depth(), 0);
        assert_eq!(queues.speech_depth(), 0);
    }

    #[test]
    fn speech_queue_full_drops_oldest_and_counts() {
        let queues = Queues::new(1, 2);
        for seq in 1..=2 {
            assert!(queues.push_speech(SpeechJob {
                utterance_id: format!("s:{seq}"),
                text: format!("t{seq}"),
            }));
        }
        assert_eq!(queues.speech_depth(), 2);
        // 队满丢最旧：s:1 被丢，s:3 入队。
        assert!(queues.push_speech(SpeechJob {
            utterance_id: "s:3".to_string(),
            text: "t3".to_string(),
        }));
        assert_eq!(queues.speech_depth(), 2);
        assert_eq!(queues.speech_dropped(), 1);

        let first = queues.recv_speech(Duration::from_millis(100)).unwrap();
        assert_eq!(first.utterance_id, "s:2");
        let second = queues.recv_speech(Duration::from_millis(100)).unwrap();
        assert_eq!(second.utterance_id, "s:3");
        assert_eq!(queues.speech_depth(), 0);
        assert!(queues
            .recv_speech(Duration::from_millis(10))
            .is_err_and(|err| err == RecvTimeoutError::Timeout));
    }

    #[test]
    fn translate_queue_recv_decrements_depth() {
        let queues = Queues::new(2, 1);
        assert!(queues.push_translate(TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
        }));
        assert_eq!(queues.translate_depth(), 1);
        let job = queues.recv_translate(Duration::from_millis(100)).unwrap();
        assert_eq!(job.text, "你好");
        assert_eq!(queues.translate_depth(), 0);
        assert!(queues
            .recv_translate(Duration::from_millis(10))
            .is_err_and(|err| err == RecvTimeoutError::Timeout));
    }

    #[test]
    fn status_reports_translate_and_speech_stats() {
        let dir = temp_dir("stats");
        let shared = Shared::new(
            Switches { translate_enabled: true, speech_enabled: true },
            dir.join("subtitles.jsonl"),
            dir.join(STATE_FILE_NAME),
        );
        let queues = Queues::new(4, 4);
        let translate = shared.translate_stats();
        translate.attempt_fail("connect refused");
        translate.add_retry();
        translate.attempt_ok();
        translate.add_translated();
        translate.add_qc_rejected();
        translate.attempt_fail("HTTP 500");
        translate.add_failed();
        let speech = shared.speech_stats();
        speech.set_connected(true);
        speech.add_delivered();
        speech.add_submitted();
        speech.add_delivered();
        speech.add_skipped();
        speech.add_delivered();
        speech.add_failed();
        queues.push_speech(SpeechJob {
            utterance_id: "s:1".to_string(),
            text: "hello".to_string(),
        });
        queues.push_speech(SpeechJob {
            utterance_id: "s:2".to_string(),
            text: "world".to_string(),
        });
        queues.push_speech(SpeechJob {
            utterance_id: "s:3".to_string(),
            text: "extra".to_string(),
        });
        queues.push_speech(SpeechJob {
            utterance_id: "s:4".to_string(),
            text: "more".to_string(),
        });
        queues.push_speech(SpeechJob {
            utterance_id: "s:5".to_string(),
            text: "drop".to_string(),
        });

        let status = shared.status(&queues);
        assert_eq!(status.translated, 1);
        assert_eq!(status.translate_failed, 1);
        assert_eq!(status.translate_retried, 1);
        assert_eq!(status.qc_rejected, 1);
        // 最近一次请求失败：连接状态与失败原因取最后一次。
        assert!(!status.llama_connected);
        assert!(status.last_translate_ok_unix_ms > 0);
        assert!(status.last_translate_fail_unix_ms >= status.last_translate_ok_unix_ms);
        assert_eq!(status.last_translate_fail_reason.as_deref(), Some("HTTP 500"));
        assert_eq!(status.speech_delivered, 3);
        assert_eq!(status.speech_submitted, 1);
        assert_eq!(status.speech_skipped, 1);
        assert_eq!(status.speech_failed, 1);
        assert!(status.tts_connected);
        assert_eq!(status.speech_queue_depth, 4);
        assert_eq!(status.speech_dropped, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}