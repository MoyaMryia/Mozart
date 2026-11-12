//! 播放线程：扬声器分时（播报优先）+ 路由状态机选源（direct 取主干、rvc 取
//! AI 接收环），路由与静音淡变，f32 单声道复制为立体声 S16 写入 ALSA。
//! 扬声器分时（SPEC-05 播报投递）：TTS 播报环有帧则播播报、静音实时；播报
//! 空窗 holdoff 200 ms 等后续播报帧，窗口耗尽恢复实时。
//! 静音 = 淡到零并保持；设备在静音与欠源期间持续写零，不停流（SPEC-02）。
//! direct 路由下 AI 环逐帧排空并计数（旧 main.c:112-116 `skipped_replies`：
//! 直出时后端回包不播、计数）；播报期间同样排空 AI 环计 skipped。

use crate::alsa_stream::{self, Op, StreamError};
use crate::route::{Mute, Router};
use crate::state::{Shared, Speaker};
use mozart_core::ctl::schema::Route;
use mozart_core::ring::Ring;
use mozart_core::{OutputFrame, RawFrame, RAW_SAMPLES};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 帧源等待预算：一个 20 ms 帧周期（主干与 AI 接收环共用）；超时补零续流，
/// 吸收采集与播放两硬件时钟的漂移。
const FRAME_WAIT: Duration = Duration::from_millis(20);

/// 播报空窗保持时长：TTS 环空后等待后续播报帧的窗口，期间静音实时；
/// 窗口耗尽恢复实时（SPEC-05 播报投递「播完恢复」）。
const BROADCAST_HOLDOFF: Duration = Duration::from_millis(200);

/// 扬声器分时调度（纯逻辑，不触碰设备与环）：播报优先，播报空窗 holdoff
/// 后恢复实时。时间经参数注入，便于逐分支单测。
struct SpeakerScheduler {
    state: Speaker,
    /// 播报空窗起点 + holdoff 的到期时刻；有播报帧时清空。
    holdoff_until: Option<Instant>,
}

impl SpeakerScheduler {
    fn new() -> Self {
        Self {
            state: Speaker::Realtime,
            holdoff_until: None,
        }
    }

    /// 每帧推进：有播报帧 → Broadcast 并重置 holdoff；播报空窗超过 holdoff
    /// → Realtime。holdoff 从首个空窗帧起算，返回当前状态。
    fn advance(&mut self, tts_available: bool, now: Instant) -> Speaker {
        if tts_available {
            self.state = Speaker::Broadcast;
            self.holdoff_until = None;
        } else if self.state == Speaker::Broadcast {
            let until = *self.holdoff_until.get_or_insert(now + BROADCAST_HOLDOFF);
            if now >= until {
                self.state = Speaker::Realtime;
                self.holdoff_until = None;
            }
        }
        self.state
    }
}

/// 单声道 f32 → 立体声 S16（L=R）：钳位 ±1，×32767（旧 float_to_s16_stereo）。
pub fn mono_to_s16_stereo(mono: &[f32], out: &mut [i16]) {
    for (pair, value) in out.chunks_exact_mut(2).zip(mono) {
        let clamped = value.clamp(-1.0, 1.0);
        let sample = (clamped * 32767.0) as i16;
        pair[0] = sample;
        pair[1] = sample;
    }
}

/// direct 路由或播报期间排空 AI 接收环并计数（帧号即弃）。
/// 后端回包在这两种情形下属设计内丢弃：环不积压，回切 rvc 时无陈货。
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
    tts: Arc<Ring<OutputFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let io = pcm
        .io_i16()
        .map_err(|err| format!("playback io_i16: {err}"))?;
    let mut stereo = [0i16; RAW_SAMPLES * 2];
    let mut router = Router::new(Route::Direct);
    let mut mute = Mute::new();
    let mut speaker = SpeakerScheduler::new();
    let mut underruns = 0u64;
    let mut reported = 0u64;
    loop {
        // 命令目标在帧边界读入状态机；不持任何锁（原子读）。
        router.request(shared.route_target());
        mute.request(shared.mute_target());

        // 播报优先：非阻塞取播报帧，推进分时状态（仅在变化时发布原子）。
        let tts_frame = tts.pop(Some(Duration::ZERO));
        let prev = speaker.state;
        let state = speaker.advance(tts_frame.is_some(), Instant::now());
        if state != prev {
            shared.set_speaker_state(state);
        }

        let mut mono = match state {
            Speaker::Broadcast => {
                // 播报期间实时不播：AI 环排空计 skipped，主干逐帧排空防陈货。
                drain_rvc_skipped(&rvc, &shared);
                let _ = trunk.pop(Some(Duration::ZERO));
                match tts_frame {
                    Some(frame) => {
                        shared.add_tts_frames_played(1);
                        frame.pcm
                    }
                    // holdoff 空窗：补零续流，等待后续播报帧。
                    None => [0f32; RAW_SAMPLES],
                }
            }
            Speaker::Realtime => match router.route() {
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
            },
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

    // ---- 扬声器分时调度（纯逻辑，注入时间） ----

    #[test]
    fn speaker_starts_realtime_and_stays_without_tts() {
        let mut scheduler = SpeakerScheduler::new();
        let t0 = Instant::now();
        // 从未播报：空窗不触发 holdoff，恒 Realtime。
        for ms in [0, 100, 300, 1000] {
            assert_eq!(
                scheduler.advance(false, t0 + Duration::from_millis(ms)),
                Speaker::Realtime
            );
        }
    }

    #[test]
    fn tts_frame_enters_broadcast_then_holdoff_expires_to_realtime() {
        let mut scheduler = SpeakerScheduler::new();
        let t0 = Instant::now();
        assert_eq!(scheduler.advance(true, t0), Speaker::Broadcast);
        // 首个空窗帧起算 holdoff（至 t0+200）：窗口内保持 Broadcast。
        assert_eq!(scheduler.advance(false, t0), Speaker::Broadcast);
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(199)),
            Speaker::Broadcast
        );
        // 窗口耗尽恢复 Realtime。
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(200)),
            Speaker::Realtime
        );
        // 恢复后无播报帧保持 Realtime。
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(400)),
            Speaker::Realtime
        );
    }

    #[test]
    fn tts_frame_during_holdoff_resets_window() {
        let mut scheduler = SpeakerScheduler::new();
        let t0 = Instant::now();
        assert_eq!(scheduler.advance(true, t0), Speaker::Broadcast);
        // 空窗 holdoff 从 t0 起算（至 t0+200）。
        assert_eq!(scheduler.advance(false, t0), Speaker::Broadcast);
        // holdoff 内（t0+150）新播报帧重置窗口。
        assert_eq!(
            scheduler.advance(true, t0 + Duration::from_millis(150)),
            Speaker::Broadcast
        );
        // 重置后首个空窗帧（t0+300）重新起算 holdoff（至 t0+500）。
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(300)),
            Speaker::Broadcast
        );
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(499)),
            Speaker::Broadcast
        );
        // 原窗口（t0+200）早已过期，但重置后须到新窗口（t0+500）才恢复。
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(500)),
            Speaker::Realtime
        );
    }

    #[test]
    fn broadcast_holdoff_matches_200ms() {
        assert_eq!(BROADCAST_HOLDOFF, Duration::from_millis(200));
    }
}
