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
    stopped: AtomicU64,
    synthesizing: AtomicBool,
    model_loaded: AtomicBool,
    queue_depth: AtomicU64,
    frames_out: AtomicU64,
    output_dropped: AtomicU64,
    /// 任务代际：TTS_JOB_STOP 推进；在途合成与帧投递据此中止（见 Abort）。
    job_epoch: AtomicU64,
    last_error: Mutex<Option<String>>,
}

/// 在途合成的中止判定：进程停止（SIGTERM）或任务代际推进（TTS_JOB_STOP）。
/// 创建时捕获任务代际；合成与帧投递期间代际变化即视为中止。
/// 真实 FFI 的生成进度回调跨 C 边界持有它（栈对象，生成调用期间存活）。
pub struct Abort<'a> {
    stop: &'a AtomicBool,
    epoch: &'a AtomicU64,
    start: u64,
}

impl Abort<'_> {
    pub fn aborted(&self) -> bool {
        self.stop.load(Relaxed) || self.epoch.load(Relaxed) != self.start
    }
}

impl Shared {
    pub fn new() -> Self {
        Self {
            synthesized: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            stopped: AtomicU64::new(0),
            synthesizing: AtomicBool::new(false),
            model_loaded: AtomicBool::new(false),
            queue_depth: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            output_dropped: AtomicU64::new(0),
            job_epoch: AtomicU64::new(0),
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
    pub fn add_stopped(&self, count: u64) {
        self.stopped.fetch_add(count, Relaxed);
    }
    /// 当前任务代际。合成线程在取任务后捕获，作为中止判定基准。
    pub fn job_epoch(&self) -> u64 {
        self.job_epoch.load(Relaxed)
    }
    /// TTS_JOB_STOP：推进任务代际，在途合成与帧投递据此中止；
    /// 在队任务由合成线程在下一轮循环清空（队列接收端归合成线程独占）。
    pub fn stop_jobs(&self) {
        self.job_epoch.fetch_add(1, Relaxed);
    }
    /// 捕获当前代际的中止判定（进程停止标志 + 任务代际）。
    pub fn abort<'a>(&'a self, stop: &'a AtomicBool) -> Abort<'a> {
        Abort {
            stop,
            epoch: &self.job_epoch,
            start: self.job_epoch.load(Relaxed),
        }
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
            stopped: self.stopped.load(Relaxed),
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