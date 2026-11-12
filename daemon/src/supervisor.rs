//! 子进程监管：拉起、收集日志、崩溃后退避重启、单服务重启、退出时终止。

use crate::state::{lock_state, now_unix_ms, Role, State};
use mozart_core::config::Config;
use mozart_core::ctl::schema::ExitInfo;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);
const TERM_GRACE: Duration = Duration::from_secs(2);

pub struct Supervisor {
    handle: thread::JoinHandle<()>,
    restart: RestartHandle,
}

/// 单服务重启句柄：控制连接持克隆，向监管线程发送重读出的配置。
#[derive(Clone)]
pub struct RestartHandle {
    role: Role,
    tx: mpsc::Sender<Arc<Config>>,
}

impl RestartHandle {
    pub fn role(&self) -> Role {
        self.role
    }

    /// 请求重启：监管线程停子进程后以新配置重起。
    /// 发送失败仅当监管线程已退出（daemon 关停中）。
    pub fn restart(&self, config: Arc<Config>) -> Result<(), mpsc::SendError<Arc<Config>>> {
        self.tx.send(config)
    }
}

impl Supervisor {
    pub fn spawn(
        role: Role,
        config: Arc<Config>,
        config_path: Option<PathBuf>,
        state: Arc<Mutex<State>>,
        running: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let (tx, restart_rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name(format!("mozart-{}", role.name()))
            .spawn(move || {
                supervise(
                    role,
                    config,
                    config_path.as_deref(),
                    &state,
                    &running,
                    &restart_rx,
                )
            })?;
        Ok(Self {
            handle,
            restart: RestartHandle { role, tx },
        })
    }

    pub fn restart_handle(&self) -> RestartHandle {
        self.restart.clone()
    }

    pub fn join(self) {
        let _ = self.handle.join();
    }
}

fn supervise(
    role: Role,
    mut config: Arc<Config>,
    config_path: Option<&Path>,
    state: &Mutex<State>,
    running: &AtomicBool,
    restart_rx: &mpsc::Receiver<Arc<Config>>,
) {
    let mut backoff = Duration::from_millis(config.daemon.restart_backoff_ms);

    while running.load(Ordering::Acquire) {
        // 重启重读配置（SPEC-04）：spawn 参数每轮跟随当前配置。
        let base = Duration::from_millis(config.daemon.restart_backoff_ms);
        let max = Duration::from_millis(config.daemon.restart_backoff_max_ms);
        let stable = Duration::from_millis(config.daemon.restart_stable_ms);
        let bin = match role {
            Role::Pre => &config.process.pre_bin,
            Role::Rvc => &config.process.rvc_bin,
            Role::Asr => &config.process.asr_bin,
            Role::Tts => &config.process.tts_bin,
            Role::Bridge => &config.process.bridge_bin,
            Role::Llama => &config.process.llama_bin,
        };
        let socket = match role {
            Role::Pre => config.pre_socket(),
            Role::Rvc => config.rvc_socket(),
            Role::Asr => config.asr_socket(),
            Role::Tts => config.tts_socket(),
            Role::Bridge => config.bridge_socket(),
            Role::Llama => config.llama_socket(),
        };
        let log_path = config.log_dir().join(format!("{}.log", role.name()));

        lock_state(state).process_starting(role);
        let mut child = match start_child(role, bin, &socket, &config, config_path, &log_path) {
            Ok(child) => child,
            Err(err) => {
                lock_state(state).process_failed(role, format!("spawn: {err}"));
                match backoff_wait(backoff, running, restart_rx) {
                    Backoff::Stopped => break,
                    Backoff::Restart(new) => {
                        config = new;
                        backoff = Duration::from_millis(config.daemon.restart_backoff_ms);
                    }
                    Backoff::Elapsed => backoff = next_backoff(backoff, max),
                }
                continue;
            }
        };

        let pid = child.id();
        let started = Instant::now();
        lock_state(state).process_running(role, pid);

        let mut restart_now = false;
        let exit = loop {
            if !running.load(Ordering::Acquire) {
                terminate(&mut child, state, role);
                return;
            }
            if let Some(new_config) = take_restart(restart_rx) {
                // 显式重启：停子进程后立即以新配置重起，不计崩溃退避。
                terminate(&mut child, state, role);
                config = new_config;
                restart_now = true;
                break None;
            }
            match child.try_wait() {
                Ok(Some(status)) => break Some(exit_info(&status)),
                Ok(None) => thread::sleep(POLL),
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    lock_state(state).process_failed(role, format!("wait: {err}"));
                    break None;
                }
            }
        };

        if restart_now {
            backoff = Duration::from_millis(config.daemon.restart_backoff_ms);
            continue;
        }
        if let Some(exit) = exit {
            lock_state(state).process_exited(role, exit);
        }
        if started.elapsed() >= stable {
            backoff = base;
        }
        match backoff_wait(backoff, running, restart_rx) {
            Backoff::Stopped => break,
            Backoff::Restart(new) => {
                config = new;
                backoff = Duration::from_millis(config.daemon.restart_backoff_ms);
            }
            Backoff::Elapsed => backoff = next_backoff(backoff, max),
        }
    }
    lock_state(state).process_stopped(role);
}

