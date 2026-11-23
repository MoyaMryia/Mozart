//! 桥共享状态（SPEC-05）：开关、字幕计数、翻译/播报计数、降级原因，控制线程只读快照。
//! 开关启动取 TOML 基线，运行时覆盖由 daemon 统一存 `state.json` 并在就绪/重启时
//! 下发（SPEC-04）；桥只持有内存活状态，不再自写状态文件。
//! 待翻译/待播报队列是有界内存队列：待播报队满丢最旧并计数（SPEC-05 播报投递），
//! 待翻译队满拒收由字幕线程记降级；SIGTERM 退出时丢弃。

use mozart_core::ctl::schema::BridgeStatus;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 版本号，Hello 与 GetStatus 一致上报。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 当前 Unix 毫秒，供字幕时间戳与降级时间。
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 开关快照：TOML 基线与运行时覆盖共用同一结构。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    pub translate_enabled: bool,
    pub speech_enabled: bool,
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

/// 在 final 时绑定的单调时钟许可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeechGeneration(u64);

impl SpeechGeneration {
    pub fn issued_ns(self) -> u64 {
        self.0
    }
}

struct SpeechGate {
    active: Option<SpeechGeneration>,
    cutoff: u64,
}

impl SpeechGate {
    fn revoke(&mut self) -> u64 {
        let after_active = self.active.take().map_or(1, |token| token.0 + 1);
        self.cutoff = mozart_core::speech::monotonic_ns().max(self.cutoff).max(after_active);
        self.cutoff
    }
}

pub struct Shared {
    pub subtitles_path: PathBuf,
    translate_enabled: AtomicBool,
    // 开关与一次非阻塞发送在同一把锁下串行。
    speech_generation: Mutex<SpeechGate>,
    subtitles_published: AtomicU64,
    records_in_memory: AtomicU32,
    events_dropped: AtomicU64,
    events_bad: AtomicU64,
    events_stale: AtomicU64,
    last_write_unix_ms: AtomicU64,
    translate_stats: TranslateStats,
    speech_stats: SpeechStats,
    degraded: Mutex<Vec<DegradedEntry>>,
}

