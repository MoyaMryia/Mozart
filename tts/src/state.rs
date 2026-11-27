//! TTS 共享状态：计数全部原子，控制线程只读快照（SPEC-05）。
//! `last_error` 用互斥锁保护，只在降级事件写入，不跨 I/O 持有。

use mozart_core::ctl::schema::TtsStatus;
use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{Relaxed, SeqCst},
};
use std::sync::Mutex;

/// 版本号，Hello 与 GetStatus 一致上报。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Shared {
    synthesized: AtomicU64,
    failed: AtomicU64,
    skipped: AtomicU64,
    stopped: AtomicU64,
    synthesizing: AtomicBool,
    /// 已受理且尚未结束播报/清算的在途句数（含正在合成、预合成槽、正在播放）。
    in_flight: AtomicU64,
    model_loaded: AtomicBool,
    queue_depth: AtomicU64,
    frames_out: AtomicU64,
    output_dropped: AtomicU64,
    /// TTS_JOB_STOP 安装的单调截止令牌。旧任务与在途合成据此中止。
    cutoff: AtomicU64,
    output_gate: Mutex<()>,
    last_error: Mutex<Option<String>>,
}

/// 在途合成的中止判定：进程停止，或任务令牌早于当前播报截止令牌。
/// 真实 FFI 的生成进度回调跨 C 边界持有它（栈对象，生成调用期间存活）。
pub struct Abort<'a> {
    stop: &'a AtomicBool,
    cutoff: &'a AtomicU64,
    pub issued_ns: u64,
}

impl Abort<'_> {
    pub fn aborted(&self) -> bool {
        self.stop.load(Relaxed) || self.issued_ns < self.cutoff.load(Relaxed)
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
            in_flight: AtomicU64::new(0),
            model_loaded: AtomicBool::new(false),
            queue_depth: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            output_dropped: AtomicU64::new(0),
            cutoff: AtomicU64::new(0),
            output_gate: Mutex::new(()),
            last_error: Mutex::new(None),
        }
    }

    pub fn add_synthesized(&self, count: u64) {
        self.synthesized.fetch_add(count, SeqCst);
    }
    pub fn add_failed(&self, count: u64) {
        self.failed.fetch_add(count, SeqCst);
    }
    pub fn add_skipped(&self, count: u64) {
        self.skipped.fetch_add(count, Relaxed);
    }
    pub fn add_stopped(&self, count: u64) {
        self.stopped.fetch_add(count, SeqCst);
    }
    pub fn cutoff(&self) -> u64 {
        self.cutoff.load(SeqCst)
    }

    fn output_gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.output_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 重放旧截止值不影响新任务。确认前等待当前非阻塞发送离开门控。
    pub fn install_fence(&self, cutoff: u64) -> u64 {
        let _gate = self.output_gate();
        self.cutoff.fetch_max(cutoff, SeqCst).max(cutoff)
    }

    pub fn send_if_current(&self, abort: &Abort<'_>, send: impl FnOnce()) {
        let _gate = self.output_gate();
        if !abort.aborted() {
            send();
        }
    }
    #[cfg(test)]
    pub fn stop_jobs(&self) { self.install_fence(mozart_core::speech::monotonic_ns()); }
    #[cfg(test)]
    pub fn abort<'a>(&'a self, stop: &'a AtomicBool) -> Abort<'a> {
        self.abort_job(stop, mozart_core::speech::monotonic_ns())
    }
    pub fn abort_job<'a>(&'a self, stop: &'a AtomicBool, issued_ns: u64) -> Abort<'a> {
        Abort { stop, cutoff: &self.cutoff, issued_ns }
    }
    pub fn set_synthesizing(&self, active: bool) {
        self.synthesizing.store(active, SeqCst);
    }
    pub fn enter_flight(&self) {
        self.in_flight.fetch_add(1, SeqCst);
    }
    pub fn leave_flight(&self) {
        self.in_flight.fetch_sub(1, SeqCst);
    }
    pub fn set_model_loaded(&self, loaded: bool) {
        self.model_loaded.store(loaded, Relaxed);
    }
    /// 入队计数：jobs 线程在 try_send 前 +1，失败回退 -1；
    /// 出队由 synth 线程 -1。成功入队必先于出队，读数不会下溢。
    pub fn queue_push(&self) {
        self.queue_depth.fetch_add(1, SeqCst);
    }
    pub fn queue_pop(&self) {
        self.queue_depth.fetch_sub(1, SeqCst);
    }
    pub fn add_frames_out(&self, count: u64) {
        self.frames_out.fetch_add(count, SeqCst);
    }
    /// 发布输出丢弃累计绝对值（输出线程从 FrameSocket 统计取）。
    pub fn set_output_dropped(&self, total: u64) {
        self.output_dropped.store(total, SeqCst);
    }

    /// 记录最近降级原因（AGENTS：降级可经状态接口查询）。只在降级路径调用。
    pub fn set_last_error(&self, message: &str) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message.to_string());
    }

    /// 组装 GetStatus 快照。先读完成计数再读活跃状态，保证观察到完成增量的同时能看到回落 idle。
    pub fn status(&self) -> TtsStatus {
        let synthesized = self.synthesized.load(SeqCst);
        let failed = self.failed.load(SeqCst);
        let skipped = self.skipped.load(SeqCst);
        let stopped = self.stopped.load(SeqCst);
        let queue_depth = self.queue_depth.load(SeqCst).min(u32::MAX as u64) as u32;
        let model_loaded = self.model_loaded.load(Relaxed);
        let frames_out = self.frames_out.load(SeqCst);
        let output_dropped = self.output_dropped.load(SeqCst);
        let last_error = self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let active = self.synthesizing.load(SeqCst) || self.in_flight.load(SeqCst) > 0;
        TtsStatus {
            state: if active { "synthesizing" } else { "idle" }.to_string(),
            synthesized,
            failed,
            skipped,
            stopped,
            queue_depth,
            model_loaded,
            frames_out,
            output_dropped,
            last_error,
            version: VERSION.to_string(),
        }
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}