//! 播放线程：路由状态机选源（direct 取主干、rvc 取 AI 接收环），路由与静音
//! 淡变，f32 单声道复制为立体声 S16 写入 ALSA。
//! 静音 = 淡到零并保持；设备在静音与欠源期间持续写零，不停流（SPEC-02）。
//! direct 路由下 AI 环逐帧排空并计数（旧 main.c:112-116 `skipped_replies`：
//! 直出时后端回包不播、计数）。

use crate::alsa_stream::{self, Op, StreamError};
use crate::route::{Mute, Router};
use crate::state::Shared;
use mozart_core::ctl::schema::Route;
use mozart_core::ring::Ring;
use mozart_core::{OutputFrame, RawFrame, RAW_SAMPLES};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// 帧源等待预算：一个 20 ms 帧周期（主干与 AI 接收环共用）；超时补零续流，
/// 吸收采集与播放两硬件时钟的漂移。
const FRAME_WAIT: Duration = Duration::from_millis(20);

/// 单声道 f32 → 立体声 S16（L=R）：钳位 ±1，×32767（旧 float_to_s16_stereo）。
pub fn mono_to_s16_stereo(mono: &[f32], out: &mut [i16]) {
    for (pair, value) in out.chunks_exact_mut(2).zip(mono) {
        let clamped = value.clamp(-1.0, 1.0);
        let sample = (clamped * 32767.0) as i16;
        pair[0] = sample;
        pair[1] = sample;
    }
}

/// direct 路由下排空 AI 接收环并计数（帧号即弃）。
/// 后端回包在 direct 下属设计内丢弃：环不积压，回切 rvc 时无陈货。
fn drain_rvc_skipped(rvc: &Ring<OutputFrame>, shared: &Shared) {
    let mut skipped = 0u64;
    while rvc.pop(Some(Duration::ZERO)).is_some() {
        skipped += 1;
    }
    if skipped > 0 {
        shared.add_ai_skipped(skipped);
    }
}

/// 播放线程主体。返回 Err 表示致命流错误；主线程据此退出进程。
pub fn run(
    pcm: alsa::pcm::PCM,
    trunk: Arc<Ring<RawFrame>>,
    rvc: Arc<Ring<OutputFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let io = pcm
        .io_i16()
        .map_err(|err| format!("playback io_i16: {err}"))?;
    let mut stereo = [0i16; RAW_SAMPLES * 2];
    let mut router = Router::new(Route::Direct);
    let mut mute = Mute::new();
    let mut underruns = 0u64;
    let mut reported = 0u64;
    loop {
        // 命令目标在帧边界读入状态机；不持任何锁（原子读）。
        router.request(shared.route_target());
        mute.request(shared.mute_target());

        let mut mono = match router.route() {
            Route::Direct => {
                // packed 字段按值整块搬出，不取引用。
                let pcm = match trunk.pop(Some(FRAME_WAIT)) {
                    Some(frame) => frame.pcm,
                    // 主干欠载：补零续流。属两时钟漂移吸收，不计为丢弃。
                    None => [0f32; RAW_SAMPLES],
                };
                // direct 期间后端输出帧不播、排空并计数（旧 skipped_replies）。
                drain_rvc_skipped(&rvc, &shared);
                pcm
            }
            Route::Rvc => {
                // rvc 路由不消费主干；逐帧排空防积压。
                let _ = trunk.pop(Some(Duration::ZERO));
                // AI 接收环缺帧补零续流（大缓冲与欠载补零归 C++ 侧，SPEC-03）。
                match rvc.pop(Some(FRAME_WAIT)) {
                    Some(frame) => frame.pcm,
                    None => [0f32; RAW_SAMPLES],
                }
            }
        };

        // 路由淡变 × 静音淡变，逐样本增益相乘。
        router.apply(&mut mono);
        mute.apply(&mut mono);
        mono_to_s16_stereo(&mono, &mut stereo);

        match alsa_stream::transfer(
            &pcm,
            &io,
            Op::Write,
            &mut stereo,
            RAW_SAMPLES,
            stop,
            &mut underruns,
        ) {
            Ok(()) => shared.add_played(1),
            Err(StreamError::Cancelled) => return Ok(()),
            Err(StreamError::NoProgress) => {
                let message = "playback: no progress for 1 s".to_string();
                shared.set_last_error(&message);
                return Err(message);
            }
            Err(StreamError::Fatal(message)) => {
                shared.set_last_error(&message);
                return Err(format!("playback: {message}"));
            }
        }
        if underruns > reported {
            reported = underruns;
            let message = format!("playback underrun recovered (total {underruns})");
            shared.set_last_error(&message);
            eprintln!("mozart-pre: {message}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mozart_core::FrameMeta;

    #[test]
    fn stereo_conversion_copies_mono_to_both_channels() {
        let mono = [0.5f32, -0.25];
        let mut stereo = [0i16; 4];
        mono_to_s16_stereo(&mono, &mut stereo);
        assert_eq!(stereo, [16383, 16383, -8191, -8191]);
    }

    #[test]
    fn stereo_conversion_clamps_full_scale() {
        let mono = [1.0f32, -1.0, 2.0, -2.0];
        let mut stereo = [0i16; 8];
        mono_to_s16_stereo(&mono, &mut stereo);
        assert_eq!(&stereo[..4], &[32767, 32767, -32767, -32767]);
        // 超界样本钳位到满幅，不回绕。
        assert_eq!(&stereo[4..], &[32767, 32767, -32767, -32767]);
    }

    #[test]
    fn roundtrip_capture_playback_matches_legacy_scaling() {
        // 采集 /32768、播放 ×32767：回环幅值有 1/32768 的固定偏差，与旧实现一致。
        let stereo = [16384i16, -16384];
        let mut mono = [0f32; 1];
        crate::capture::fl_to_mono(&stereo, &mut mono);
        let mut back = [0i16; 2];
        mono_to_s16_stereo(&mono, &mut back);
        assert_eq!(back, [16383, 16383]);
    }

    #[test]
    fn frame_wait_matches_frame_period() {
        // 等待预算等于一个契约帧周期（frame.h：960 样本 @ 48 kHz）。
        let frame_ms = RAW_SAMPLES as u64 * 1000 / mozart_core::RAW_SAMPLE_RATE as u64;
        assert_eq!(FRAME_WAIT, Duration::from_millis(frame_ms));
    }

    #[test]
    fn direct_route_drains_rvc_ring_into_skipped_counter() {
        // direct 路由下 AI 环逐帧排空、按帧计数、环归零（旧 skipped_replies）。
        let shared = Shared::new("cap".to_string(), "play".to_string());
        let rvc = Ring::<OutputFrame>::new(4);
        for idx in 0..3u32 {
            rvc.push(OutputFrame {
                meta: FrameMeta {
                    pts_ns: 0,
                    frame_idx: idx,
                    vad_flag: 0,
                    energy_db: 0,
                    conf: 0,
                    segment_id: 0,
                },
                pcm: [0.0; mozart_core::OUTPUT_SAMPLES],
            });
        }
        drain_rvc_skipped(&rvc, &shared);
        assert_eq!(rvc.depth(), 0, "排空后 AI 环不得有陈货");
        let status = shared.status(crate::state::QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
        });
        assert_eq!(status.consumer_drops["rvc.skipped"], 3);

        // 空环再排：计数不增长（无帧不计数）。
        drain_rvc_skipped(&rvc, &shared);
        let status = shared.status(crate::state::QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
        });
        assert_eq!(status.consumer_drops["rvc.skipped"], 3);
    }
}
