//! 识别服务共享状态：计数全部原子，控制线程只读快照，不碰解码热路径（SPEC-05）。
//! `last_error` 用互斥锁保护，只在降级事件写入，不跨 I/O 持有。

use mozart_core::ctl::schema::AsrStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 版本号，Hello 与 GetStatus 一致上报。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 当前 Unix 毫秒，供最后收包时间戳。
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct Shared {
    /// 每次启动随机生成，三种事件与状态都携带。
    pub startup_id: String,
    frames_received: AtomicU64,
    frames_rejected: AtomicU64,
    event_buffer_dropped: AtomicU64,
    finals: AtomicU64,
    review_skipped: AtomicU64,
    review_failed: AtomicU64,
    last_packet_unix_ms: AtomicU64,
    bridge_connected: AtomicBool,
    decoding: AtomicBool,
    reviewing: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    pub fn new(startup_id: String) -> Self {
        Self {
            startup_id,
            frames_received: AtomicU64::new(0),
            frames_rejected: AtomicU64::new(0),
            event_buffer_dropped: AtomicU64::new(0),
            finals: AtomicU64::new(0),
            review_skipped: AtomicU64::new(0),
            review_failed: AtomicU64::new(0),
            last_packet_unix_ms: AtomicU64::new(0),
            bridge_connected: AtomicBool::new(false),
            decoding: AtomicBool::new(false),
            reviewing: AtomicBool::new(false),
            last_error: Mutex::new(None),
        }
    }

    pub fn add_received(&self, frames: u64) {
        self.frames_received.fetch_add(frames, Relaxed);
    }
    /// 发布坏包拒收累计绝对值（接收线程从 FrameSocket 统计取）。
    pub fn set_frames_rejected(&self, total: u64) {
        self.frames_rejected.store(total, Relaxed);
    }
    pub fn set_last_packet_unix_ms(&self, ms: u64) {
        self.last_packet_unix_ms.store(ms, Relaxed);
    }
    pub fn add_final(&self, count: u64) {
        self.finals.fetch_add(count, Relaxed);
    }
    pub fn add_review_skipped(&self, count: u64) {
        self.review_skipped.fetch_add(count, Relaxed);
    }
    pub fn add_review_failed(&self, count: u64) {
        self.review_failed.fetch_add(count, Relaxed);
    }
    /// 发布输出缓冲丢句累计绝对值（输出线程调用）。
    pub fn set_event_buffer_dropped(&self, total: u64) {
        self.event_buffer_dropped.store(total, Relaxed);
    }
    pub fn set_bridge_connected(&self, connected: bool) {
        self.bridge_connected.store(connected, Relaxed);
    }
    pub fn set_decoding(&self, active: bool) {
        self.decoding.store(active, Relaxed);
    }
    pub fn set_reviewing(&self, active: bool) {
        self.reviewing.store(active, Relaxed);
    }

    /// 记录最近降级原因（AGENTS：降级可经状态接口查询）。只在降级路径调用。
    pub fn set_last_error(&self, message: &str) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message.to_string());
    }

    /// 组装 GetStatus 快照。ring_dropped 由控制线程从环形队列读数传入。
    pub fn status(&self, ring_dropped: u64) -> AsrStatus {
        let state = if self.decoding.load(Relaxed) {
            "decoding"
        } else if self.reviewing.load(Relaxed) {
            "reviewing"
        } else {
            "idle"
        };
        AsrStatus {
            state: state.to_string(),
            startup_id: self.startup_id.clone(),
            frames_received: self.frames_received.load(Relaxed),
            frames_rejected: self.frames_rejected.load(Relaxed),
            ring_dropped,
            event_buffer_dropped: self.event_buffer_dropped.load(Relaxed),
            finals: self.finals.load(Relaxed),
            review_skipped: self.review_skipped.load(Relaxed),
            review_failed: self.review_failed.load(Relaxed),
            last_packet_unix_ms: self.last_packet_unix_ms.load(Relaxed),
            bridge_connected: self.bridge_connected.load(Relaxed),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            version: VERSION.to_string(),
        }
    }
}