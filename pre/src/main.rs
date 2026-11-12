//! mozart-pre：音频前端进程（SPEC-02 / SPEC-05）。
//! 线程结构：采集（ALSA 攒块→环）→ 处理（DSP 全链：80 Hz 高通 + WOLA 谱降噪
//! + gate/AGC/limiter → 主干环；主干同帧打包 16 kHz InputFrame 恒定发送到
//! rvc 与 asr input_socket）→ 播放（扬声器分时：播报优先，空窗 holdoff 恢复
//! 实时；实时按路由/静音状态机→淡变→ALSA 写，rvc 路由取 AI 接收环，direct
//! 路由排空计数）；接收线程独立收 rvc 输出帧入 AI 环、TTS 播报帧入播报环；
//! 控制线程独立服务 SEQPACKET。
//! 发送恒定不停（旧 main.c L11「AI 路径永不停」）：路由只决定播放源，
//! 不停发输入帧。发送侧惰性建链，pre 启动不依赖 rvc/asr 生命周期。
//! 停止：SIGTERM/SIGINT 置停止标志，各线程在时限内退出；设备打开失败立即退出，
//! daemon 按退避重启（SPEC-02：换设备不做在线命令，由重启代替）。

mod ai;
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
use mozart_core::dgram::FrameSocket;
use mozart_core::ring::Ring;
use mozart_core::{OutputFrame, RawFrame};
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

