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

/// 扬声器分时状态（SPEC-05 播报投递）：实时与播报共用扬声器，播报优先，
/// 播报空窗 holdoff 后恢复实时。播放线程写、控制线程只读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Realtime,
    Broadcast,
}

fn speaker_code(speaker: Speaker) -> u8 {
    match speaker {
        Speaker::Realtime => 0,
        Speaker::Broadcast => 1,
    }
}

fn speaker_from_code(code: u8) -> Speaker {
    match code {
        0 => Speaker::Realtime,
        1 => Speaker::Broadcast,
        // 写入只经 speaker_code，编码封闭。
        _ => unreachable!("speaker encoding is closed over 0..=1"),
    }
}

fn speaker_name(speaker: Speaker) -> &'static str {
    match speaker {
        Speaker::Realtime => "realtime",
        Speaker::Broadcast => "broadcast",
    }
}

/// 状态快照需要的队列读数；控制线程组装 status 时传入。
pub struct QueueReadout {
    /// 采集 → DSP 环深度（帧）。
    pub depth: u64,
    /// 采集环与主干环满丢最旧的合计（帧）。
    pub dropped: u64,
    /// AI 接收环满丢最旧（帧）。
    pub rvc_dropped: u64,
    /// 播报接收环满丢最旧（帧）。
    pub tts_dropped: u64,
}

pub struct Shared {
    route_target: AtomicU8,
    mute_target: AtomicBool,
    frames_captured: AtomicU64,
    frames_played: AtomicU64,
    /// AI 输入帧发送丢弃（含未建链期），处理线程发布绝对值。
    ai_send_dropped: AtomicU64,
    /// asr 输入帧发送丢弃（含未建链期），处理线程发布绝对值。
    asr_send_dropped: AtomicU64,
    /// AI 输出帧坏包丢弃，接收线程发布绝对值。
    ai_recv_dropped: AtomicU64,
    /// TTS 播报帧坏包丢弃，接收线程发布绝对值。
    tts_recv_dropped: AtomicU64,
    /// direct 路由期间丢弃的 rvc 输出帧（旧 skipped_replies），播放线程累加。
    ai_skipped: AtomicU64,
    /// 扬声器分时状态，播放线程在帧边界写、控制线程只读。
    speaker_state: AtomicU8,
    /// 播报帧播放累计，播放线程累加。
    tts_frames_played: AtomicU64,
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
            ai_send_dropped: AtomicU64::new(0),
            asr_send_dropped: AtomicU64::new(0),
            ai_recv_dropped: AtomicU64::new(0),
            tts_recv_dropped: AtomicU64::new(0),
            ai_skipped: AtomicU64::new(0),
            speaker_state: AtomicU8::new(speaker_code(Speaker::Realtime)),
            tts_frames_played: AtomicU64::new(0),
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

    /// 发布 AI 输入帧发送丢弃的累计绝对值（发送线程调用）。
    pub fn set_ai_send_dropped(&self, total: u64) {
        self.ai_send_dropped.store(total, Ordering::Relaxed);
    }

    /// 发布 asr 输入帧发送丢弃的累计绝对值（发送线程调用）。
    pub fn set_asr_send_dropped(&self, total: u64) {
        self.asr_send_dropped.store(total, Ordering::Relaxed);
    }

    /// 发布 AI 输出帧坏包丢弃的累计绝对值（接收线程调用）。
    pub fn set_ai_recv_dropped(&self, total: u64) {
        self.ai_recv_dropped.store(total, Ordering::Relaxed);
    }

    /// 发布 TTS 播报帧坏包丢弃的累计绝对值（接收线程调用）。
    pub fn set_tts_recv_dropped(&self, total: u64) {
        self.tts_recv_dropped.store(total, Ordering::Relaxed);
    }

    /// direct 路由期间丢弃的 rvc 输出帧计数（播放线程调用）。
    pub fn add_ai_skipped(&self, frames: u64) {
        self.ai_skipped.fetch_add(frames, Ordering::Relaxed);
    }

    /// 扬声器分时状态；播放线程在帧边界写，控制线程只读。
    pub fn speaker_state(&self) -> Speaker {
        speaker_from_code(self.speaker_state.load(Ordering::Relaxed))
    }

    pub fn set_speaker_state(&self, speaker: Speaker) {
        self.speaker_state
            .store(speaker_code(speaker), Ordering::Relaxed);
    }

