//! 引擎共享状态：命令目标与计数全部走原子，控制线程写、音频线程只读；
//! `last_error` 用互斥锁保护，只在降级事件写入，不跨任何 I/O 持有。

use mozart_core::ctl::schema::{PreStatus, Route};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

/// 版本号，Hello 与 GetStatus 一致上报。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn route_code(route: Route) -> u8 {
    match route {
        Route::Direct => 0,
        Route::Rvc => 1,
    }
}

fn route_from_code(code: u8) -> Route {
    match code {
        0 => Route::Direct,
        1 => Route::Rvc,
        // 写入只经 route_code，编码封闭。
        _ => unreachable!("route encoding is closed over 0..=1"),
    }
}

/// 状态快照需要的队列读数；控制线程组装 status 时传入。
pub struct QueueReadout {
    /// 采集 → DSP 环深度（帧）。
    pub depth: u64,
    /// 采集环与主干环满丢最旧的合计（帧）。
    pub dropped: u64,
}

pub struct Shared {
    route_target: AtomicU8,
    mute_target: AtomicBool,
    frames_captured: AtomicU64,
    frames_played: AtomicU64,
    last_error: Mutex<Option<String>>,
    capture_device: String,
    playback_device: String,
}

impl Shared {
    pub fn new(capture_device: String, playback_device: String) -> Self {
        Self {
            route_target: AtomicU8::new(route_code(Route::Direct)),
            mute_target: AtomicBool::new(false),
            frames_captured: AtomicU64::new(0),
            frames_played: AtomicU64::new(0),
            last_error: Mutex::new(None),
            capture_device,
            playback_device,
        }
    }

    /// 命令目标路由；播放线程在帧边界读入状态机。
    pub fn route_target(&self) -> Route {
        route_from_code(self.route_target.load(Ordering::Relaxed))
    }

    pub fn set_route_target(&self, route: Route) {
        self.route_target.store(route_code(route), Ordering::Relaxed);
    }

    pub fn mute_target(&self) -> bool {
        self.mute_target.load(Ordering::Relaxed)
    }

    pub fn set_mute_target(&self, mute: bool) {
        self.mute_target.store(mute, Ordering::Relaxed);
    }

    pub fn add_captured(&self, frames: u64) {
        self.frames_captured.fetch_add(frames, Ordering::Relaxed);
    }

    pub fn add_played(&self, frames: u64) {
        self.frames_played.fetch_add(frames, Ordering::Relaxed);
    }

    /// 记录最近降级原因（AGENTS：降级可经状态接口查询）。
    /// 只在降级路径调用：错误分支允许分配，稳态热路径不触碰。
    pub fn set_last_error(&self, message: &str) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message.to_string());
    }

    /// 组装 GetStatus 快照：只读原子与队列读数，不阻塞音频线程。
    pub fn status(&self, queue: QueueReadout) -> PreStatus {
        PreStatus {
            state: "running".to_string(),
            // 路由与静音上报命令目标；淡变在 20 ms 内完成，属执行细节。
            route: self.route_target(),
            mute: self.mute_target(),
            capture_device: self.capture_device.clone(),
            playback_device: self.playback_device.clone(),
            frames_captured: self.frames_captured.load(Ordering::Relaxed),
            frames_played: self.frames_played.load(Ordering::Relaxed),
            frames_dropped: queue.dropped,
            queue_depth: queue.depth,
            // 消费者分发与丢弃计数由 Task 4 接入数据面后填充。
            consumer_drops: BTreeMap::new(),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            version: VERSION.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_code_roundtrip_is_closed() {
        for route in [Route::Direct, Route::Rvc] {
            assert_eq!(route_from_code(route_code(route)), route);
        }
    }

    #[test]
    fn status_reports_targets_and_counters() {
        let shared = Shared::new("cap".to_string(), "play".to_string());
        shared.set_route_target(Route::Rvc);
        shared.set_mute_target(true);
        shared.add_captured(3);
        shared.add_played(2);
        shared.set_last_error("boom");
        let status = shared.status(QueueReadout {
            depth: 5,
            dropped: 7,
        });
        assert_eq!(status.route, Route::Rvc);
        assert!(status.mute);
        assert_eq!(status.state, "running");
        assert_eq!(status.capture_device, "cap");
        assert_eq!(status.playback_device, "play");
        assert_eq!(status.frames_captured, 3);
        assert_eq!(status.frames_played, 2);
        assert_eq!(status.frames_dropped, 7);
        assert_eq!(status.queue_depth, 5);
        assert_eq!(status.consumer_drops, BTreeMap::new());
        assert_eq!(status.last_error.as_deref(), Some("boom"));
        assert_eq!(status.version, VERSION);
    }
}