/// 处理线程：采集环 → DSP 全链 → 主干环 + 16 kHz 打包恒定发送。
/// DSP 在采集环之后、direct 分支之前——direct 取的 48 kHz 主干含 DSP，
/// 与旧实现一致。发送不随路由停发（旧 main.c L11）；同一 InputFrame 恒发到
/// rvc 与 asr 两条输入支路，各自惰性建链、独立丢弃计数。
fn process_thread(
    capture_ring: Arc<Ring<RawFrame>>,
    trunk: Arc<Ring<RawFrame>>,
    shared: Arc<Shared>,
    mut ai_sender: ai::Sender,
    mut asr_sender: ai::AsrSender,
    denoise: bool,
    gate: bool,
    agc: bool,
    limiter: bool,
) -> Result<(), String> {
    // DSP 状态为定长数组，构造后热路径零分配。
    let mut dsp = dsp::Dsp::new(denoise, gate, agc, limiter);
    while !STOP.load(Ordering::Relaxed) {
        if let Some(mut frame) = capture_ring.pop(Some(PROCESS_POLL)) {
            let conf = dsp.process_frame(&mut frame);
            let input_frame = dsp.pack_frame(&frame, conf);
            trunk.push(frame);
            ai_sender.send(&input_frame, &shared);
            asr_sender.send(&input_frame, &shared);
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

/// 设备线程 panic 传播根治：catch_unwind 把 panic 转成 Err，与致命设备
/// 错误走同一条传播链（stop_on_fatal 置停止标志，join 侧收到 Ok(Err)）。
/// 不加这一层时，panic 线程的停止标志要等 main join 到它才补置，而 main
/// 可能正阻塞在前面健康线程的 join 上，进程半死。闭包经 AssertUnwindSafe
/// 包装：线程退出路径不需要 unwind 一致性保证，正常路径行为不变。
fn guard_panic<F>(body: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => Err(payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panicked".to_string())),
    }
}

/// 依序回收线程。线程返回 Err（致命设备错误、无进展降级）或 panic 时
/// 置停止标志，让其余线程在时限内退出，主线程得以收尾并失败退出；
/// daemon 按退避重启（SPEC-02：换设备由重启代替）。
/// 设备线程返回 Err 前已自置标志（stop_on_fatal）；panic 由 guard_panic
/// 转成 Err 走同一条传播链，不再依赖 main join 到该线程才补置；
/// 此处幂等补置兜底。
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
                // 线程 panic 兜底（guard_panic 之外的路径，如控制线程）：
                // 置停止标志让其余线程退出后一并回收。
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
    let rvc_ring = Arc::new(Ring::<OutputFrame>::new(ai::RVC_QUEUE_FRAMES));
    let tts_ring = Arc::new(Ring::<OutputFrame>::new(ai::TTS_QUEUE_FRAMES));

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

    // AI 支路输出侧：pre 绑定 output_socket 收 rvc 输出帧（SPEC-02 方向约定）。
    // 绑定失败（run_dir 缺失等）立即退出：部署路径属配置错误。
    let rvc_out_socket = match FrameSocket::<OutputFrame>::bind(
        &config.rvc_output_socket(),
        ai::RVC_QUEUE_FRAMES,
    ) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!(
                "mozart-pre: bind {}: {err}",
                config.rvc_output_socket().display()
            );
            return ExitCode::FAILURE;
        }
    };
    // AI 支路输入侧：连接 rvc input_socket，对端未就绪时发送丢弃计数
    //（惰性建链，不阻塞启动）。
    let ai_sender = ai::Sender::new(config.rvc_input_socket());
    // 识别支路输入侧：连接 asr input_socket，与 rvc 同帧恒发；asr 未起时
    // 发送丢弃计数，不阻塞 pre（SPEC-05）。
    let asr_sender = ai::AsrSender::new(config.asr_input_socket());

    // 播报支路输出侧：pre 绑定 tts_output_socket 收 TTS 播报帧（SPEC-05 方向
    // 约定：pre 绑定、TTS 连接）。绑定失败立即退出：部署路径属配置错误。
    let tts_out_socket = match FrameSocket::<OutputFrame>::bind(
        &config.tts_output_socket(),
        ai::TTS_QUEUE_FRAMES,
    ) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!(
                "mozart-pre: bind {}: {err}",
                config.tts_output_socket().display()
            );
            return ExitCode::FAILURE;
        }
    };

    // 设备先就绪，后拉线程（新实现次序：采集设备先于播放；旧实现 main.c
    // 播放设备先就绪，本实现两设备在拉线程前同步打开，次序差异无行为影响）。
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
    eprintln!(
        "mozart-pre: ai link out={}",
        config.rvc_output_socket().display()
    );

    // 采集与处理线程；播放线程持有状态机；接收线程独立收 rvc 输出帧；
    // 控制线程独立服务。
    let capture_handle = {
        let ring = capture_ring.clone();
        let shared = shared.clone();
        match spawn_thread("mozart-capture", move || {
            stop_on_fatal(
                &STOP,
                guard_panic(move || capture::run(capture_pcm, ring, shared, &STOP)),
            )
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("capture", err),
        }
    };
    let control_circles = (capture_ring.clone(), trunk.clone(), rvc_ring.clone(), tts_ring.clone());
    let process_handle = {
        let from = capture_ring.clone();
        let to = trunk.clone();
        let shared = shared.clone();
        let denoise = config.pre.dsp.denoise;
        let gate = config.pre.dsp.gate;
        let agc = config.pre.dsp.agc;
        let limiter = config.pre.dsp.limiter;
        match spawn_thread("mozart-trunk", move || {
            process_thread(from, to, shared, ai_sender, asr_sender, denoise, gate, agc, limiter)
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("process", err),
        }
    };
    let rvc_recv_handle = {
        let shared = shared.clone();
        let ring = rvc_ring.clone();
        match spawn_thread("mozart-rvc-recv", move || {
            stop_on_fatal(
                &STOP,
                guard_panic(move || ai::recv_loop(rvc_out_socket, ring, shared, &STOP)),
            )
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("rvc-recv", err),
        }
    };
    let tts_recv_handle = {
        let shared = shared.clone();
        let ring = tts_ring.clone();
        match spawn_thread("mozart-tts-recv", move || {
            stop_on_fatal(
                &STOP,
                guard_panic(move || ai::tts_recv_loop(tts_out_socket, ring, shared, &STOP)),
            )
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("tts-recv", err),
        }
    };
    let playback_handle = {
        let shared = shared.clone();
        match spawn_thread("mozart-playback", move || {
            stop_on_fatal(
                &STOP,
                guard_panic(move || {
                    playback::run(playback_pcm, trunk, rvc_ring, tts_ring, shared, &STOP)
                }),
            )
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("playback", err),
        }
    };
    let control_handle = {
        let shared = shared.clone();
        let (capture_ring, trunk, rvc_ring, tts_ring) = control_circles;
        match spawn_thread("mozart-control", move || {
            control::serve(listener, shared, capture_ring, trunk, rvc_ring, tts_ring, &STOP);
            Ok(())
        }) {
            Ok(handle) => handle,
            Err(err) => return spawn_failure("control", err),
        }
    };

    let failure = join_threads(vec![
        ("capture", capture_handle),
        ("process", process_handle),
        ("rvc-recv", rvc_recv_handle),
        ("tts-recv", tts_recv_handle),
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

    /// 本模块两条用例都操纵全局 STOP，用互斥锁串行：并行置位会让
    /// "正常退出不得置停止标志"的断言观察到对方线程的置位，必然竞态失败。
    static STOP_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 线程返回 Err（致命设备错误、无进展降级）必须置停止标志，其余线程据此
    /// 退出、主线程得以收尾（审查修正点）；正常退出不得置。
    #[test]
    fn thread_fatal_error_sets_stop_flag() {
        let _guard = STOP_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// 设备线程 panic 必须转成 Err 并由传播链置停止标志（审查根治点）：
    /// 不加 catch_unwind 时，main 要 join 到 panic 线程才补置停止标志，
    /// 而它可能正阻塞在前面健康线程的 join 上，进程半死。
    #[test]
    fn device_thread_panic_sets_stop_flag_before_join() {
        let _guard = STOP_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // panic → guard_panic 捕获为 Err → stop_on_fatal 置停止标志。
        // 模拟 main 的包装次序：spawn + stop_on_fatal(guard_panic(body))。
        STOP.store(false, Ordering::Relaxed);
        let handle = std::thread::spawn(|| {
            stop_on_fatal(
                &STOP,
                guard_panic(|| panic!("device exploded")),
            )
        });
        // 主线程此刻可以先收尾其他线程的 Err 再 join 本线程：停止标志在线程
        // 退出时已置，不依赖 join 结果。
        let result = handle.join().expect("guard_panic 不让 panic 外逸");
        assert!(result.is_err(), "panic 必须转成 Err");
        assert!(
            STOP.load(Ordering::Relaxed),
            "设备线程 panic 后、main join 到它之前，停止标志必须已置"
        );
        STOP.store(false, Ordering::Relaxed);

        // join 侧收到 Ok(Err(panic 消息))：走既有 Err 传播链（补置幂等）。
        let handle = std::thread::spawn(|| {
            stop_on_fatal(
                &STOP,
                guard_panic(|| panic!("with payload {}", 42)),
            )
        });
        let joined = join_threads(vec![("panic", handle)]);
        assert_eq!(joined, Some("panic: with payload 42".to_string()));
        assert!(STOP.load(Ordering::Relaxed), "join 侧 Err 补置停止标志");
        STOP.store(false, Ordering::Relaxed);

        // 非 panic 路径不变：正常与 Err 语义保持原样。
        assert!(guard_panic(|| Ok(())).is_ok());
        assert_eq!(guard_panic(|| Err("fatal".to_string())), Err("fatal".to_string()));
        assert!(!STOP.load(Ordering::Relaxed), "guard_panic 不改变正常路径");

        // 无法 downcast 的 payload（如非字符串）降级为通用消息。
        let handle = std::thread::spawn(|| {
            stop_on_fatal(
                &STOP,
                guard_panic(|| std::panic::panic_any(7_i32)),
            )
        });
        assert_eq!(join_threads(vec![("opaque", handle)]), Some("opaque: panicked".to_string()));
        assert!(STOP.load(Ordering::Relaxed));
        STOP.store(false, Ordering::Relaxed);
    }
}
