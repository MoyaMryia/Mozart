//! 子进程监管：拉起、收集日志、崩溃后退避重启、退出时终止。

use crate::state::{lock_state, now_unix_ms, Role, State};
use mozart_core::config::Config;
use mozart_core::ctl::schema::ExitInfo;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);
const TERM_GRACE: Duration = Duration::from_secs(2);

pub struct Supervisor {
    handle: thread::JoinHandle<()>,
}

impl Supervisor {
    pub fn spawn(
        role: Role,
        config: Arc<Config>,
        config_path: Option<PathBuf>,
        state: Arc<Mutex<State>>,
        running: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let handle = thread::Builder::new()
            .name(format!("mozart-{}", role_name(role)))
            .spawn(move || {
                supervise(role, &config, config_path.as_deref(), &state, &running)
            })?;
        Ok(Self { handle })
    }

    pub fn join(self) {
        let _ = self.handle.join();
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Pre => "pre",
        Role::Rvc => "rvc",
    }
}

fn supervise(
    role: Role,
    config: &Config,
    config_path: Option<&Path>,
    state: &Mutex<State>,
    running: &AtomicBool,
) {
    let bin = match role {
        Role::Pre => &config.process.pre_bin,
        Role::Rvc => &config.process.rvc_bin,
    };
    let socket = match role {
        Role::Pre => config.pre_socket(),
        Role::Rvc => config.rvc_socket(),
    };
    let log_path = config.log_dir().join(format!("{}.log", role_name(role)));
    let base = Duration::from_millis(config.daemon.restart_backoff_ms);
    let max = Duration::from_millis(config.daemon.restart_backoff_max_ms);
    let stable = Duration::from_millis(config.daemon.restart_stable_ms);
    let mut backoff = base;

    while running.load(Ordering::Acquire) {
        lock_state(state).process_starting(role);
        let mut child = match start_child(bin, &socket, config_path, &log_path) {
            Ok(child) => child,
            Err(err) => {
                lock_state(state).process_failed(role, format!("spawn: {err}"));
                if !sleep_unless_stopped(backoff, running) {
                    break;
                }
                backoff = next_backoff(backoff, max);
                continue;
            }
        };

        let pid = child.id();
        let started = Instant::now();
        lock_state(state).process_running(role, pid);

        let exit = loop {
            if !running.load(Ordering::Acquire) {
                terminate(&mut child, state, role);
                return;
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

        if let Some(exit) = exit {
            lock_state(state).process_exited(role, exit);
        }
        if started.elapsed() >= stable {
            backoff = base;
        }
        if !sleep_unless_stopped(backoff, running) {
            break;
        }
        backoff = next_backoff(backoff, max);
    }
    lock_state(state).process_stopped(role);
}

fn start_child(
    bin: &Path,
    socket: &Path,
    config: Option<&Path>,
    log_path: &Path,
) -> io::Result<Child> {
    let log = OpenOptions::new().create(true).append(true).open(log_path)?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(resolve_bin(bin));
    command.arg("--socket").arg(socket);
    // SPEC-04：子进程与 daemon 读同一 TOML，各读自己的配置段；daemon 以内置
    // 默认启动时无文件可传，不传 --config，子进程按同一顺序解析得同一默认值。
    if let Some(config) = config {
        command.arg("--config").arg(config);
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
