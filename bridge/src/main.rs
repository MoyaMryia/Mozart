//! mozart-bridge：字幕桥进程（SPEC-05）。
//! 线程结构：事件（连接 asr-events.sock，读循环只入队）→ 字幕（记录表合并 +
//! subtitles.jsonl 逐行落盘 + final 入待翻译队列）；控制线程独立服务 bridge.sock
//! （Hello/GetStatus + 翻译/播报开关）。翻译调度与播报投递在 Task 5 接入队列。
//! 开关以 TOML 为基线，运行时命令覆盖并存档 run_dir/bridge-state.json，重启恢复。
//! 停止：SIGTERM/SIGINT 置停止标志；停收新事件，丢弃在途队列，
//! 字幕存档已逐行 flush，在 daemon 2 s 宽限内退出。

mod control;
mod events;
mod state;
mod subtitles;

use clap::Parser;
use mozart_core::config::Config;
use mozart_core::ctl::Seqpacket;
use state::{Queues, Shared, Switches};
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
#[command(name = "mozart-bridge", version, about = "Mozart 字幕桥")]
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
    // 入口校验一次：[bridge] 等配置段的合法性在 Config::load 内完成。
    let config = Config::load(cli.config.as_deref()).map_err(|err| err.to_string())?;
    install_signal_handlers();

    // 开关：TOML 基线叠加状态文件覆盖；状态文件损坏即致命退出。
    let baseline = Switches {
        translate_enabled: config.bridge.translate_enabled,
        speech_enabled: config.bridge.speech_enabled,
    };
    let state_path = config.daemon.run_dir.join(state::STATE_FILE_NAME);
    let switches = state::load_switches(&state_path, baseline)?;

    let shared = Arc::new(Shared::new(
        switches,
        config.bridge.subtitles_path.clone(),
        state_path,
    ));
    // 容量已由配置校验保证 ≥ 1。
    let queues = Arc::new(Queues::new(
        config.bridge.translate_queue_capacity as usize,
        config.bridge.speech_queue_capacity as usize,
    ));

    let listener = bind_socket(&cli.socket)?;
    listener
        .listen(4)
        .map_err(|err| format!("listen：{err}"))?;
    let writer = subtitles::JsonlWriter::open(&config.bridge.subtitles_path)
        .map_err(|err| format!("打开字幕 {}：{err}", config.bridge.subtitles_path.display()))?;

    let (events_tx, events_rx) = mpsc::channel::<events::Event>();

    eprintln!(
        "mozart-bridge: events={} subtitles={} translate={} speech={}",
        config.asr_events_socket().display(),
        config.bridge.subtitles_path.display(),
        switches.translate_enabled,
        switches.speech_enabled,
    );

    let events_handle = {
        let path = config.asr_events_socket();
        let shared = shared.clone();
        spawn_thread("mozart-bridge-events", move || {
            stop_on_fatal(guard_panic(move || {
                events::run(path, events_tx, shared, &STOP)
            }))
        })
        .map_err(|err| format!("启动事件线程：{err}"))?
    };
    let subtitles_handle = {
        let shared = shared.clone();
        let queues = queues.clone();
        spawn_thread("mozart-bridge-subtitles", move || {
            stop_on_fatal(guard_panic(move || {
                subtitles::run(events_rx, writer, queues, shared, &STOP)
            }))
        })
        .map_err(|err| format!("启动字幕线程：{err}"))?
    };
    let control_handle = {
        let shared = shared.clone();
        let queues = queues.clone();
        spawn_thread("mozart-bridge-control", move || {
            guard_panic(move || {
                control::serve(listener, shared, queues, &STOP);
                Ok(())
            })
        })
        .map_err(|err| format!("启动控制线程：{err}"))?
    };

    let failure = join_threads(vec![
        ("events", events_handle),
        ("subtitles", subtitles_handle),
        ("control", control_handle),
    ]);
    // 收尾：丢弃在途队列（SPEC-05）；字幕存档已逐行 flush，无需额外写盘。
    queues.discard();
    match failure {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mozart-bridge: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_contract_matches_supervisor() {
        let cli =
            Cli::try_parse_from(["mozart-bridge", "--socket", "/run/mozart/bridge.sock"]).unwrap();
        assert_eq!(cli.socket, PathBuf::from("/run/mozart/bridge.sock"));
        assert!(cli.config.is_none());

        let cli = Cli::try_parse_from([
            "mozart-bridge",
            "--socket",
            "bridge.sock",
            "--config",
            "mozart.toml",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("mozart.toml")));

        assert!(Cli::try_parse_from(["mozart-bridge"]).is_err());
        assert!(Cli::try_parse_from(["mozart-bridge", "--socket"]).is_err());
        assert!(Cli::try_parse_from(["mozart-bridge", "--socket", "s", "--bogus"]).is_err());
    }
}