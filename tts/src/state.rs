//! TTS 共享状态：计数全部原子，控制线程只读快照（SPEC-05）。
//! `last_error` 用互斥锁保护，只在降级事件写入，不跨 I/O 持有。

use mozart_core::ctl::schema::TtsStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

/// 版本号，Hello 与 GetStatus 一致上报。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Shared {
    synthesized: AtomicU64,
    failed: AtomicU64,
    skipped: AtomicU64,
    synthesizing: AtomicBool,
    model_loaded: AtomicBool,
    queue_depth: AtomicU64,
    frames_out: AtomicU64,
    output_dropped: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            synthesized: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            synthesizing: AtomicBool::new(false),
            model_loaded: AtomicBool::new(false),
            queue_depth: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            output_dropped: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    pub fn add_synthesized(&self, count: u64) {
        self.synthesized.fetch_add(count, Relaxed);
    }
    pub fn add_failed(&self, count: u64) {
        self.failed.fetch_add(count, Relaxed);
    }
    pub fn add_skipped(&self, count: u64) {
        self.skipped.fetch_add(count, Relaxed);
    }
    pub fn set_synthesizing(&self, active: bool) {
        self.synthesizing.store(active, Relaxed);
    }
    pub fn set_model_loaded(&self, loaded: bool) {
        self.model_loaded.store(loaded, Relaxed);
    }
    /// 入队计数：jobs 线程在 try_send 前 +1，失败回退 -1；
    /// 出队由 synth 线程 -1。成功入队必先于出队，读数不会下溢。
    pub fn queue_push(&self) {
        self.queue_depth.fetch_add(1, Relaxed);
    }
    pub fn queue_pop(&self) {
        self.queue_depth.fetch_sub(1, Relaxed);
    }
    pub fn add_frames_out(&self, count: u64) {
        self.frames_out.fetch_add(count, Relaxed);
    }
    /// 发布输出丢弃累计绝对值（synth 线程从 FrameSocket 统计取）。
    pub fn set_output_dropped(&self, total: u64) {
        self.output_dropped.store(total, Relaxed);
    }

    /// 记录最近降级原因（AGENTS：降级可经状态接口查询）。只在降级路径调用。
    pub fn set_last_error(&self, message: &str) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message.to_string());
    }

    /// 组装 GetStatus 快照。
    pub fn status(&self) -> TtsStatus {
        TtsStatus {
            state: if self.synthesizing.load(Relaxed) {
                "synthesizing"
            } else {
                "idle"
            }
            .to_string(),
            synthesized: self.synthesized.load(Relaxed),
            failed: self.failed.load(Relaxed),
            skipped: self.skipped.load(Relaxed),
            queue_depth: self.queue_depth.load(Relaxed).min(u32::MAX as u64) as u32,
            model_loaded: self.model_loaded.load(Relaxed),
            frames_out: self.frames_out.load(Relaxed),
            output_dropped: self.output_dropped.load(Relaxed),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            version: VERSION.to_string(),
        }
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}