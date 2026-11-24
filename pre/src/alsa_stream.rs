//! ALSA 非阻塞流：设备打开配置与有期限传输（旧 pcm_io.h 语义）。
//! 传输循环：`EAGAIN`/`EINTR` 每 2 ms 重试；`EPIPE` 计数并 prepare；
//! `ESTRPIPE` 走 resume；1 s 无进展返回 `NoProgress`；停止请求返回 `Cancelled`。
//! 实时线程约束：稳态路径无堆分配；错误分支的字符串构造只在降级时发生。

use crate::aec_probe;
use alsa::pcm::{Access, Format, HwParams, TstampType, IO, PCM};
use alsa::{Direction, ValueOr};
use mozart_core::RAW_SAMPLE_RATE;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// 采集/播放共用周期：64 帧 @ 48 kHz = 1.33 ms（旧 main.c:415，≤10 ms 端到端余量）。
pub const PERIOD_FRAMES: usize = 64;
/// 硬件缓冲周期数（旧 n_periods=4）。
pub const N_PERIODS: usize = 4;
/// 非阻塞重试间隔（旧 MOZART_PCM_RETRY_NS = 2 ms）。
const RETRY: Duration = Duration::from_millis(2);
/// 无进展上限（旧 MOZART_PCM_NO_PROGRESS_NS = 1 s）。
const NO_PROGRESS: Duration = Duration::from_secs(1);

/// 流错误：`Cancelled`、`Reopen` 与 `NoProgress` 由调用方按停止/切设备/降级处理。
#[derive(Debug)]
pub enum StreamError {
    Cancelled,
    Reopen,
    NoProgress,
    Fatal(String),
}

/// 为 ALSA 步骤附加上下文，失败信息可直接进日志与状态。
fn step<T>(context: &str, result: alsa::Result<T>) -> Result<T, String> {
    result.map_err(|err| format!("{context}: {err}"))
}

/// 采播共用的目标硬件缓冲样本数：基础周期余量外加两帧契约帧（`period = 64` 时为
/// `2176` ≈ `45.33 ms`），吸收采集侧调度抖动与播放侧 20 ms 等帧预算防 XRUN。
pub(crate) fn target_buffer_frames(period: usize) -> usize {
    period * N_PERIODS + mozart_core::RAW_SAMPLES * 2
}

/// 播放起播阈值：攒满两帧契约帧（40 ms）再开播，受实际硬件缓冲夹紧；
/// 避免 EPIPE 恢复或热切播放设备后仅写 1 帧（20 ms）即起播，吃不到额外缓冲余量。
pub(crate) fn playback_start_threshold(buffer: usize) -> usize {
    (mozart_core::RAW_SAMPLES * 2).min(buffer)
}

/// 打开并配置设备：S16_LE、2 ch、48 kHz、RW 交错、`SND_PCM_NONBLOCK`。
/// 采播缓冲均请求 `target_buffer_frames(period)`；采集 `start_threshold = 1`，
/// 播放 `start_threshold = playback_start_threshold(actual_buffer)`。
pub fn open(device: &str, dir: Direction, timestamp: bool) -> Result<PCM, String> {
    // nonblock=true 映射 SND_PCM_NONBLOCK。
    let pcm = step("open", PCM::new(device, dir, true))?;
    let hw = step("hw_params_any", HwParams::any(&pcm))?;
    step("set_access", hw.set_access(Access::RWInterleaved))?;
    step("set_format", hw.set_format(Format::s16()))?;
    step("set_channels", hw.set_channels(2))?;
    let rate = step(
        "set_rate_near",
        hw.set_rate_near(RAW_SAMPLE_RATE, ValueOr::Nearest),
    )?;
    if rate != RAW_SAMPLE_RATE {
        return Err(format!(
            "set_rate_near: device gave {rate} Hz, want {RAW_SAMPLE_RATE}"
        ));
    }
    let period = step(
        "set_period_size_near",
        hw.set_period_size_near(PERIOD_FRAMES as i64, ValueOr::Nearest),
    )? as usize;
    let buffer = target_buffer_frames(period);
    let actual_buffer = step(
        "set_buffer_size_near",
        hw.set_buffer_size_near(buffer as i64),
    )? as usize;
    step("hw_params", pcm.hw_params(&hw))?;

    let sw = step("sw_params_current", pcm.sw_params_current())?;
    let start_threshold = match dir {
        Direction::Capture => 1,
        Direction::Playback => playback_start_threshold(actual_buffer),
    };
    step(
        "set_start_threshold",
        sw.set_start_threshold(start_threshold as i64),
    )?;
    if timestamp {
        step("set_tstamp_mode", sw.set_tstamp_mode(true))?;
        step("set_tstamp_type", sw.set_tstamp_type(TstampType::Monotonic))?;
    }
    step("sw_params", pcm.sw_params(&sw))?;
    drop(sw);

    step("prepare", pcm.prepare())?;
    drop(hw);
    Ok(pcm)
}

