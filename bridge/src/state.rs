//! 桥共享状态（SPEC-05）：开关、字幕计数、降级原因，控制线程只读快照。
//! 开关以 TOML 为基线，运行时命令覆盖并原子写状态文件存档（SPEC-04 模型甲）；
//! 桥是开关的活权威，状态文件只是快照，启动恢复一次，运行时不对账。
//! 待翻译/待播报队列是有界内存队列，Task 5 消费；SIGTERM 退出时丢弃。

use mozart_core::ctl::schema::BridgeStatus;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 版本号，Hello 与 GetStatus 一致上报。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 运行时状态文件名（run_dir 下）：开关覆盖的存档快照。
pub const STATE_FILE_NAME: &str = "bridge-state.json";

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

pub struct Shared {
    pub subtitles_path: PathBuf,
    state_path: PathBuf,
    translate_enabled: AtomicBool,
    speech_enabled: AtomicBool,
    subtitles_published: AtomicU64,
    records_in_memory: AtomicU32,
    last_write_unix_ms: AtomicU64,
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
            degraded: Mutex::new(Vec::new()),
        }
    }

    pub fn translate_enabled(&self) -> bool {
        self.translate_enabled.load(Relaxed)
    }
    pub fn speech_enabled(&self) -> bool {
        self.speech_enabled.load(Relaxed)
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

    /// 组装 GetStatus 快照。翻译/播报计数字段 Task 5 填充，此处保持默认零值。
    pub fn status(&self, translate_queue_depth: u32, speech_queue_depth: u32) -> BridgeStatus {
        BridgeStatus {
            translate_enabled: self.translate_enabled(),
            speech_enabled: self.speech_enabled(),
            translate_queue_depth,
            speech_queue_depth,
            subtitles_published: self.subtitles_published.load(Relaxed),
            records_in_memory: self.records_in_memory.load(Relaxed),
            subtitles_path: self.subtitles_path.display().to_string(),
            last_write_unix_ms: self.last_write_unix_ms.load(Relaxed),
            degraded: self.degraded_strings(),
            version: VERSION.to_string(),
            ..Default::default()
        }
    }
}

/// 待翻译句（Task 5 消费）：final 原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateJob {
    pub utterance_id: String,
    pub text: String,
}

/// 待播报句（Task 5 消费）：合格译文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechJob {
    pub utterance_id: String,
    pub text: String,
}

/// 有界内存队列（SPEC-05）：容量来自配置，队满拒收由调用方记录降级。
/// 接收端放锁里：Task 5 工作线程取件与 speech stop 清空共用；
/// 深度用原子计数（入队 +1、取件 -1），控制线程只读，不跨阻塞持有锁。
pub struct Queues {
    translate_tx: SyncSender<TranslateJob>,
    translate_rx: Mutex<Receiver<TranslateJob>>,
    translate_depth: AtomicU32,
    speech_tx: SyncSender<SpeechJob>,
    speech_rx: Mutex<Receiver<SpeechJob>>,
    speech_depth: AtomicU32,
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

    /// Task 5 播报投递入口：译文入待播报队列；队满返回 false。
    #[allow(dead_code)] // Task 4 无投递方，Task 5 翻译线程使用。
    pub fn push_speech(&self, job: SpeechJob) -> bool {
        if self.speech_tx.try_send(job).is_err() {
            return false;
        }
        self.speech_depth.fetch_add(1, Relaxed);
        true
    }

    pub fn translate_depth(&self) -> u32 {
        self.translate_depth.load(Relaxed)
    }

    pub fn speech_depth(&self) -> u32 {
        self.speech_depth.load(Relaxed)
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
        assert!(bad.status(0, 0).degraded.iter().any(|d| d.starts_with("state file write failed")));

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
        let status = shared.status(0, 0);
        assert_eq!(status.degraded.len(), 1);
        assert!(status.degraded[0].starts_with("asr events disconnected (unix_ms="));
        shared.resolve("asr events disconnected");
        assert!(shared.status(0, 0).degraded.is_empty());
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
        // 队满拒收。
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

        queues.discard();
        assert_eq!(queues.translate_depth(), 0);
        assert_eq!(queues.speech_depth(), 0);
    }
}