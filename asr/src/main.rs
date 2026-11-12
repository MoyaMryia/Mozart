//! mozart-asr：识别服务进程（SPEC-05）。
//! 线程结构：收包（输入 DGRAM → SPSC 环）→ 解码（流式识别 + endpoint 切句 + 整句移交复核）
//! → 复核（独立队列容量 2，SenseVoice 离线）；输出线程独占事件 SEQPACKET（桥连接），
//! 控制线程独立服务 asr.sock。计数全部原子，控制只读。
//! 停止：SIGTERM/SIGINT 置停止标志；解码喂 0.3 s 静音切出最后 final，复核限时 1 s 排空、
//! 剩余标 skipped。句音频缓冲超 700 帧即致命退出，由 daemon 重启。

mod control;
mod decoder;
mod event;
mod ffi;
mod output;
mod receiver;
mod review;
mod state;
#[cfg(test)]
mod tests;

use clap::Parser;
use mozart_core::config::Config;
use mozart_core::dgram::FrameSocket;
use mozart_core::ring::Ring;
use mozart_core::thread::{bind_socket, guard_panic, spawn_thread};
use mozart_core::InputFrame;
use receiver::Inbox;
use state::Shared;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

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
#[command(name = "mozart-asr", version, about = "Mozart 识别服务")]
struct Cli {
    /// 控制面 socket 路径（daemon 传入）
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    /// 配置文件；缺省按 MOZART_CONFIG、./mozart.toml 顺序查找
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

/// 生成 8 位十六进制启动号：优先 /dev/urandom，失败兜底时间+pid。
fn new_startup_id() -> String {
    let mut bytes = [0u8; 4];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        return bytes.iter().map(|b| format!("{b:02x}")).collect();
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mixed = nanos ^ ((std::process::id() as u64) << 32);
    format!("{:08x}", mixed as u32)
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
    // 入口校验一次：[asr] 配置合法性在 Config::load 内完成。
    let config = Config::load(cli.config.as_deref()).map_err(|err| err.to_string())?;
    install_signal_handlers();

    let shared = Arc::new(Shared::new(new_startup_id()));
    // 容量已由配置校验保证 ≥ 1。
    let ring = Arc::new(Ring::<Inbox>::new(config.asr.ring_capacity as usize));

    let listener = bind_socket(&cli.socket)?;
    listener
        .listen(4)
        .map_err(|err| format!("listen：{err}"))?;
    let input = FrameSocket::<InputFrame>::bind(
        &config.asr_input_socket(),
        config.asr.ring_capacity as usize,
    )
    .map_err(|err| format!("绑定 {}：{err}", config.asr_input_socket().display()))?;
    let events_listener = output::EventListener::bind(&config.asr_events_socket())
        .map_err(|err| format!("绑定 {}：{err}", config.asr_events_socket().display()))?;

    let (events_tx, events_rx) = mpsc::sync_channel::<event::Event>(event::EVENT_QUEUE_CAPACITY);
    let review_enabled = !config.asr.review_model_dir.as_os_str().is_empty();
    let (review_tx, review_rx) = if review_enabled {
        let (tx, rx) = mpsc::sync_channel::<review::ReviewJob>(review::QUEUE_CAPACITY);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    eprintln!(
        "mozart-asr: startup_id={} input={} events={} review={}",
        shared.startup_id,
        config.asr_input_socket().display(),
        config.asr_events_socket().display(),
        review_enabled,
    );

    let receiver_handle = {
        let ring = ring.clone();
        let shared = shared.clone();
        spawn_thread("mozart-asr-recv", move || {
            stop_on_fatal(guard_panic(move || {
                receiver::run(input, ring, shared, &STOP)
            }))
        })
        .map_err(|err| format!("启动收包线程：{err}"))?
    };
    let decoder_handle = {
        let ring = ring.clone();
        let shared = shared.clone();
        let events = events_tx.clone();
        let model_dir = config.asr.model_dir.clone();
        let threads = config.asr.threads;
        spawn_thread("mozart-asr-decode", move || {
            stop_on_fatal(guard_panic(move || {
                decoder::run(ring, model_dir, threads, review_tx, events, shared, &STOP)
            }))
        })
        .map_err(|err| format!("启动解码线程：{err}"))?
    };
    let review_handle = match review_rx {
        Some(rx) => {
            let shared = shared.clone();
            let events = events_tx.clone();
            let model_dir = config.asr.review_model_dir.clone();
            let threads = config.asr.threads;
            Some(
                spawn_thread("mozart-asr-review", move || {
                    guard_panic(move || {
                        review::run(rx, model_dir, threads, events, shared, &STOP);
                        Ok(())
                    })
                })
                .map_err(|err| format!("启动复核线程：{err}"))?,
            )
        }
        None => None,
    };
    let output_handle = {
        let shared = shared.clone();
        let capacity = config.asr.output_queue_capacity as usize;
        spawn_thread("mozart-asr-output", move || {
            guard_panic(move || {
                output::run(events_listener, events_rx, shared, capacity);
                Ok(())
            })
        })
        .map_err(|err| format!("启动输出线程：{err}"))?
    };
    let control_handle = {
        let shared = shared.clone();
        let ring = ring.clone();
        spawn_thread("mozart-asr-control", move || {
            guard_panic(move || {
                control::serve(listener, shared, ring, &STOP);
                Ok(())
            })
        })
        .map_err(|err| format!("启动控制线程：{err}"))?
    };
    // events_tx 的原始持有者在此释放：解码/复核退出后输出线程收到 Disconnected。
    drop(events_tx);

    let mut handles = vec![
        ("receiver", receiver_handle),
        ("decoder", decoder_handle),
    ];
    if let Some(handle) = review_handle {
        handles.push(("review", handle));
    }
    handles.push(("output", output_handle));
    handles.push(("control", control_handle));
    match join_threads(handles) {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mozart-asr: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn cli_contract_matches_supervisor() {
        let cli = Cli::try_parse_from(["mozart-asr", "--socket", "/run/mozart/asr.sock"]).unwrap();
        assert_eq!(cli.socket, PathBuf::from("/run/mozart/asr.sock"));
        assert!(cli.config.is_none());

        let cli = Cli::try_parse_from([
            "mozart-asr",
            "--socket",
            "asr.sock",
            "--config",
            "mozart.toml",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("mozart.toml")));

        assert!(Cli::try_parse_from(["mozart-asr"]).is_err());
        assert!(Cli::try_parse_from(["mozart-asr", "--socket"]).is_err());
        assert!(Cli::try_parse_from(["mozart-asr", "--socket", "s", "--bogus"]).is_err());
    }

    #[test]
    fn startup_id_is_eight_hex_chars() {
        let id = new_startup_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        // 两次启动号不同（随机源）。
        assert_ne!(id, new_startup_id());
    }
}