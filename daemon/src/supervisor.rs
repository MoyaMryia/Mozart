//! 子进程监管：拉起、收集日志、崩溃后退避重启、单服务重启、退出时终止。

use crate::runtime::SharedConfig;
use crate::state::{lock_state, now_unix_ms, Role, State};
use mozart_core::config::{Config, LlmModelConfig};
use mozart_core::ctl::schema::ExitInfo;
use std::ffi::OsString;
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
        config: SharedConfig,
        config_path: Option<PathBuf>,
        state: Arc<Mutex<State>>,
        running: Arc<AtomicBool>,
        services_active: Arc<AtomicBool>,
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
                    &services_active,
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
    shared: SharedConfig,
    config_path: Option<&Path>,
    state: &Mutex<State>,
    running: &AtomicBool,
    services_active: &AtomicBool,
    restart_rx: &mpsc::Receiver<Arc<Config>>,
) {
    // spawn 配置是快照：只在显式重启时重读（重启类键不动运行进程）。
    let mut config = shared.snapshot();
    let mut backoff = Duration::from_millis(config.daemon.restart_backoff_ms);

    while running.load(Ordering::Acquire) {
        // 退避参数是运行时键（SPEC-04）：每轮读最新生效值，即时生效。
        let tuning = shared.snapshot();
        let base = Duration::from_millis(tuning.daemon.restart_backoff_ms);
        let max = Duration::from_millis(tuning.daemon.restart_backoff_max_ms);
        let stable = Duration::from_millis(tuning.daemon.restart_stable_ms);
        let unconfigured =
            (role == Role::Enhance && config.enhance.is_none()) || (role == Role::Web && config.web.is_none());
        if unconfigured || (role.is_business() && !services_active.load(Ordering::Acquire)) {
            lock_state(state).process_stopped(role);
            let wake_on_active = (role.is_business() && !unconfigured).then_some(true);
            match backoff_wait(POLL, running, restart_rx, wake_on_active, services_active) {
                Backoff::Stopped => break,
                Backoff::Restart(new) => {
                    config = new;
                    backoff = base;
                }
                Backoff::ServiceToggle => backoff = base,
                Backoff::Elapsed => {}
            }
            continue;
        }
        let bin = match role {
            Role::Enhance => &config.process.enhance_bin,
            Role::Pre => &config.process.pre_bin,
            Role::Rvc => &config.process.rvc_bin,
            Role::Asr => &config.process.asr_bin,
            Role::Tts => &config.process.tts_bin,
            Role::Bridge => &config.process.bridge_bin,
            Role::Llama => &config.process.llama_bin,
            Role::Web => &config.process.web_bin,
        };
        let socket = match role {
            Role::Enhance => Some(config.enhance_socket()),
            Role::Pre => Some(config.pre_socket()),
            Role::Rvc => Some(config.rvc_socket()),
            Role::Asr => Some(config.asr_socket()),
            Role::Tts => Some(config.tts_socket()),
            Role::Bridge => Some(config.bridge_socket()),
            Role::Llama => Some(config.llama_socket()),
            Role::Web => None,
        };
        let log_path = config.log_dir().join(format!("{}.log", role.name()));

        lock_state(state).process_starting(role);
        let mut child = match start_child(
            role,
            bin,
            socket.as_deref(),
            &config,
            &shared,
            config_path,
            &log_path,
        ) {
            Ok(child) => child,
            Err(err) => {
                lock_state(state).process_failed(role, format!("spawn: {err}"));
                let wake_on_active = role.is_business().then_some(false);
                match backoff_wait(backoff, running, restart_rx, wake_on_active, services_active) {
                    Backoff::Stopped => break,
                    Backoff::Restart(new) => {
                        config = new;
                        backoff = base;
                    }
                    Backoff::ServiceToggle => backoff = base,
                    Backoff::Elapsed => backoff = next_backoff(backoff, max),
                }
                continue;
            }
        };

        let pid = child.id();
        let started = Instant::now();
        lock_state(state).process_running(role, pid);

        let mut restart_now = false;
        let mut paused_now = false;
        let exit = loop {
            if !running.load(Ordering::Acquire) {
                terminate(&mut child, state, role);
                return;
            }
            if role.is_business() && !services_active.load(Ordering::Acquire) {
                terminate(&mut child, state, role);
                paused_now = true;
                break None;
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

        if restart_now || paused_now {
            backoff = base;
            continue;
        }
        if let Some(exit) = exit {
            lock_state(state).process_exited(role, exit);
        }
        if started.elapsed() >= stable {
            backoff = base;
        }
        let wake_on_active = role.is_business().then_some(false);
        match backoff_wait(backoff, running, restart_rx, wake_on_active, services_active) {
            Backoff::Stopped => break,
            Backoff::Restart(new) => {
                config = new;
                backoff = base;
            }
            Backoff::ServiceToggle => backoff = base,
            Backoff::Elapsed => backoff = next_backoff(backoff, max),
        }
    }
    lock_state(state).process_stopped(role);
}

/// 退避等待的结果：停止监管 / 业务组暂停或恢复切换 / 睡满时长 / 被重启请求打断。
enum Backoff {
    Stopped,
    ServiceToggle,
    Elapsed,
    Restart(Arc<Config>),
}

/// 退避等待：每 POLL 检查 running、业务启停信号与重启请求，重启不被退避拖延。
fn backoff_wait(
    mut remaining: Duration,
    running: &AtomicBool,
    restart_rx: &mpsc::Receiver<Arc<Config>>,
    wake_on_active: Option<bool>,
    services_active: &AtomicBool,
) -> Backoff {
    while !remaining.is_zero() {
        if !running.load(Ordering::Acquire) {
            return Backoff::Stopped;
        }
        if wake_on_active.is_some_and(|target| services_active.load(Ordering::Acquire) == target) {
            return Backoff::ServiceToggle;
        }
        if let Some(config) = take_restart(restart_rx) {
            return Backoff::Restart(config);
        }
        let step = remaining.min(POLL);
        thread::sleep(step);
        remaining -= step;
    }
    if !running.load(Ordering::Acquire) {
        Backoff::Stopped
    } else if wake_on_active.is_some_and(|target| services_active.load(Ordering::Acquire) == target) {
        Backoff::ServiceToggle
    } else {
        Backoff::Elapsed
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
    socket: Option<&Path>,
    config: &Config,
    shared: &SharedConfig,
    config_path: Option<&Path>,
    log_path: &Path,
) -> io::Result<Child> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(resolve_bin(bin));
    if role == Role::Llama {
        let socket = socket.expect("llama socket configured");
        // llama-server 不说控制协议（SPEC-05）：无 --socket/--config 契约，
        // --host 直接填 Unix socket 路径；启动参数来自生效的翻译模型配置。
        // 第三方进程不会自清陈旧 socket 文件，daemon 重启后残留文件会让其
        // bind 失败崩溃循环，故启动前由 daemon 清除（mozart 子进程自行 unlink）。
        let _ = std::fs::remove_file(socket);
        command.args(llama_args(config, socket));
    } else if role == Role::Enhance {
        let socket = socket.expect("enhance socket configured");
        let enhance = config
            .enhance
            .as_ref()
            .expect("enhance configured before spawn");
        command
            .arg("--socket")
            .arg(socket)
            .arg("--audio-socket")
            .arg(config.enhance_audio_socket())
            .arg("--model")
            .arg(&enhance.model);
    } else if role == Role::Web {
        if let Some(config_path) = config_path {
            command.arg("--config").arg(config_path);
        }
    } else {
        let socket = socket.expect("service socket configured");
        command.arg("--socket").arg(socket);
        if role == Role::Pre {
            let effective = shared.snapshot();
            command
                .arg("--effective-enhance")
                .arg(serde_json::to_string(&config.enhance).map_err(io::Error::other)?)
                .arg("--effective-dsp")
                .arg(serde_json::to_string(&effective.pre.dsp).map_err(io::Error::other)?)
                .arg("--effective-aec")
                .arg(serde_json::to_string(&effective.pre.aec).map_err(io::Error::other)?)
                .arg("--effective-capture-gain-db")
                .arg(effective.pre.capture_gain_db.to_string())
                .arg("--effective-capture-device")
                .arg(&effective.pre.capture_device)
                .arg("--effective-playback-device")
                .arg(&effective.pre.playback_device);
        }
        // SPEC-04：子进程与 daemon 读同一 TOML，各读自己的配置段；daemon 以内置
        // 默认启动时无文件可传，不传 --config，子进程按同一顺序解析得同一默认值。
        if let Some(config_path) = config_path {
            command.arg("--config").arg(config_path);
        }
    }
    for (key, value) in &config.process.env {
        command.env(key, value);
    }
    if role == Role::Bridge {
        command.env(mozart_core::speech::SUPERVISED_BRIDGE_BOOT_ENV, "1");
    }
    if role == Role::Pre {
        command.env(mozart_core::ctl::schema::SUPERVISED_PLAYOUT_ENV, "1");
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

fn llama_args(config: &Config, socket: &Path) -> Vec<OsString> {
    let model = config
        .active_llm_model()
        .unwrap_or_else(|| LlmModelConfig::legacy_default(PathBuf::new()));
    let threads = model.threads.to_string();
    vec![
        OsString::from("-m"),
        model.path.into_os_string(),
        OsString::from("-c"),
        OsString::from(model.ctx_size.to_string()),
        OsString::from("-b"),
        OsString::from(model.batch_size.to_string()),
        OsString::from("-ub"),
        OsString::from(model.ubatch_size.to_string()),
        OsString::from("--cache-ram"),
        OsString::from(model.cache_ram_mb.to_string()),
        OsString::from("-ngl"),
        OsString::from(model.gpu_layers.to_string()),
        OsString::from("-np"),
        OsString::from("1"),
        OsString::from("-t"),
        OsString::from(&threads),
        OsString::from("-tb"),
        OsString::from(threads),
        OsString::from("--host"),
        socket.as_os_str().to_os_string(),
        OsString::from("--jinja"),
    ]
}

fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.saturating_mul(2).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag_value(args: &[OsString], flag: &str) -> String {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|idx| args.get(idx + 1))
            .map(|val| val.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    #[test]
    fn llama_args_uses_legacy_defaults_when_unconfigured_or_legacy() {
        let socket = Path::new("/run/mozart/llama.sock");
        let empty = Config::default();
        let args = llama_args(&empty, socket);
        assert_eq!(flag_value(&args, "-m"), "");
        assert_eq!(flag_value(&args, "-c"), "2048");
        assert_eq!(flag_value(&args, "-b"), "256");
        assert_eq!(flag_value(&args, "-ub"), "128");
        assert_eq!(flag_value(&args, "--cache-ram"), "128");
        assert_eq!(flag_value(&args, "-ngl"), "99");
        assert_eq!(flag_value(&args, "-np"), "1");
        assert_eq!(flag_value(&args, "-t"), "2");
        assert_eq!(flag_value(&args, "-tb"), "2");
        assert_eq!(flag_value(&args, "--host"), "/run/mozart/llama.sock");
        assert!(args.iter().any(|arg| arg == "--jinja"));

        let mut legacy = Config::default();
        legacy.bridge.llama_model = PathBuf::from("/models/legacy.gguf");
        let args = llama_args(&legacy, socket);
        assert_eq!(flag_value(&args, "-m"), "/models/legacy.gguf");
        assert_eq!(flag_value(&args, "-c"), "2048");
        assert_eq!(flag_value(&args, "--cache-ram"), "128");
    }

    #[test]
    fn llama_args_applies_selected_model_entry() {
        let mut config = Config::default();
        config.bridge.llama_model_id = Some("hymt".to_string());
        config.bridge.models.push(LlmModelConfig {
            id: "hymt".to_string(),
            path: PathBuf::from("/models/hymt.gguf"),
            prompt_profile: mozart_core::config::PromptProfile::HyMt,
            ctx_size: 1024,
            cache_ram_mb: 64,
            batch_size: 512,
            ubatch_size: 256,
            gpu_layers: 48,
            threads: 4,
            ..LlmModelConfig::default()
        });
        let args = llama_args(&config, Path::new("/tmp/llama.sock"));
        assert_eq!(flag_value(&args, "-m"), "/models/hymt.gguf");
        assert_eq!(flag_value(&args, "-c"), "1024");
        assert_eq!(flag_value(&args, "-b"), "512");
        assert_eq!(flag_value(&args, "-ub"), "256");
        assert_eq!(flag_value(&args, "--cache-ram"), "64");
        assert_eq!(flag_value(&args, "-ngl"), "48");
        assert_eq!(flag_value(&args, "-np"), "1");
        assert_eq!(flag_value(&args, "-t"), "4");
        assert_eq!(flag_value(&args, "-tb"), "4");
        assert_eq!(flag_value(&args, "--host"), "/tmp/llama.sock");
        assert!(args.iter().any(|arg| arg == "--jinja"));
    }
}
