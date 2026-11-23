//! 播放线程：扬声器分时（播报优先）+ 路由状态机选源（direct 取主干、rvc 取
//! AI 接收环），路由与静音淡变，f32 单声道复制为立体声 S16 写入 ALSA。
//! 扬声器分时（SPEC-05 播报投递）：TTS 播报环有帧则播播报、静音实时；播报
//! 空窗 holdoff 200 ms 等后续播报帧，窗口耗尽恢复实时。
//! 静音 = 淡到零并保持；设备在静音与欠源期间持续写零，不停流（SPEC-02）。
//! direct 路由下 AI 环逐帧排空并计数（旧 main.c:112-116 `skipped_replies`：
//! 直出时后端回包不播、计数）；播报期间同样排空 AI 环计 skipped。

use crate::alsa_stream::{self, Op, StreamError};
use crate::route::{FadeIn, Mute, Router, FADE_SAMPLES};
use crate::state::{Shared, Speaker};
use alsa::Direction;
use mozart_core::ctl::schema::{PlayoutPolicy, Route};
use mozart_core::ring::Ring;
use mozart_core::{OutputFrame, RawFrame, OUTPUT_SAMPLES, RAW_SAMPLES, RAW_SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 帧源等待预算：一个 20 ms 帧周期（主干与 AI 接收环共用）；超时补零续流，
/// 吸收采集与播放两硬件时钟的漂移。
const FRAME_WAIT: Duration = Duration::from_millis(20);

/// 播报连续发帧期的微抖动等待：仅在刚收到过播报帧、尚未进入 holdoff 空窗时使用，
/// 既可吸收 TTS 跨进程唤醒抖动，又远低于 ALSA 播放缓冲余量（~20 ms），防止周期性欠载。
const BROADCAST_JITTER_WAIT: Duration = Duration::from_millis(4);

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

    /// 每轮播放前取 TTS 环的等待预算：仅播报连续发帧期给微抖动等待，
    /// 实时态与 holdoff 空窗期零等待，由 ALSA 阻塞写独自定拍。
    fn poll_wait(&self) -> Duration {
        if self.state == Speaker::Broadcast && self.holdoff_until.is_none() {
            BROADCAST_JITTER_WAIT
        } else {
            Duration::ZERO
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

/// 播放边界无分配状态化重采样器：当 RVC 模型原生采样率（如 40 kHz）与 ALSA
/// 播放采样率（48 kHz）不同时，在播放线程入口做一次 1 样本延迟线性插值重采样；
/// 采样率一致时零拷贝直通。
struct BoundaryResampler {
    last_sample: f32,
    last_rate: u32,
}

impl BoundaryResampler {
    fn new() -> Self {
        Self {
            last_sample: 0.0,
            last_rate: 0,
        }
    }

    fn reset(&mut self) {
        self.last_sample = 0.0;
        self.last_rate = 0;
    }

    fn resample_to_48k(&mut self, frame: &OutputFrame) -> [f32; RAW_SAMPLES] {
        let rate = frame.sample_rate;
        let valid = (frame.valid_samples as usize).min(OUTPUT_SAMPLES);
        let pcm = frame.pcm;
        if rate == RAW_SAMPLE_RATE && valid == RAW_SAMPLES {
            self.last_sample = pcm[RAW_SAMPLES - 1];
            self.last_rate = rate;
            return pcm;
        }
        if valid == 0 || rate == 0 {
            self.reset();
            return [0f32; RAW_SAMPLES];
        }
        if self.last_rate != rate {
            self.last_sample = pcm[0];
            self.last_rate = rate;
        }
        let mut out = [0f32; RAW_SAMPLES];
        let step = valid as f64 / RAW_SAMPLES as f64;
        for (j, slot) in out.iter_mut().enumerate() {
            let pos = (j as f64) * step - 1.0;
            if pos < 0.0 {
                let frac = (pos + 1.0) as f32;
                *slot = self.last_sample + (pcm[0] - self.last_sample) * frac;
            } else {
                let idx0 = (pos as usize).min(valid - 1);
                let idx1 = (idx0 + 1).min(valid - 1);
                let frac = (pos - idx0 as f64) as f32;
                *slot = pcm[idx0] + (pcm[idx1] - pcm[idx0]) * frac;
            }
        }
        self.last_sample = pcm[valid - 1];
        out
    }
}

/// 实时与播报切换淡变（10 ms = FADE_SAMPLES）：
/// - Realtime → Broadcast：前 10 ms 从切换当帧的实时采样线性交叉淡化到播报帧；
/// - Broadcast → Realtime：前 10 ms 从静音（holdoff 尾部）线性淡入到实时帧。
fn apply_speaker_fade(
    prev: Speaker,
    state: Speaker,
    mono: &mut [f32; RAW_SAMPLES],
    realtime_tail: &[f32; RAW_SAMPLES],
) {
    match (prev, state) {
        (Speaker::Realtime, Speaker::Broadcast) => {
            for i in 0..FADE_SAMPLES {
                let gain = (i + 1) as f32 / FADE_SAMPLES as f32;
                mono[i] = realtime_tail[i] * (1.0 - gain) + mono[i] * gain;
            }
        }
        (Speaker::Broadcast, Speaker::Realtime) => {
            for (i, sample) in mono.iter_mut().take(FADE_SAMPLES).enumerate() {
                let gain = (i + 1) as f32 / FADE_SAMPLES as f32;
                *sample *= gain;
            }
        }
        _ => {}
    }
}

/// 播报帧推进：切换首帧从当前实时路由取样做交叉淡化，随后播报期间只排空
/// 实时环、重置 RVC 重采样器，并在静音占位缓冲上推进路由状态机（不改播报音量）。
fn step_broadcast(
    prev: Speaker,
    tts_frame: Option<OutputFrame>,
    allow_realtime_tail: bool,
    trunk: &Ring<RawFrame>,
    rvc: &Ring<OutputFrame>,
    shared: &Shared,
    router: &mut Router,
    rvc_resampler: &mut BoundaryResampler,
) -> [f32; RAW_SAMPLES] {
    let mut realtime_tail = [0f32; RAW_SAMPLES];
    if allow_realtime_tail && prev == Speaker::Realtime {
        realtime_tail = match router.route() {
            Route::Direct => {
                drain_rvc_skipped(rvc, shared);
                rvc_resampler.reset();
                trunk
                    .pop(Some(Duration::ZERO))
                    .map_or([0f32; RAW_SAMPLES], |frame| frame.pcm)
            }
            Route::Rvc => {
                let _ = trunk.pop(Some(Duration::ZERO));
                let tail = rvc
                    .pop(Some(Duration::ZERO))
                    .map_or([0f32; RAW_SAMPLES], |frame| {
                        rvc_resampler.resample_to_48k(&frame)
                    });
                drain_rvc_skipped(rvc, shared);
                rvc_resampler.reset();
                tail
            }
        };
        router.apply(&mut realtime_tail);
    } else {
        drain_rvc_skipped(rvc, shared);
        let _ = trunk.pop(Some(Duration::ZERO));
        rvc_resampler.reset();
        let mut dummy = [0f32; RAW_SAMPLES];
        router.apply(&mut dummy);
    }
    let mut mono = match tts_frame {
        Some(frame) => {
            shared.add_tts_frames_played(1);
            frame.pcm
        }
        // holdoff 空窗：补零续流，等待后续播报帧。
        None => [0f32; RAW_SAMPLES],
    };
    apply_speaker_fade(prev, Speaker::Broadcast, &mut mono, &realtime_tail);
    mono
}

/// 实时帧推进：按路由选源并应用路由淡变；刚从播报恢复时跳过等待预算，
fn select_speech(tagged: mozart_core::SpeechFrame, cutoff: u64) -> Option<OutputFrame> {
    (tagged.issued_ns >= cutoff).then_some(tagged.frame)
}

/// 避免与 tts.pop(FRAME_WAIT) 叠加成两帧等待。
fn step_realtime(
    prev: Speaker,
    trunk: &Ring<RawFrame>,
    rvc: &Ring<OutputFrame>,
    shared: &Shared,
    router: &mut Router,
    rvc_resampler: &mut BoundaryResampler,
) -> [f32; RAW_SAMPLES] {
    let wait = if prev == Speaker::Broadcast {
        Duration::ZERO
    } else {
        FRAME_WAIT
    };
    let mut mono = match router.route() {
        Route::Direct => {
            // packed 字段按值整块搬出，不取引用。
            let pcm = match trunk.pop(Some(wait)) {
                Some(frame) => frame.pcm,
                // 主干欠载：补零续流。属两时钟漂移吸收，不计为丢弃。
                None => [0f32; RAW_SAMPLES],
            };
            // direct 期间后端输出帧不播、排空并计数（旧 skipped_replies）。
            drain_rvc_skipped(rvc, shared);
            rvc_resampler.reset();
            pcm
        }
        Route::Rvc => {
            // rvc 路由不消费主干；逐帧排空防积压。
            let _ = trunk.pop(Some(Duration::ZERO));
            // AI 接收环缺帧补零续流（大缓冲与欠载补零归 C++ 侧，SPEC-03）。
            match rvc.pop(Some(wait)) {
                Some(frame) => rvc_resampler.resample_to_48k(&frame),
                None => {
                    rvc_resampler.reset();
                    [0f32; RAW_SAMPLES]
                }
            }
        }
    };
    router.apply(&mut mono);
    apply_speaker_fade(prev, Speaker::Realtime, &mut mono, &[0f32; RAW_SAMPLES]);
    mono
}

/// 播放线程主体。返回 Err 表示致命流错误；主线程据此退出进程。
pub fn run(
    mut pcm: alsa::pcm::PCM,
    trunk: Arc<Ring<RawFrame>>,
    rvc: Arc<Ring<OutputFrame>>,
    tts: Arc<Ring<mozart_core::SpeechFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let mut stereo = [0i16; RAW_SAMPLES * 2];
    let mut router = Router::new(Route::Direct);
    let mut mute = Mute::new();
    let mut fade_in = FadeIn::new();
    let mut speaker = SpeakerScheduler::new();
    let mut previous_speaker = Speaker::Realtime;
    let mut rvc_resampler = BoundaryResampler::new();
    let mut underruns = 0u64;
    let mut reported = 0u64;
    loop {
        // This boundary follows the previous ALSA write. Acknowledgement here
        // precedes selection using the installed cutoff.
        let request = shared.playback_request.load(Ordering::SeqCst);
        let policy = shared.playout_policy();
        shared.playback_ack.store(request, Ordering::SeqCst);
        if shared.playback_switch.has_pending() {
            shared.playback_switch.apply_pending(
                |target| alsa_stream::open_and_verify_io("playback", target, Direction::Playback),
                |_, new_pcm| {
                    pcm = new_pcm;
                    fade_in.trigger();
                },
            );
        }
        let io = pcm.io_i16().map_err(|err| {
            let message = format!("playback io_i16: {err}");
            shared.set_last_error(&message);
            message
        })?;
        // 命令目标在帧边界读入状态机；不持任何锁（原子读）。
        router.request(shared.route_target());
        mute.request(shared.mute_target());

        // 播报优先：仅播报连续发帧期给微抖动等待，holdoff 空窗期与实时态零等待。
        let tts_allowed = matches!(policy, PlayoutPolicy::Legacy | PlayoutPolicy::BroadcastOnly);
        let wait = if tts_allowed { speaker.poll_wait() } else { Duration::ZERO };
        let tts_frame = tts
            .pop(Some(wait))
            .filter(|_| tts_allowed)
            .and_then(|tagged| select_speech(tagged, shared.speech_cutoff.load(Ordering::SeqCst)));
        let prev = previous_speaker;
        let scheduled = speaker.advance(tts_frame.is_some(), Instant::now());
        let state = match policy {
            PlayoutPolicy::Legacy => scheduled,
            PlayoutPolicy::RealtimeOnly => Speaker::Realtime,
            PlayoutPolicy::BroadcastOnly | PlayoutPolicy::Silent => Speaker::Broadcast,
        };
        if state != prev {
            shared.set_speaker_state(state);
            previous_speaker = state;
        }

        let mut mono = match state {
            Speaker::Broadcast => step_broadcast(
                prev,
                tts_frame,
                policy == PlayoutPolicy::Legacy,
                &trunk,
                &rvc,
                &shared,
                &mut router,
                &mut rvc_resampler,
            ),
            Speaker::Realtime => {
                step_realtime(prev, &trunk, &rvc, &shared, &mut router, &mut rvc_resampler)
            }
        };

        // 静音淡变与切设备起播淡入作用于当前最终扬声器输出。
        fade_in.apply(&mut mono);
        mute.apply(&mut mono);
        mono_to_s16_stereo(&mono, &mut stereo);

        match alsa_stream::transfer(
            &pcm,
            &io,
            Op::Write,
            &mut stereo,
            RAW_SAMPLES,
            stop,
            shared.playback_switch.pending_flag(),
            &mut underruns,
        ) {
            Ok(()) => shared.add_played(1),
            Err(StreamError::Cancelled) => return Ok(()),
            Err(StreamError::Reopen) => continue,
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
    #[test]
    fn speech_fence_rejects_delayed_old_frame_and_accepts_new() {
        let frame = || {
            mozart_core::OutputFrame::new_48k(
                mozart_core::FrameMeta {
                    pts_ns: 0,
                    frame_idx: 0,
                    vad_flag: 0,
                    energy_db: 0,
                    conf: 0,
                    segment_id: 0,
                },
                [0.0; mozart_core::OUTPUT_SAMPLES],
            )
        };
        assert!(super::select_speech(
            mozart_core::SpeechFrame {
                issued_ns: 9,
                frame: frame()
            },
            10
        )
        .is_none());
        assert!(super::select_speech(
            mozart_core::SpeechFrame {
                issued_ns: 10,
                frame: frame()
            },
            10
        )
        .is_some());
    }
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
        let shared = crate::state::test_shared("cap", "play", "test");
        let rvc = Ring::<OutputFrame>::new(4);
        for idx in 0..3u32 {
            rvc.push(OutputFrame::new_48k(
                FrameMeta {
                    pts_ns: 0,
                    frame_idx: idx,
                    vad_flag: 0,
                    energy_db: 0,
                    conf: 0,
                    segment_id: 0,
                },
                [0.0; mozart_core::OUTPUT_SAMPLES],
            ));
        }
        drain_rvc_skipped(&rvc, &shared);
        assert_eq!(rvc.depth(), 0, "排空后 AI 环不得有陈货");
        let status = shared.status(crate::state::QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
        });
        assert_eq!(status.consumer_drops["rvc.skipped"], 3);

        // 空环再排：计数不增长（无帧不计数）。
        drain_rvc_skipped(&rvc, &shared);
        let status = shared.status(crate::state::QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
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

    #[test]
    fn speaker_scheduler_waits_only_during_active_broadcast() {
        let mut scheduler = SpeakerScheduler::new();
        let t0 = Instant::now();
        assert_eq!(scheduler.poll_wait(), Duration::ZERO);

        assert_eq!(scheduler.advance(true, t0), Speaker::Broadcast);
        assert_eq!(scheduler.poll_wait(), BROADCAST_JITTER_WAIT);

        // 进入 holdoff 空窗后立刻退回零等待，避免空等叠加 ALSA 阻塞写导致 underrun。
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(20)),
            Speaker::Broadcast
        );
        assert_eq!(scheduler.poll_wait(), Duration::ZERO);

        // 恢复发帧后重新开启微抖动等待；holdoff 到期切回 Realtime 后仍为零等待。
        assert_eq!(
            scheduler.advance(true, t0 + Duration::from_millis(40)),
            Speaker::Broadcast
        );
        assert_eq!(scheduler.poll_wait(), BROADCAST_JITTER_WAIT);
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(60)),
            Speaker::Broadcast
        );
        assert_eq!(
            scheduler.advance(false, t0 + Duration::from_millis(260)),
            Speaker::Realtime
        );
        assert_eq!(scheduler.poll_wait(), Duration::ZERO);
    }

    #[test]
    fn boundary_resampler_handles_40k_and_48k_passthrough() {
        let mut resampler = BoundaryResampler::new();
        let meta = FrameMeta {
            pts_ns: 0,
            frame_idx: 0,
            vad_flag: 1,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        };
        let mut pcm48 = [0.0f32; OUTPUT_SAMPLES];
        pcm48[0] = 0.25;
        pcm48[959] = -0.5;
        let out48 = resampler.resample_to_48k(&OutputFrame::new_48k(meta, pcm48));
        assert_eq!(out48[0], 0.25);
        assert_eq!(out48[959], -0.5);

        let mut pcm40 = [0.0f32; OUTPUT_SAMPLES];
        for i in 0..800 {
            pcm40[i] = i as f32 / 800.0;
        }
        let out40 = resampler.resample_to_48k(&OutputFrame::new(meta, 40000, 800, pcm40));
        assert_eq!(out40.len(), RAW_SAMPLES);
        assert!(out40.iter().all(|v| v.is_finite()));
        assert!((out40[0] - 0.0).abs() < 1e-6);
        assert!(out40[959] > 0.99);
    }

    /// 32k→48k 边界重采样：只取 valid_samples 有效段插值到 960 样本，
    /// 段外陈货不得泄漏；换采样率时重置插值历史，不跨率插值。
    #[test]
    fn boundary_resampler_handles_32k_and_rate_switch() {
        let mut resampler = BoundaryResampler::new();
        let meta = FrameMeta {
            pts_ns: 0,
            frame_idx: 0,
            vad_flag: 1,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        };
        let mut pcm32 = [0.0f32; OUTPUT_SAMPLES];
        for (i, slot) in pcm32.iter_mut().enumerate().take(640) {
            *slot = i as f32 / 640.0;
        }
        // 有效段之外模拟补零/陈货：不得参与插值。
        for slot in pcm32[640..].iter_mut() {
            *slot = 99.0;
        }
        let out = resampler.resample_to_48k(&OutputFrame::new(meta, 32000, 640, pcm32));
        assert_eq!(out.len(), RAW_SAMPLES);
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(
            out.iter().all(|v| (0.0..=1.0).contains(v)),
            "valid_samples 之外的样本不得泄漏到播放"
        );
        assert!(
            out.windows(2).all(|pair| pair[1] >= pair[0]),
            "斜坡插值必须单调不回折"
        );
        assert!((out[0]).abs() < 1e-6);
        assert!((out[480] - 319.0 / 640.0).abs() < 1e-3);
        assert!(out[959] > 0.99);

        // 切到 40k：插值历史重置为新率首样本。
        let mut pcm40 = [0.0f32; OUTPUT_SAMPLES];
        for (i, slot) in pcm40.iter_mut().enumerate().take(800) {
            *slot = 0.5 + i as f32 / 1600.0;
        }
        let out40 = resampler.resample_to_48k(&OutputFrame::new(meta, 40000, 800, pcm40));
        assert!(
            (out40[0] - 0.5).abs() < 1e-6,
            "换率首帧必须从新率首样本起插值"
        );

        // valid_samples = 0：输出静音并重置历史。
        let silent = resampler.resample_to_48k(&OutputFrame::new(meta, 32000, 0, pcm32));
        assert!(silent.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn speaker_fade_crossfades_into_broadcast_and_fades_back() {
        // Realtime → Broadcast：前 480 样本从实时尾部线性过渡到播报帧，后 480 保持纯播报。
        let tail = [1.0f32; RAW_SAMPLES];
        let mut mono = [-1.0f32; RAW_SAMPLES];
        apply_speaker_fade(Speaker::Realtime, Speaker::Broadcast, &mut mono, &tail);
        let expected_first = 1.0 * (1.0 - 1.0 / FADE_SAMPLES as f32) + (-1.0) / FADE_SAMPLES as f32;
        assert!((mono[0] - expected_first).abs() < 1e-5);
        assert!((mono[FADE_SAMPLES / 2 - 1]).abs() < 1e-5);
        assert_eq!(mono[FADE_SAMPLES - 1], -1.0);
        assert_eq!(mono[RAW_SAMPLES - 1], -1.0);

        // Broadcast → Realtime：前 480 样本从 0 线性淡入到 1，后 480 保持满幅。
        let mut back = [0.8f32; RAW_SAMPLES];
        apply_speaker_fade(Speaker::Broadcast, Speaker::Realtime, &mut back, &tail);
        assert!((back[0] - 0.8 / FADE_SAMPLES as f32).abs() < 1e-6);
        assert_eq!(back[FADE_SAMPLES - 1], 0.8);
        assert_eq!(back[RAW_SAMPLES - 1], 0.8);
    }

    #[test]
    fn exclusive_playout_has_no_microphone_tail_or_gap_audio() {
        let shared = crate::state::test_shared("cap", "play", "startup");
        let trunk = Ring::<RawFrame>::new(4);
        let rvc = Ring::<OutputFrame>::new(4);
        let mut router = Router::new(Route::Direct);
        let mut resampler = BoundaryResampler::new();
        let meta = FrameMeta { pts_ns: 0, frame_idx: 0, vad_flag: 1, energy_db: 0, conf: 0, segment_id: 0 };
        trunk.push(RawFrame { meta, pcm: [0.8; RAW_SAMPLES] });
        let gap = step_broadcast(Speaker::Realtime, None, false, &trunk, &rvc, &shared, &mut router, &mut resampler);
        assert!(gap.iter().all(|&sample| sample == 0.0));
        trunk.push(RawFrame { meta, pcm: [0.8; RAW_SAMPLES] });
        let voiced = step_broadcast(Speaker::Broadcast, Some(OutputFrame::new_48k(meta, [0.25; OUTPUT_SAMPLES])), false,
            &trunk, &rvc, &shared, &mut router, &mut resampler);
        assert!(voiced.iter().all(|&sample| sample == 0.25));
        assert!(trunk.pop(Some(Duration::ZERO)).is_none());
    }

    #[test]
    fn broadcast_isolates_router_fade_and_resets_resampler() {
        let shared = crate::state::test_shared("cap", "play", "startup");
        let trunk = Ring::<RawFrame>::new(4);
        let rvc = Ring::<OutputFrame>::new(4);
        let mut router = Router::new(Route::Direct);
        let mut resampler = BoundaryResampler::new();
        let meta = FrameMeta {
            pts_ns: 0,
            frame_idx: 0,
            vad_flag: 1,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        };
        // 预先让 resampler 处于 40 kHz 历史（last_rate != 0）。
        let _ =
            resampler.resample_to_48k(&OutputFrame::new(meta, 40000, 800, [0.9; OUTPUT_SAMPLES]));
        assert_eq!(resampler.last_rate, 40000);

        // 在播报期切路由（Direct → Rvc）：router 状态机在后台完成切换，
        // 播报帧不受路由淡变削弱，且 resampler 历史被重置。
        router.request(Route::Rvc);
        let out1 = step_broadcast(
            Speaker::Broadcast,
            Some(OutputFrame::new_48k(meta, [0.5; OUTPUT_SAMPLES])),
            true,
            &trunk,
            &rvc,
            &shared,
            &mut router,
            &mut resampler,
        );
        assert!(out1.iter().all(|&v| v == 0.5), "播报帧不得被路由淡变衰减");
        assert_eq!(resampler.last_rate, 0, "播报期须重置 RVC 重采样器历史");

        let out2 = step_broadcast(
            Speaker::Broadcast,
            Some(OutputFrame::new_48k(meta, [0.5; OUTPUT_SAMPLES])),
            true,
            &trunk,
            &rvc,
            &shared,
            &mut router,
            &mut resampler,
        );
        assert!(out2.iter().all(|&v| v == 0.5));
        assert_eq!(router.route(), Route::Rvc, "路由状态机须在后台完成切换");
    }
}