    /// 播报帧播放计数（播放线程调用）。
    pub fn add_tts_frames_played(&self, frames: u64) {
        self.tts_frames_played.fetch_add(frames, Ordering::Relaxed);
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
        // consumer_drops 口径（SPEC-02 / SPEC-05）：
        // - "rvc"：AI 链路异常丢弃 = 输入发送丢弃 + 输出坏包丢弃 + 接收环满；
        // - "rvc.skipped"：direct 路由期间丢弃的输出帧（旧 skipped_replies，
        //   设计内丢弃，恒发链路在 direct 下必然增长）；
        // - "asr"：识别支路输入发送丢弃（含未建链期，asr 未起时增长属正常）；
        // - "tts"：播报链路异常丢弃 = 播报帧坏包丢弃 + 播报接收环满。
        let mut consumer_drops = BTreeMap::new();
        consumer_drops.insert(
            "rvc".to_string(),
            self.ai_send_dropped.load(Ordering::Relaxed)
                + self.ai_recv_dropped.load(Ordering::Relaxed)
                + queue.rvc_dropped,
        );
        consumer_drops.insert(
            "rvc.skipped".to_string(),
            self.ai_skipped.load(Ordering::Relaxed),
        );
        consumer_drops.insert(
            "asr".to_string(),
            self.asr_send_dropped.load(Ordering::Relaxed),
        );
        consumer_drops.insert(
            "tts".to_string(),
            self.tts_recv_dropped.load(Ordering::Relaxed) + queue.tts_dropped,
        );
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
            consumer_drops,
            speaker_state: speaker_name(self.speaker_state()).to_string(),
            tts_frames_played: self.tts_frames_played.load(Ordering::Relaxed),
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
    fn speaker_code_roundtrip_is_closed() {
        for speaker in [Speaker::Realtime, Speaker::Broadcast] {
            assert_eq!(speaker_from_code(speaker_code(speaker)), speaker);
        }
    }

    #[test]
    fn status_reports_speaker_state_and_tts_frames() {
        let shared = Shared::new("cap".to_string(), "play".to_string());
        // 默认实时、零播报帧。
        let status = shared.status(QueueReadout { depth: 0, dropped: 0, rvc_dropped: 0, tts_dropped: 0 });
        assert_eq!(status.speaker_state, "realtime");
        assert_eq!(status.tts_frames_played, 0);

        shared.set_speaker_state(Speaker::Broadcast);
        shared.add_tts_frames_played(5);
        let status = shared.status(QueueReadout { depth: 0, dropped: 0, rvc_dropped: 0, tts_dropped: 0 });
        assert_eq!(status.speaker_state, "broadcast");
        assert_eq!(status.tts_frames_played, 5);
        assert_eq!(shared.speaker_state(), Speaker::Broadcast);
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
            rvc_dropped: 11,
            tts_dropped: 0,
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
        assert_eq!(status.last_error.as_deref(), Some("boom"));
        assert_eq!(status.version, VERSION);
    }

    #[test]
    fn consumer_drops_aggregate_rvc_path_and_skipped_replies() {
        // 口径（SPEC-02 / SPEC-05）："rvc" = 发送丢弃 + 坏包丢弃 + 接收环满（异常）；
        // "rvc.skipped" = direct 路由丢弃的输出帧（设计内，旧 skipped_replies）；
        // "asr" = 识别支路输入发送丢弃；"tts" = 播报坏包丢弃 + 播报接收环满。
        let shared = Shared::new("cap".to_string(), "play".to_string());
        shared.set_ai_send_dropped(2);
        shared.set_ai_recv_dropped(3);
        shared.add_ai_skipped(4);
        shared.set_asr_send_dropped(6);
        shared.set_tts_recv_dropped(7);
        let status = shared.status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 5,
            tts_dropped: 8,
        });
        assert_eq!(status.consumer_drops["rvc"], 10, "2+3+5 三源合计");
        assert_eq!(status.consumer_drops["rvc.skipped"], 4);
        assert_eq!(status.consumer_drops["asr"], 6);
        assert_eq!(status.consumer_drops["tts"], 15, "7+8 坏包与环满合计");

        // 默认零值：四个键恒存在，daemon 可稳定解析。
        let status = Shared::new("c".to_string(), "p".to_string()).status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
        });
        assert_eq!(
            status.consumer_drops,
            BTreeMap::from([
                ("rvc".to_string(), 0),
                ("rvc.skipped".to_string(), 0),
                ("asr".to_string(), 0),
                ("tts".to_string(), 0),
            ])
        );
    }
}