impl Shared {
    pub fn new(switches: Switches, subtitles_path: PathBuf) -> Self {
        Self {
            subtitles_path,
            translate_enabled: AtomicBool::new(switches.translate_enabled),
            speech_generation: Mutex::new(SpeechGate {
                active: switches
                    .speech_enabled
                    .then(|| SpeechGeneration(mozart_core::speech::monotonic_ns().max(1))),
                cutoff: 0,
            }),
            subtitles_published: AtomicU64::new(0),
            records_in_memory: AtomicU32::new(0),
            events_dropped: AtomicU64::new(0),
            events_bad: AtomicU64::new(0),
            events_stale: AtomicU64::new(0),
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
        self.speech_generation().is_some()
    }

    fn speech_gate(&self) -> MutexGuard<'_, SpeechGate> {
        self.speech_generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn speech_generation(&self) -> Option<SpeechGeneration> {
        self.speech_gate().active
    }

    /// `send` 必须非阻塞。应答在释放门控之后等待。
    pub fn send_current_speech<T>(
        &self,
        generation: SpeechGeneration,
        send: impl FnOnce() -> T,
    ) -> Option<T> {
        let gate = self.speech_gate();
        (gate.active == Some(generation)).then(send)
    }

    pub fn translate_stats(&self) -> &TranslateStats {
        &self.translate_stats
    }
    pub fn speech_stats(&self) -> &SpeechStats {
        &self.speech_stats
    }

    /// 运行时开关只改桥内活状态；持久存档归 daemon `state.json`（SPEC-04）。
    pub fn set_switches(&self, switches: Switches) {
        self.translate_enabled
            .store(switches.translate_enabled, Relaxed);
        let mut gate = self.speech_gate();
        if switches.speech_enabled && gate.active.is_none() {
            gate.active = Some(SpeechGeneration(
                mozart_core::speech::monotonic_ns()
                    .max(gate.cutoff.checked_add(1).expect("speech cutoff overflow")),
            ));
        } else if !switches.speech_enabled && gate.active.is_some() {
            gate.revoke();
        }
    }

    pub fn stop_speech(&self) -> u64 {
        let mut gate = self.speech_gate();
        gate.revoke()
    }

    /// 已发布句数按 utterance_id 计，不按 JSONL 行数（沿用旧实现）。
    pub fn add_published(&self) {
        self.subtitles_published.fetch_add(1, Relaxed);
    }
    pub fn set_records_in_memory(&self, count: u32) {
        self.records_in_memory.store(count, Relaxed);
    }
    /// 事件→字幕通道满丢（partial）。
    pub fn add_event_dropped(&self) {
        self.events_dropped.fetch_add(1, Relaxed);
    }
    /// 坏事件（解析失败）丢弃。
    pub fn add_event_bad(&self) {
        self.events_bad.fetch_add(1, Relaxed);
    }
    /// 跨启动号事件丢弃。
    pub fn add_event_stale(&self) {
        self.events_stale.fetch_add(1, Relaxed);
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
            events_dropped: self.events_dropped.load(Relaxed),
            events_bad: self.events_bad.load(Relaxed),
            events_stale: self.events_stale.load(Relaxed),
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
    pub speech_generation: Option<SpeechGeneration>,
}

/// 待播报句：合格译文，投递线程消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechJob {
    pub utterance_id: String,
    pub text: String,
    pub generation: SpeechGeneration,
}

#[cfg(test)]
impl SpeechGeneration {
    pub const FIRST: Self = Self(1);
}

/// 有界内存队列（SPEC-05）：容量来自配置。
/// 待翻译队满拒收，由字幕线程记录降级；待播报队满丢最旧并计数（播报投递条款）。
/// 播报取件在条件变量上等待，不占用停止命令所需的队列锁。
pub struct Queues {
    translate_tx: SyncSender<TranslateJob>,
    translate_rx: Mutex<Receiver<TranslateJob>>,
    translate_depth: AtomicU32,
    speech: Mutex<VecDeque<SpeechJob>>,
    speech_ready: Condvar,
    speech_capacity: usize,
    speech_dropped: AtomicU64,
}

impl Queues {
    /// 容量已由配置校验保证 ≥ 1。
    pub fn new(translate_capacity: usize, speech_capacity: usize) -> Self {
        let (translate_tx, translate_rx) = mpsc::sync_channel(translate_capacity);
        Self {
            translate_tx,
            translate_rx: Mutex::new(translate_rx),
            translate_depth: AtomicU32::new(0),
            speech: Mutex::new(VecDeque::new()),
            speech_ready: Condvar::new(),
            speech_capacity,
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

    fn speech_queue(&self) -> MutexGuard<'_, VecDeque<SpeechJob>> {
        self.speech
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 译文入待播报队列；队满丢最旧并计数（SPEC-05）。
    pub fn push_speech(&self, job: SpeechJob) {
        let mut queue = self.speech_queue();
        if queue.len() == self.speech_capacity {
            queue.pop_front();
            self.speech_dropped.fetch_add(1, Relaxed);
        }
        queue.push_back(job);
        drop(queue);
        self.speech_ready.notify_one();
    }

    /// 投递线程取件；空队列等待期间释放队列锁。
    pub fn recv_speech(&self, timeout: Duration) -> Option<SpeechJob> {
        let (mut queue, _) = self
            .speech_ready
            .wait_timeout_while(self.speech_queue(), timeout, |queue| queue.is_empty())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.pop_front()
    }

    pub fn translate_depth(&self) -> u32 {
        self.translate_depth.load(Relaxed)
    }

    pub fn speech_depth(&self) -> u32 {
        self.speech_queue().len() as u32
    }

    /// 待播报队满丢弃累计（不含 speech stop 清空）。
    pub fn speech_dropped(&self) -> u64 {
        self.speech_dropped.load(Relaxed)
    }

    /// speech stop：清空待播报队列，返回丢弃数。
    pub fn clear_speech(&self) -> u32 {
        let mut queue = self.speech_queue();
        let cleared = queue.len() as u32;
        queue.clear();
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
    fn set_switches_updates_live_state() {
        let dir = temp_dir("set");
        let shared = Shared::new(
            Switches {
                translate_enabled: true,
                speech_enabled: true,
            },
            dir.join("subtitles.jsonl"),
        );
        shared.set_switches(Switches {
            translate_enabled: false,
            speech_enabled: false,
        });
        assert!(!shared.translate_enabled());
        assert!(!shared.speech_enabled());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn speech_fence_orders_reopen_tokens() {
        let switches = |speech_enabled| Switches { translate_enabled: true, speech_enabled };
        let shared = Shared::new(switches(true), PathBuf::new());
        let first = shared.speech_generation().unwrap();
        let cutoff = shared.stop_speech();
        assert!(cutoff >= first.issued_ns());
        assert!(shared.send_current_speech(first, || ()).is_none());
        let next_cutoff = shared.stop_speech();
        assert!(next_cutoff >= cutoff);
        shared.set_switches(switches(true));
        assert!(shared.speech_generation().unwrap().issued_ns() > next_cutoff);
    }

    #[test]
    fn speech_generation_orders_submission_and_revocation() {
        let dir = temp_dir("speech-gate");
        let switches = |speech_enabled| Switches {
            translate_enabled: true,
            speech_enabled,
        };
        let shared = Shared::new(switches(true), dir.join("subtitles.jsonl"));
        let initial = shared.speech_generation().unwrap();

        // The gate must span the side effect itself, not just the check preceding it.
        assert_eq!(
            shared.send_current_speech(initial, || {
                assert!(matches!(
                    shared.speech_generation.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                "sent"
            }),
            Some("sent")
        );
        shared.set_switches(switches(false));
        assert!(shared
            .send_current_speech(initial, || panic!("send after revocation"))
            .is_none());
        shared.set_switches(switches(true));
        let current = shared.speech_generation().unwrap();
        assert_ne!(initial, current);
        shared.set_switches(switches(true));
        assert_eq!(shared.speech_generation(), Some(current));
        assert!(shared
            .send_current_speech(initial, || panic!("old send after reopening"))
            .is_none());
        assert_eq!(shared.send_current_speech(current, || "sent"), Some("sent"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn degraded_dedupes_and_resolves() {
        let dir = temp_dir("degraded");
        let shared = Shared::new(
            Switches {
                translate_enabled: true,
                speech_enabled: true,
            },
            dir.join("subtitles.jsonl"),
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
            speech_generation: None,
        }));
        assert!(queues.push_translate(TranslateJob {
            utterance_id: "s:2".to_string(),
            text: "世界".to_string(),
            speech_generation: None,
        }));
        // 待翻译队满拒收。
        assert!(!queues.push_translate(TranslateJob {
            utterance_id: "s:3".to_string(),
            text: "满".to_string(),
            speech_generation: None,
        }));
        assert_eq!(queues.translate_depth(), 2);

        queues.push_speech(SpeechJob {
            utterance_id: "s:1".to_string(),
            text: "hello".to_string(),
            generation: SpeechGeneration::FIRST,
        });
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
            queues.push_speech(SpeechJob {
                utterance_id: format!("s:{seq}"),
                text: format!("t{seq}"),
                generation: SpeechGeneration::FIRST,
            });
        }
        assert_eq!(queues.speech_depth(), 2);
        // 队满丢最旧：s:1 被丢，s:3 入队。
        queues.push_speech(SpeechJob {
            utterance_id: "s:3".to_string(),
            text: "t3".to_string(),
            generation: SpeechGeneration::FIRST,
        });
        assert_eq!(queues.speech_depth(), 2);
        assert_eq!(queues.speech_dropped(), 1);

        let first = queues.recv_speech(Duration::from_millis(100)).unwrap();
        assert_eq!(first.utterance_id, "s:2");
        let second = queues.recv_speech(Duration::from_millis(100)).unwrap();
        assert_eq!(second.utterance_id, "s:3");
        assert_eq!(queues.speech_depth(), 0);
        assert!(queues.recv_speech(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn translate_queue_recv_decrements_depth() {
        let queues = Queues::new(2, 1);
        assert!(queues.push_translate(TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
            speech_generation: None,
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
            Switches {
                translate_enabled: true,
                speech_enabled: true,
            },
            dir.join("subtitles.jsonl"),
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
            generation: SpeechGeneration::FIRST,
        });
        queues.push_speech(SpeechJob {
            utterance_id: "s:2".to_string(),
            text: "world".to_string(),
            generation: SpeechGeneration::FIRST,
        });
        queues.push_speech(SpeechJob {
            utterance_id: "s:3".to_string(),
            text: "extra".to_string(),
            generation: SpeechGeneration::FIRST,
        });
        queues.push_speech(SpeechJob {
            utterance_id: "s:4".to_string(),
            text: "more".to_string(),
            generation: SpeechGeneration::FIRST,
        });
        queues.push_speech(SpeechJob {
            utterance_id: "s:5".to_string(),
            text: "drop".to_string(),
            generation: SpeechGeneration::FIRST,
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
