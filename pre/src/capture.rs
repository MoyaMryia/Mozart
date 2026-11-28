//! 采集线程：ALSA 读入 S16 立体声，period 64 帧攒 960 帧块，取 FL 单声道转 f32 主干。
//! 取 FL 不做双声道均值（旧 main.c:717；均值会引入梳状滤波）。

use crate::alsa_stream::{self, Op, StreamError, PERIOD_FRAMES};
use crate::state::Shared;
use alsa::Direction;
use mozart_core::ring::Ring;
use mozart_core::{FrameMeta, RawFrame, FRAME_MS, RAW_SAMPLES, RAW_SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 单调时钟时间戳（纳秒），填 `meta.pts_ns`（frame.h 语义）。
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts 可写；CLOCK_MONOTONIC 由内核支持。
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// S16 → f32 归一化除数；设备采集与文件采集源共用。
pub const S16_NORM: f32 = 32768.0;

/// 在采集边界保存门控与设备恢复代次，不让排队跨越接管或采集切换。
#[derive(Clone, Copy)]
pub struct CapturedFrame {
    pub frame: RawFrame,
    pub rvc_ai_active: bool,
    pub file_generation: u64,
    pub capture_epoch: u64,
    /// 本帧首样本被采集硬件录入的单调时钟估测时刻（ns）；未启用 AEC 或文件源时为 0。
    pub cap_start_ns: u64,
}

impl std::ops::Deref for CapturedFrame {
    type Target = RawFrame;
    fn deref(&self) -> &RawFrame {
        &self.frame
    }
}

pub fn enqueue(ring: &Ring<CapturedFrame>, frame: RawFrame, shared: &Shared) {
    enqueue_timed(ring, frame, 0, shared);
}

pub fn enqueue_timed(
    ring: &Ring<CapturedFrame>,
    frame: RawFrame,
    cap_start_ns: u64,
    shared: &Shared,
) {
    let (rvc_ai_active, file_generation) = shared.file_snapshot();
    ring.push(CapturedFrame {
        frame,
        file_generation,
        rvc_ai_active,
        capture_epoch: shared.capture_epoch(),
        cap_start_ns,
    });
}

const FRAME_NS: u64 = FRAME_MS as u64 * 1_000_000;

fn capture_alsa_delay_ns(pcm: &alsa::pcm::PCM) -> u64 {
    pcm.status()
        .ok()
        .and_then(|st| {
            let frames = st.get_delay();
            (frames > 0).then(|| {
                ((frames as u64).min(RAW_SAMPLE_RATE as u64 / 2) * 1_000_000_000)
                    / RAW_SAMPLE_RATE as u64
            })
        })
        .unwrap_or(0)
}

/// 交错立体声 S16 → 单声道 f32：取 FL（左声道），除以 32768。
pub fn fl_to_mono(stereo: &[i16], out: &mut [f32]) {
    for (slot, pair) in out.iter_mut().zip(stereo.chunks_exact(2)) {
        *slot = pair[0] as f32 / S16_NORM;
    }
}

/// 采集线程主体：攒满 960 样本即组装 RawFrame 入队。
/// 启动阶段若指定采集设备未就绪（`pcm == None`），以待机节拍等待 `PRE_SET_DEVICE` 热切接入；
/// 运行中遇致命流错误仍返回 `Err`，交 daemon 退避重启。
pub fn run(
    mut pcm: Option<alsa::pcm::PCM>,
    ring: Arc<Ring<CapturedFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
    mut probe: Option<crate::aec_probe::Stream>,
) -> Result<(), String> {
    let mut stereo = [0i16; PERIOD_FRAMES * 2];
    let mut block = [0f32; RAW_SAMPLES];
    let mut filled = 0usize;
    let mut frame_idx: u32 = 0;
    let mut overruns = 0u64;
    let mut reported = 0u64;
    let standby_poll = Duration::from_millis(FRAME_MS as u64);
    loop {
        if shared.capture_switch.has_pending() {
            let timestamp = probe.is_some();
            shared.capture_switch.apply_pending(
                |target| {
                    alsa_stream::open_and_verify_io(
                        "capture",
                        target,
                        Direction::Capture,
                        timestamp,
                    )
                },
                |target, new_pcm| {
                    if let Some(probe) = &mut probe {
                        probe.discontinuity();
                    }
                    pcm = Some(new_pcm);
                    filled = 0;
                    shared.bump_capture_epoch();
                    shared.clear_startup_open_error_if_ready();
                    shared.refresh_capture_mixer(pcm.as_ref(), target);
                },
            );
        }
        let Some(active_pcm) = pcm.as_ref() else {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(standby_poll);
            continue;
        };
        let io = active_pcm.io_i16().map_err(|err| {
            let message = format!("capture io_i16: {err}");
            shared.set_last_error(&message);
            message
        })?;
        match alsa_stream::transfer(
            active_pcm,
            &io,
            Op::Read,
            &mut stereo,
            PERIOD_FRAMES,
            stop,
            shared.capture_switch.pending_flag(),
            &mut overruns,
            probe.as_mut(),
        ) {
            Ok(()) => {}
            Err(StreamError::Cancelled) => return Ok(()),
            Err(StreamError::Reopen) => {
                filled = 0;
                continue;
            }
            Err(StreamError::NoProgress) => {
                let message = "capture: no progress for 1 s".to_string();
                shared.set_last_error(&message);
                return Err(message);
            }
            Err(StreamError::Fatal(message)) => {
                shared.set_last_error(&message);
                return Err(format!("capture: {message}"));
            }
        }
        if overruns > reported {
            reported = overruns;
            filled = 0;
            shared.bump_capture_epoch();
            let message = format!("capture overrun recovered (total {overruns})");
            shared.set_last_error(&message);
            eprintln!("mozart-pre: {message}");
            continue;
        }

        // 攒块：15 × 64 = 960 样本。
        fl_to_mono(&stereo, &mut block[filled..filled + PERIOD_FRAMES]);
        filled += PERIOD_FRAMES;
        if filled < RAW_SAMPLES {
            continue;
        }
        filled = 0;
        let pts_ns = now_ns();
        let cap_start_ns = if shared.aec_enabled() {
            let unread_ns = capture_alsa_delay_ns(active_pcm);
            pts_ns.saturating_sub(FRAME_NS + unread_ns)
        } else {
            0
        };
        // packed 字段按值构造；frame_idx 回填 Task 3/4 的 DSP/VAD 前先置零。
        let frame = RawFrame {
            meta: FrameMeta {
                pts_ns,
                frame_idx,
                vad_flag: 0,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm: block,
        };
        enqueue_timed(&ring, frame, cap_start_ns, &shared);
        frame_idx = frame_idx.wrapping_add(1);
        shared.add_captured(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fl_channel_becomes_mono_float() {
        let stereo = [100i16, -100, 200, -200, 0, 4096];
        let mut mono = [0f32; 3];
        fl_to_mono(&stereo, &mut mono);
        assert_eq!(mono, [100.0 / 32768.0, 200.0 / 32768.0, 0.0]);
    }

    #[test]
    fn fl_to_mono_ignores_right_channel() {
        // FR 与 FL 不同时，输出只跟 FL：验证未做均值。
        let stereo = [16384i16, -16384, 8192, 24576];
        let mut mono = [0f32; 2];
        fl_to_mono(&stereo, &mut mono);
        assert_eq!(mono, [16384.0 / 32768.0, 8192.0 / 32768.0]);
    }

    #[test]
    fn period_divides_frame_exactly() {
        // 960 必须能被周期整除，攒块不会跨帧残留。
        assert_eq!(RAW_SAMPLES % PERIOD_FRAMES, 0);
    }
}