/// 退避等待的结果：停止监管 / 睡满时长 / 被重启请求打断。
enum Backoff {
    Stopped,
    Elapsed,
    Restart(Arc<Config>),
}

/// 退避等待：每 POLL 检查 running 与重启请求，重启不被退避拖延。
fn backoff_wait(
    mut remaining: Duration,
    running: &AtomicBool,
    restart_rx: &mpsc::Receiver<Arc<Config>>,
) -> Backoff {
    while !remaining.is_zero() {
        if !running.load(Ordering::Acquire) {
            return Backoff::Stopped;
        }
        if let Some(config) = take_restart(restart_rx) {
            return Backoff::Restart(config);
        }
        let step = remaining.min(POLL);
        thread::sleep(step);
        remaining -= step;
    }
    if running.load(Ordering::Acquire) {
        Backoff::Elapsed
    } else {
        Backoff::Stopped
    }
}

/// 取最近一次重启请求：多次请求合并，最后一份配置生效。
fn take_restart(restart_rx: &mpsc::Receiver<Arc<Config>>) -> Option<Arc<Config>> {
    let mut latest = None;
    while let Ok(config) = restart_rx.try_recv() {
        latest = Some(config);
    }
    latest
}

fn start_child(
    role: Role,
    bin: &Path,
    socket: &Path,
    config: &Config,
    config_path: Option<&Path>,
    log_path: &Path,
) -> io::Result<Child> {
    let log = OpenOptions::new().create(true).append(true).open(log_path)?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(resolve_bin(bin));
    if role == Role::Llama {
        // llama-server 不说控制协议（SPEC-05）：无 --socket/--config 契约，
        // --host 直接填 Unix socket 路径；启动参数沿用旧实现。
        // 第三方进程不会自清陈旧 socket 文件，daemon 重启后残留文件会让其
        // bind 失败崩溃循环，故启动前由 daemon 清除（mozart 子进程自行 unlink）。
        let _ = std::fs::remove_file(socket);
        command
            .arg("-m")
            .arg(&config.bridge.llama_model)
            .arg("-c")
            .arg("2048")
            .arg("-b")
            .arg("256")
            .arg("-ub")
            .arg("128")
            .arg("--cache-ram")
            .arg("128")
            .arg("-ngl")
            .arg("99")
            .arg("-np")
            .arg("1")
            .arg("-t")
            .arg("2")
            .arg("-tb")
            .arg("2")
            .arg("--host")
            .arg(socket)
            .arg("--jinja");
    } else {
        command.arg("--socket").arg(socket);
        // SPEC-04：子进程与 daemon 读同一 TOML，各读自己的配置段；daemon 以内置
        // 默认启动时无文件可传，不传 --config，子进程按同一顺序解析得同一默认值。
        if let Some(config_path) = config_path {
            command.arg("--config").arg(config_path);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // 父进程线程消失时让内核终止子进程，避免 daemon 崩溃后留下孤儿。
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            // fork 与 prctl 之间父进程可能已退出（ppid 变为 1）；此时退出，避免孤儿。
            if libc::getppid() == 1 {
                return Err(io::Error::other("parent died before PDEATHSIG"));
            }
            Ok(())
        });
    }
    command.spawn()
}

fn resolve_bin(bin: &Path) -> PathBuf {
    if bin.components().count() > 1 {
        return bin.to_path_buf();
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(bin)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| bin.to_path_buf())
}

fn terminate(child: &mut Child, state: &Mutex<State>, role: Role) {
    // SAFETY: kill 只向已知 pid 发送信号。
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + TERM_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    lock_state(state).process_stopped(role);
}

fn exit_info(status: &std::process::ExitStatus) -> ExitInfo {
    use std::os::unix::process::ExitStatusExt;
    ExitInfo {
        code: status.code(),
        signal: status.signal(),
        at_unix_ms: now_unix_ms(),
    }
}

pub(crate) fn sleep_unless_stopped(mut remaining: Duration, running: &AtomicBool) -> bool {
    while !remaining.is_zero() {
        if !running.load(Ordering::Acquire) {
            return false;
        }
        let step = remaining.min(POLL);
        thread::sleep(step);
        remaining -= step;
    }
    running.load(Ordering::Acquire)
}

fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.saturating_mul(2).min(max)
}
