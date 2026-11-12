//! mozart-tts：播报合成进程（SPEC-05）。
//! 线程结构：jobs（tts-jobs.sock SEQPACKET 收任务 → 有界队列）→ synth（PocketTTS
//! 合成 + 24 kHz→48 kHz 重采样 + 契约帧输出 tts-out.sock）；控制线程独立服务 tts.sock。
//! 计数全部原子，控制只读。v1 不走变声：pre 绑定 tts-out.sock 收帧播放，TTS 连接
//! 发送，惰性建链，pre 未就绪时发送丢弃并计数。
//! 停止：SIGTERM/SIGINT 置停止标志；放弃在途合成与队列剩余，在 daemon 2 s 宽限内退出。

mod control;
mod ffi;
mod jobs;
mod output;
mod resample;
mod state;
mod synth;

use clap::Parser;
use mozart_core::config::Config;
use mozart_core::ctl::Seqpacket;
use state::Shared;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

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

#[derive(Parser)]
#[command(name = "mozart-tts", version, about = "Mozart 播报合成")]
struct Cli {
    /// 控制面 socket 路径（daemon 传入）
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    /// 配置文件；缺省按 MOZART_CONFIG、./mozart.toml 顺序查找
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

/// SEQPACKET 绑定：清理陈旧文件后绑定（进程重启无需外部清理）。
fn bind_socket(path: &Path) -> Result<Seqpacket, String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("清理陈旧 socket {}：{err}", path.display())),
    }
    Seqpacket::bind(path).map_err(|err| format!("绑定 {}：{err}", path.display()))
}

/// 线程致命错误传播：返回 Err 前置停止标志，其余线程据此有序退出。
fn stop_on_fatal(result: Result<(), String>) -> Result<(), String> {
    if result.is_err() {
        STOP.store(true, Ordering::Relaxed);
    }
    result
}

/// 把 panic 转成 Err，走与致命错误同一条传播链（避免半死进程）。
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

fn spawn_thread<F>(name: &str, body: F) -> std::io::Result<JoinHandle<Result<(), String>>>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    std::thread::Builder::new().name(name.to_string()).spawn(body)
}

/// 依序回收线程；任一线程 Err/panic 置停止标志并记录首个失败。
fn join_threads(handles: Vec<(&'static str, JoinHandle<Result<(), String>>)>) -> Option<String> {
    let mut failure = None;
    for (name, handle) in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                STOP.store(true, Ordering::Relaxed);
                failure = failure.or(Some(format!("{name}: {message}")));
            }
            Err(_) => {
                STOP.store(true, Ordering::Relaxed);
                failure = failure.or(Some(format!("{name}: panicked")));
            }
        }
    }
    failure
}

fn run(cli: Cli) -> Result<(), String> {
    // 入口校验一次：[tts] 配置合法性在 Config::load 内完成。
    let config = Config::load(cli.config.as_deref()).map_err(|err| err.to_string())?;
    install_signal_handlers();

    let shared = Arc::new(Shared::new());
    let listener = bind_socket(&cli.socket)?;
    listener
        .listen(4)
        .map_err(|err| format!("listen：{err}"))?;
    let jobs_listener = bind_socket(&config.tts_jobs_socket())?;
    jobs_listener
        .listen(4)
        .map_err(|err| format!("listen：{err}"))?;
    // 输出侧惰性建链：pre 绑定 tts-out.sock，TTS 连接发送；pre 未就绪不阻塞启动。
    let mut writer = output::FrameWriter::new(config.tts_output_socket(), shared.clone());

    // 参考 WAV 启动时加载；未配置则无参考音频。
    let reference = if config.tts.reference_wav.as_os_str().is_empty() {
        None
    } else {
        Some(synth::load_reference_wav(&config.tts.reference_wav)?)
    };

    // 容量已由配置校验保证 ≥ 1。
    let (tasks_tx, tasks_rx) = mpsc::sync_channel::<jobs::Job>(config.tts.queue_capacity as usize);

    eprintln!(
        "mozart-tts: jobs={} output={} queue_capacity={} reference={}",
        config.tts_jobs_socket().display(),
        config.tts_output_socket().display(),
        config.tts.queue_capacity,
        config.tts.reference_wav.display(),
    );

    let jobs_handle = {
        let shared = shared.clone();
        spawn_thread("mozart-tts-jobs", move || {
            guard_panic(move || {
                jobs::serve(jobs_listener, tasks_tx, shared, &STOP);
                Ok(())
            })
        })
        .map_err(|err| format!("启动任务线程：{err}"))?
    };
    let synth_handle = {
        let shared = shared.clone();
        let model_dir = config.tts.model_dir.clone();
        let threads = config.tts.threads;
        let provider = config.tts.provider.clone();
        spawn_thread("mozart-tts-synth", move || {
            stop_on_fatal(guard_panic(move || {
                synth::run(
                    tasks_rx, model_dir, threads, provider, reference, &mut writer, &shared, &STOP,
                )
            }))
        })
        .map_err(|err| format!("启动合成线程：{err}"))?
    };
    let control_handle = {
        let shared = shared.clone();
        spawn_thread("mozart-tts-control", move || {
            guard_panic(move || {
                control::serve(listener, shared, &STOP);
                Ok(())
            })
        })
        .map_err(|err| format!("启动控制线程：{err}"))?
    };

    match join_threads(vec![
        ("jobs", jobs_handle),
        ("synth", synth_handle),
        ("control", control_handle),
    ]) {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mozart-tts: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_contract_matches_supervisor() {
        let cli = Cli::try_parse_from(["mozart-tts", "--socket", "/run/mozart/tts.sock"]).unwrap();
        assert_eq!(cli.socket, PathBuf::from("/run/mozart/tts.sock"));
        assert!(cli.config.is_none());

        let cli = Cli::try_parse_from([
            "mozart-tts",
            "--socket",
            "tts.sock",
            "--config",
            "mozart.toml",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("mozart.toml")));

        assert!(Cli::try_parse_from(["mozart-tts"]).is_err());
        assert!(Cli::try_parse_from(["mozart-tts", "--socket"]).is_err());
        assert!(Cli::try_parse_from(["mozart-tts", "--socket", "s", "--bogus"]).is_err());
    }
}