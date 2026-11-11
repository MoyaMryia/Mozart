//! 采集线程：ALSA 读入 S16 立体声，period 64 帧攒 960 帧块，取 FL 单声道转 f32 主干。
//! 取 FL 不做双声道均值（旧 main.c:717；均值会引入梳状滤波）。

use crate::alsa_stream::{self, Op, StreamError, PERIOD_FRAMES};
use crate::state::Shared;
use mozart_core::ring::Ring;
use mozart_core::{FrameMeta, RawFrame, RAW_SAMPLES};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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

/// 交错立体声 S16 → 单声道 f32：取 FL（左声道），除以 32768。
pub fn fl_to_mono(stereo: &[i16], out: &mut [f32]) {
    for (slot, pair) in out.iter_mut().zip(stereo.chunks_exact(2)) {
        *slot = pair[0] as f32 / 32768.0;
    }
}

/// 采集线程主体：攒满 960 样本即组装 RawFrame 入队。
/// 返回 Err 表示致命流错误；主线程据此退出进程，daemon 按退避重启。
pub fn run(
    pcm: alsa::pcm::PCM,
    ring: Arc<Ring<RawFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let io = pcm
        .io_i16()
        .map_err(|err| format!("capture io_i16: {err}"))?;
    let mut stereo = [0i16; PERIOD_FRAMES * 2];
    let mut block = [0f32; RAW_SAMPLES];
    let mut filled = 0usize;
    let mut frame_idx: u32 = 0;
    let mut overruns = 0u64;
    let mut reported = 0u64;
    loop {
        match alsa_stream::transfer(
            &pcm,
            &io,
            Op::Read,
            &mut stereo,
            PERIOD_FRAMES,
            stop,
            &mut overruns,
        ) {
            Ok(()) => {}
            Err(StreamError::Cancelled) => return Ok(()),
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
            let message = format!("capture overrun recovered (total {overruns})");
            shared.set_last_error(&message);
            eprintln!("mozart-pre: {message}");
        }

        // 攒块：15 × 64 = 960 样本。
        fl_to_mono(&stereo, &mut block[filled..filled + PERIOD_FRAMES]);
        filled += PERIOD_FRAMES;
        if filled < RAW_SAMPLES {
            continue;
        }
        filled = 0;
        // packed 字段按值构造；frame_idx 回填 Task 3/4 的 DSP/VAD 前先置零。
        let frame = RawFrame {
            meta: FrameMeta {
                pts_ns: now_ns(),
                frame_idx,
                vad_flag: 0,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm: block,
        };
        ring.push(frame);
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
