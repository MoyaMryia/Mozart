//! mozart-pre：音频前端进程（SPEC-02）。
//! 线程结构：采集（ALSA 攒块→环）→ 处理（DSP：80 Hz 高通 + WOLA 谱降噪；
//! gate/AGC/limiter 由 Task 4 插入）→ 播放（路由/静音状态机→淡变→ALSA 写）；
//! 控制线程独立服务 SEQPACKET。
//! 停止：SIGTERM/SIGINT 置停止标志，各线程在时限内退出；设备打开失败立即退出，
//! daemon 按退避重启（SPEC-02：换设备不做在线命令，由重启代替）。

mod alsa_stream;
mod capture;
mod control;
mod dsp;
mod playback;
mod route;
mod state;

use alsa::Direction;
use mozart_core::config::Config;
use mozart_core::ctl::Seqpacket;
use mozart_core::ring::Ring;
use mozart_core::RawFrame;
use state::Shared;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// 主干环（处理 → 播放）容量：8 帧，与采集环默认同量级；未配置化（YAGNI，Task 3/4 复核）。
const TRUNK_QUEUE_FRAMES: usize = 8;
/// 处理线程出队等待步长：超时后检查停止标志。
const PROCESS_POLL: Duration = Duration::from_millis(100);

/// 停止标志：信号处理器与各线程共用。
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    // 只做原子存储，异步信号安全。
    STOP.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    // SAFETY: handler 只做原子存储；signal 其余语义不受影响。
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}

struct Args {
    socket: PathBuf,
    config: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut socket = None;
    let mut config = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(
                    args.next().ok_or("--socket needs a path")?,
                ));
            }
            "--config" => {
                config = Some(PathBuf::from(
                    args.next().ok_or("--config needs a path")?,
                ));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        socket: socket.ok_or("missing required argument: --socket <path>")?,
        config,
    })
}

/// 控制面 socket 绑定：清理陈旧文件后绑定（进程重启无需外部清理）。
fn bind_control_socket(path: &Path) -> Result<Seqpacket, String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("remove stale socket {}: {err}", path.display())),
    }
    Seqpacket::bind(path).map_err(|err| format!("bind {}: {err}", path.display()))
}

/// 处理线程：采集环 → 主干环。DSP 阶段（本任务：80 Hz 高通 + WOLA 谱降噪）
/// 在采集环之后、direct 分支之前——direct 取的 48 kHz 主干含 DSP，与旧实现
/// 一致。置信度返回值由 Task 4 的 VAD/打包消费。
fn process_thread(
    capture_ring: Arc<Ring<RawFrame>>,
    trunk: Arc<Ring<RawFrame>>,
    denoise: bool,
) -> Result<(), String> {
    // DSP 状态为定长数组，构造后热路径零分配。
    let mut dsp = dsp::Dsp::new(denoise);
    while !STOP.load(Ordering::Relaxed) {
        if let Some(mut frame) = capture_ring.pop(Some(PROCESS_POLL)) {
            dsp.process_frame(&mut frame);
            trunk.push(frame);
        }
    }
    Ok(())
}

/// 拉起具名线程；失败由调用方退出（先拉的线程随进程退出回收）。
fn spawn_thread<F>(name: &str, body: F) -> std::io::Result<std::thread::JoinHandle<Result<(), String>>>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    std::thread::Builder::new().name(name.to_string()).spawn(body)
}

/// 设备线程致命错误传播（审查修正点）：返回 Err 前先置停止标志。
/// main 按固定顺序 join；只靠 main 在 join 结果里补置时，排在死亡线程
/// 之前的健康线程（如 playback 先死时的 capture）不会退出，main 永远
/// 阻塞在它的 join 上。设备线程必须自己传播停止，其余线程才能有序退出。
fn stop_on_fatal(stop: &AtomicBool, result: Result<(), String>) -> Result<(), String> {
    if result.is_err() {
        stop.store(true, Ordering::Relaxed);
    }
    result
}

