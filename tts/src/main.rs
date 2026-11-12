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
use mozart_core::thread::{bind_socket, guard_panic, spawn_thread};
use state::Shared;
use std::path::PathBuf;
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

/// 线程致命错误传播：绑定本进程全局 STOP（逻辑见 core::thread）。
fn stop_on_fatal(result: Result<(), String>) -> Result<(), String> {
    mozart_core::thread::stop_on_fatal(&STOP, result)
}

/// 依序回收线程：绑定本进程全局 STOP（逻辑见 core::thread）。
fn join_threads(handles: Vec<(&'static str, JoinHandle<Result<(), String>>)>) -> Option<String> {
    mozart_core::thread::join_threads(&STOP, handles)
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