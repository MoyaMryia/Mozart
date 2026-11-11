//! ALSA 非阻塞流：设备打开配置与有期限传输（旧 pcm_io.h 语义）。
//! 传输循环：`EAGAIN`/`EINTR` 每 2 ms 重试；`EPIPE` 计数并 prepare；
//! `ESTRPIPE` 走 resume；1 s 无进展返回 `NoProgress`；停止请求返回 `Cancelled`。
//! 实时线程约束：稳态路径无堆分配；错误分支的字符串构造只在降级时发生。

use alsa::pcm::{Access, Format, HwParams, IO, PCM};
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

/// 流错误：`Cancelled` 与 `NoProgress` 由调用方按停止/降级处理。
#[derive(Debug)]
pub enum StreamError {
    Cancelled,
    NoProgress,
    Fatal(String),
}

/// 为 ALSA 步骤附加上下文，失败信息可直接进日志与状态。
fn step<T>(context: &str, result: alsa::Result<T>) -> Result<T, String> {
    result.map_err(|err| format!("{context}: {err}"))
}

/// 打开并配置设备：S16_LE、2 ch、48 kHz、RW 交错、`SND_PCM_NONBLOCK`、
/// start_threshold=1（旧 capture.c / playback.c）。
/// 采集缓冲 = period × 4；播放缓冲 = max(period × 4, period + 960)，
/// 后者保证一整帧 960 一次写入可容纳（旧 playback.c:74-76）。
pub fn open(device: &str, dir: Direction) -> Result<PCM, String> {
    // nonblock=true 映射 SND_PCM_NONBLOCK。
    let pcm = step("open", PCM::new(device, dir, true))?;
    let hw = step("hw_params_any", HwParams::any(&pcm))?;
    step("set_access", hw.set_access(Access::RWInterleaved))?;
    step("set_format", hw.set_format(Format::s16()))?;
    step("set_channels", hw.set_channels(2))?;
    let rate = step("set_rate_near", hw.set_rate_near(RAW_SAMPLE_RATE, ValueOr::Nearest))?;
    if rate != RAW_SAMPLE_RATE {
        return Err(format!(
            "set_rate_near: device gave {rate} Hz, want {RAW_SAMPLE_RATE}"
        ));
    }
    let period = step(
        "set_period_size_near",
        hw.set_period_size_near(PERIOD_FRAMES as i64, ValueOr::Nearest),
    )? as usize;
    let buffer = match dir {
        Direction::Capture => period * N_PERIODS,
        Direction::Playback => (period * N_PERIODS).max(period + mozart_core::RAW_SAMPLES),
    };
    step("set_buffer_size_near", hw.set_buffer_size_near(buffer as i64))?;
    step("hw_params", pcm.hw_params(&hw))?;

    let sw = step("sw_params_current", pcm.sw_params_current())?;
    step("set_start_threshold", sw.set_start_threshold(1))?;
    step("sw_params", pcm.sw_params(&sw))?;
    drop(sw);

    step("prepare", pcm.prepare())?;
    drop(hw);
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
    xruns: &mut u64,
) -> Result<(), StreamError> {
    let mut deadline = Instant::now() + NO_PROGRESS;
    let mut done = 0usize;
    let mut suspended = false;
    while done < frames {
        if stop.load(Ordering::Relaxed) {
            return Err(StreamError::Cancelled);
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
                    done += n;
                    deadline = Instant::now() + NO_PROGRESS;
                    if done == frames {
                        return Ok(());
                    }
                    continue; // 有部分进度：立即续传，不睡眠。
                }
                Err(err) => match err.errno() {
                    libc::EPIPE => {
                        *xruns += 1;
                        step("prepare", pcm.prepare()).map_err(StreamError::Fatal)?;
                    }
                    libc::ESTRPIPE => suspended = true,
                    libc::EAGAIN | libc::EINTR => {}
                    _ => return Err(StreamError::Fatal(format!("transfer: {err}"))),
                },
            }
        }
        thread::sleep(RETRY);
    }
    Ok(())
}