/// 依序回收线程。线程返回 Err（致命设备错误、无进展降级）或 panic 时
/// 置停止标志，让其余线程在时限内退出，主线程得以收尾并失败退出；
/// daemon 按退避重启（SPEC-02：换设备由重启代替）。
/// 设备线程返回 Err 前已自置标志（stop_on_fatal）；此处幂等补置兜底。
fn join_threads(handles: Vec<(&'static str, JoinHandle<Result<(), String>>)>) -> Option<String> {
    let mut failure = None;
    for (name, handle) in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                // 线程致命错误：同样置停止标志，否则其余线程永不退出，进程挂起。
                STOP.store(true, Ordering::Relaxed);
                failure = failure.or(Some(format!("{name}: {message}")));
            }
            Err(_) => {
                // 线程 panic：置停止标志让其余线程退出后一并回收。
                STOP.store(true, Ordering::Relaxed);
                failure = failure.or(Some(format!("{name}: panicked")));
            }
        }
    }
    failure
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("mozart-pre: {message}");
            eprintln!("usage: mozart-pre --socket <path> [--config <path>]");
            return ExitCode::from(2);
        }
    };
    // 入口校验一次：[pre] 设备名与队列容量在 Config::load 内完成。
    let config = match Config::load(args.config.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("mozart-pre: {err}");
            return ExitCode::FAILURE;
        }
    };

    install_signal_handlers();

    let shared = Arc::new(Shared::new(
        config.pre.capture_device.clone(),
        config.pre.playback_device.clone(),
    ));
    // 容量已由配置校验保证 ≥ 1；Ring::new 对 0 会 panic。
    let capture_ring = Arc::new(Ring::<RawFrame>::new(
        config.pre.capture_queue_frames as usize,
    ));
    let trunk = Arc::new(Ring::<RawFrame>::new(TRUNK_QUEUE_FRAMES));

    let listener = match bind_control_socket(&args.socket) {
        Ok(listener) => listener,
        Err(message) => {
            eprintln!("mozart-pre: {message}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = listener.listen(4) {
        eprintln!("mozart-pre: listen: {err}");
        return ExitCode::FAILURE;
    }

    // 设备先就绪，后拉线程（旧实现次序：播放设备先于采集）。
    let capture_pcm = match alsa_stream::open(&config.pre.capture_device, Direction::Capture) {
        Ok(pcm) => pcm,
        Err(message) => {
            eprintln!("mozart-pre: capture open: {message}");
            return ExitCode::FAILURE;
        }
    };
    let playback_pcm = match alsa_stream::open(&config.pre.playback_device, Direction::Playback) {
        Ok(pcm) => pcm,
        Err(message) => {
            eprintln!("mozart-pre: playback open: {message}");
            return ExitCode::FAILURE;
        }
    };
    log_stream_setup("capture", &config.pre.capture_device, &capture_pcm);
    log_stream_setup("playback", &config.pre.playback_device, &playback_pcm);

    // 采集与处理线程；播放线程持有状态机；控制线程独立服务。
    let capture_handle = {
        let ring = capture_ring.clone();
        let shared = shared.clone();
        match spawn_thread("mozart-capture", move || {
            stop_on_fatal(&STOP, capture::run(capture_pcm, ring, shared, &STOP))
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("capture", err),
        }
    };
    let control_circles = (capture_ring.clone(), trunk.clone());
    let process_handle = {
        let from = capture_ring.clone();
        let to = trunk.clone();
        let denoise = config.pre.dsp.denoise;
        match spawn_thread("mozart-trunk", move || process_thread(from, to, denoise)) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("process", err),
        }
    };
    let playback_handle = {
        let shared = shared.clone();
        match spawn_thread("mozart-playback", move || {
            stop_on_fatal(&STOP, playback::run(playback_pcm, trunk, shared, &STOP))
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("playback", err),
        }
    };
    let control_handle = {
        let shared = shared.clone();
        let (capture_ring, trunk) = control_circles;
        match spawn_thread("mozart-control", move || {
            control::serve(listener, shared, capture_ring, trunk, &STOP);
            Ok(())
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("control", err),
        }
    };

    let failure = join_threads(vec![
        ("capture", capture_handle),
        ("process", process_handle),
        ("playback", playback_handle),
        ("control", control_handle),
    ]);
    match failure {
        Some(message) => {
            eprintln!("mozart-pre: {message}");
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}

fn spawn_failure(role: &str, err: std::io::Error) -> ExitCode {
    eprintln!("mozart-pre: spawn {role} thread: {err}");
    ExitCode::FAILURE
}

fn log_stream_setup(role: &str, device: &str, pcm: &alsa::pcm::PCM) {
    match pcm.get_params() {
        Ok((buffer, period)) => {
            eprintln!("mozart-pre: {role} device {device} ready (period {period}, buffer {buffer})");
        }
        Err(err) => eprintln!("mozart-pre: {role} device {device} ready (params unknown: {err})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 线程返回 Err（致命设备错误、无进展降级）必须置停止标志，其余线程据此
    /// 退出、主线程得以收尾（审查修正点）；正常退出不得置。
    /// 全局 STOP 在本 crate 测试中仅本用例使用，串行操纵不与其他用例竞争。
    #[test]
    fn thread_fatal_error_sets_stop_flag() {
        // 传播点一：设备线程返回 Err 前自置停止标志——playback 先死时 main
        // 还阻塞在 capture join，靠它让 capture 有序退出。
        STOP.store(false, Ordering::Relaxed);
        assert!(stop_on_fatal(&STOP, Err("fatal stream error".to_string())).is_err());
        assert!(STOP.load(Ordering::Relaxed), "设备线程返回 Err 前必须置停止标志");
        STOP.store(false, Ordering::Relaxed);
        assert!(stop_on_fatal(&STOP, Ok(())).is_ok());
        assert!(!STOP.load(Ordering::Relaxed), "正常退出不得置停止标志");

        // 传播点二：join 循环对 Err 同样补置（幂等兜底）；正常退出不置。
        let ok = std::thread::spawn(|| Ok::<(), String>(()));
        assert_eq!(join_threads(vec![("ok", ok)]), None);
        assert!(!STOP.load(Ordering::Relaxed), "正常退出不得置停止标志");
        let err = std::thread::spawn(|| Err::<(), String>("fatal device error".to_string()));
        assert_eq!(
            join_threads(vec![("err", err)]),
            Some("err: fatal device error".to_string())
        );
        assert!(STOP.load(Ordering::Relaxed), "线程返回 Err 必须置停止标志");
        STOP.store(false, Ordering::Relaxed);
    }
}