pub fn log_stream_setup(role: &str, device: &str, pcm: &PCM) {
    match pcm.get_params() {
        Ok((buffer, period)) => {
            eprintln!(
                "mozart-pre: {role} device {device} ready (period {period}, buffer {buffer})"
            );
        }
        Err(err) => eprintln!("mozart-pre: {role} device {device} ready (params unknown: {err})"),
    }
}

pub fn open_and_verify_io(
    role: &str,
    device: &str,
    dir: Direction,
    timestamp: bool,
) -> Result<PCM, String> {
    let pcm = open(device, dir, timestamp)?;
    pcm.io_i16()
        .map_err(|err| format!("{role} io_i16: {err}"))?;
    log_stream_setup(role, device, &pcm);
    Ok(pcm)
}

/// 传输方向。
#[derive(Clone, Copy)]
pub enum Op {
    Read,
    Write,
}

/// 传输一整块交错立体声样本；`buf` 长度必须为 `frames × 2`。
/// 部分进度从断点续传；成功返回时 `frames` 帧全部完成。
pub fn transfer(
    pcm: &PCM,
    io: &IO<i16>,
    op: Op,
    buf: &mut [i16],
    frames: usize,
    stop: &AtomicBool,
    reopen: &AtomicBool,
    xruns: &mut u64,
    mut probe: Option<&mut aec_probe::Stream>,
) -> Result<(), StreamError> {
    let mut deadline = Instant::now() + NO_PROGRESS;
    let mut done = 0usize;
    let mut suspended = false;
    while done < frames {
        if stop.load(Ordering::Relaxed) {
            return Err(StreamError::Cancelled);
        }
        if reopen.load(Ordering::Acquire) {
            return Err(StreamError::Reopen);
        }
        if Instant::now() >= deadline {
            return Err(StreamError::NoProgress);
        }
        if suspended {
            match pcm.resume() {
                Ok(()) => suspended = false,
                // resume 未就绪（仍挂起）：等下一轮再试（旧 pcm_io.h 循环）。
                Err(err) if err.errno() == libc::EAGAIN => {}
                // resume 报错：按旧语义改走 prepare 恢复。
                Err(_) => {
                    step("prepare", pcm.prepare()).map_err(StreamError::Fatal)?;
                    suspended = false;
                }
            }
        } else {
            let chunk = &mut buf[done * 2..frames * 2];
            let result = match op {
                Op::Read => io.readi(chunk),
                Op::Write => io.writei(chunk),
            };
            match result {
                Ok(0) => {} // 无进展：不重置期限，等 2 ms 再试。
                Ok(n) => {
                    if let Some(probe) = &mut probe {
                        probe.observe(pcm, &chunk[..n * 2]);
                    }
                    done += n;
                    deadline = Instant::now() + NO_PROGRESS;
                    if done == frames {
                        return Ok(());
                    }
                    continue; // 有部分进度：立即续传，不睡眠。
                }
                Err(err) => match err.errno() {
                    libc::EPIPE => {
                        if let Some(probe) = &mut probe {
                            probe.discontinuity();
                        }
                        *xruns += 1;
                        step("prepare", pcm.prepare()).map_err(StreamError::Fatal)?;
                    }
                    libc::ESTRPIPE => {
                        if let Some(probe) = &mut probe {
                            probe.discontinuity();
                        }
                        suspended = true;
                    }
                    libc::EAGAIN | libc::EINTR => {}
                    _ => return Err(StreamError::Fatal(format!("transfer: {err}"))),
                },
            }
        }
        thread::sleep(RETRY);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_buffer_covers_two_contract_frames_plus_periods() {
        let buffer = target_buffer_frames(PERIOD_FRAMES);
        assert_eq!(buffer, 2176);
        assert_eq!(buffer % PERIOD_FRAMES, 0);
        assert_eq!(playback_start_threshold(buffer), mozart_core::RAW_SAMPLES * 2);
        assert_eq!(playback_start_threshold(1024), 1024);
    }
}
