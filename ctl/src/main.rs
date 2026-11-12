use clap::{Parser, Subcommand, ValueEnum};
use mozart_core::config::{ensure_run_dir, Config};
use mozart_core::ctl::schema::{Activity, ProcessState, Response, Route, Status};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const EXIT_ERROR: u8 = 1;
const EXIT_UNREACHABLE: u8 = 3;
const EXIT_REJECTED: u8 = 4;
const EXIT_TIMEOUT: u8 = 5;

#[derive(Parser)]
#[command(name = "mozart-ctl", version, about = "Mozart 控制面客户端")]
struct Cli {
    /// 配置文件；缺省按 MOZART_CONFIG、./mozart.toml 顺序查找
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// 输出稳定 JSON 数据
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 启动 daemon 并等待控制 socket 就绪
    Up,
    /// 停止 daemon 及其子进程
    Down,
    /// 查询整机状态
    Status,
    /// 切换音频路由
    Route {
        #[command(subcommand)]
        action: RouteAction,
    },
    /// 设置静音
    Mute {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// 字幕翻译开关（经 daemon 转发到桥）
    Translate {
        #[arg(value_enum)]
        state: OnOff,
    },
    /// 翻译播报开关与急停（经 daemon 转发到桥）
    Speech {
        #[command(subcommand)]
        action: SpeechAction,
    },
}

#[derive(Subcommand)]
enum RouteAction {
    Set {
        #[arg(value_enum)]
        route: RouteArg,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum RouteArg {
    Direct,
    Rvc,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Subcommand)]
enum SpeechAction {
    /// 开启播报
    On,
    /// 关闭播报（在途自然播完）
    Off,
    /// 关播报并清空待播报队列（在途丢弃）
    Stop,
}

struct CliError {
    code: u8,
    message: String,
}

impl CliError {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mozart-ctl: {}", err.message);
            ExitCode::from(err.code)
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    let config_path = Config::resolve_path(cli.config.as_deref());
    let config = Config::load_from(config_path.as_deref())
        .map_err(|err| CliError::new(EXIT_ERROR, err.to_string()))?;
    match cli.command {
        Action::Up => up(&config, config_path.as_deref(), cli.json),
        Action::Down => down(&config, cli.json),
        Action::Status => status(&config, cli.json),
        Action::Route { action } => {
            let RouteAction::Set { route } = action;
            route_set(&config, route, cli.json)
        }
        Action::Mute { state } => mute(&config, state == "on", cli.json),
        Action::Translate { state } => {
            translate(&config, matches!(state, OnOff::On), cli.json)
        }
        Action::Speech { action } => speech(&config, action, cli.json),
    }
}

fn up(config: &Config, config_path: Option<&Path>, json_output: bool) -> Result<(), CliError> {
    if probe(config).is_ok() {
        print_state(json_output, "already-running", "daemon already running");
        return Ok(());
    }

    ensure_run_dir(&config.daemon.run_dir).map_err(|err| {
        CliError::new(
            EXIT_ERROR,
            format!("prepare {}: {err}", config.daemon.run_dir.display()),
        )
    })?;
    std::fs::create_dir_all(config.log_dir()).map_err(|err| {
        CliError::new(
            EXIT_ERROR,
            format!("create {}: {err}", config.log_dir().display()),
        )
    })?;
    let mut child = spawn_daemon(config, config_path)?;

    let deadline = Instant::now() + Duration::from_millis(config.daemon.start_timeout_ms);
    let mut spawn_exit: Option<String> = None;
    loop {
        if spawn_exit.is_none() {
            if let Some(status) = child
                .try_wait()
                .map_err(|err| CliError::new(EXIT_ERROR, err.to_string()))?
            {
                // 并发 up 时本实例可能抢锁失败退出；另一实例正在就绪，继续等 socket，
                // 保持命令幂等。
                spawn_exit = Some(format!("daemon exited during startup: {status}"));
            }
        }
        match probe(config) {
            Ok(_) => {
                if spawn_exit.is_none() {
                    print_state(json_output, "running", "daemon started");
                } else {
                    print_state(json_output, "already-running", "daemon already running");
                }
                return Ok(());
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(match spawn_exit {
                        Some(message) => CliError::new(EXIT_ERROR, message),
                        None => CliError::new(
                            EXIT_TIMEOUT,
                            format!("daemon did not become ready: {err}"),
                        ),
                    });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn down(config: &Config, json_output: bool) -> Result<(), CliError> {
    let conn = match Seqpacket::connect_timeout(&config.daemon_socket(), rpc_timeout(config)) {
        Ok(conn) => conn,
        Err(err) if is_unreachable(&err) => {
            print_state(json_output, "not-running", "daemon not running");
            return Ok(());
        }
        Err(err) => return Err(CliError::new(EXIT_UNREACHABLE, format!("connect: {err}"))),
    };
    let response = request(
        &conn,
        ctl::DAEMON_SHUTDOWN,
        b"{}",
        config.daemon.rpc_timeout_ms,
    )
    .map_err(classify_io)?;
    expect_ok(response)?;

    let deadline = Instant::now() + Duration::from_millis(config.daemon.start_timeout_ms);
    loop {
        if Seqpacket::connect_timeout(&config.daemon_socket(), rpc_timeout(config)).is_err() {
            print_state(json_output, "stopped", "daemon stopped");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::new(EXIT_TIMEOUT, "daemon did not stop in time"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn status(config: &Config, json_output: bool) -> Result<(), CliError> {
    let conn = connect(config)?;
    let response = request(
        &conn,
        ctl::DAEMON_GET_STATUS,
        b"{}",
        config.daemon.rpc_timeout_ms,
    )
    .map_err(classify_io)?;
    let data = expect_ok(response)?;
    let status: Status = serde_json::from_value(data)
        .map_err(|err| CliError::new(EXIT_ERROR, format!("invalid status payload: {err}")))?;
    if json_output {
        let text = serde_json::to_string_pretty(&status)
            .map_err(|err| CliError::new(EXIT_ERROR, err.to_string()))?;
        println!("{text}");
    } else {
        print_status(&status);
    }
    Ok(())
}

fn route_set(config: &Config, route: RouteArg, json_output: bool) -> Result<(), CliError> {
    let target = match route {
        RouteArg::Direct => Route::Direct,
        RouteArg::Rvc => Route::Rvc,
    };
    let conn = connect(config)?;
    let body = json!({"route": target}).to_string();
    let response = request(
        &conn,
        ctl::DAEMON_ROUTE_SET,
        body.as_bytes(),
        config.daemon.rpc_timeout_ms,
    )
    .map_err(classify_io)?;
    let data = expect_ok(response)?;
    if json_output {
        println!("{data}");
    } else {
        println!("route: {}", route_name(target));
    }
    Ok(())
}

fn mute(config: &Config, mute: bool, json_output: bool) -> Result<(), CliError> {
    let conn = connect(config)?;
    let body = json!({"mute": mute}).to_string();
    let response = request(
        &conn,
        ctl::DAEMON_MUTE_SET,
        body.as_bytes(),
        config.daemon.rpc_timeout_ms,
    )
    .map_err(classify_io)?;
    let data = expect_ok(response)?;
    if json_output {
        println!("{data}");
    } else {
        println!("mute: {}", on_off(mute));
    }
    Ok(())
}

fn translate(config: &Config, enabled: bool, json_output: bool) -> Result<(), CliError> {
    let conn = connect(config)?;
    let body = json!({"enabled": enabled}).to_string();
    let response = request(
        &conn,
        ctl::DAEMON_TRANSLATE_SET,
        body.as_bytes(),
        config.daemon.rpc_timeout_ms,
    )
    .map_err(classify_io)?;
    let data = expect_ok(response)?;
    if json_output {
        println!("{data}");
    } else {
        println!("translate: {}", on_off(enabled));
    }
    Ok(())
}

fn speech(config: &Config, action: SpeechAction, json_output: bool) -> Result<(), CliError> {
    let (type_, body, human) = match action {
        SpeechAction::On => (
            ctl::DAEMON_SPEECH_SET,
            json!({"enabled": true}).to_string(),
            "speech: on",
        ),
        SpeechAction::Off => (
            ctl::DAEMON_SPEECH_SET,
            json!({"enabled": false}).to_string(),
            "speech: off",
        ),
        SpeechAction::Stop => (ctl::DAEMON_SPEECH_STOP, "{}".to_string(), "speech: stopped"),
    };
    let conn = connect(config)?;
    let response = request(
        &conn,
        type_,
        body.as_bytes(),
        config.daemon.rpc_timeout_ms,
    )
    .map_err(classify_io)?;
    let data = expect_ok(response)?;
    if json_output {
        println!("{data}");
    } else {
        println!("{human}");
    }
    Ok(())
}

fn probe(config: &Config) -> io::Result<Response> {
    let conn = Seqpacket::connect_timeout(&config.daemon_socket(), rpc_timeout(config))?;
    request(&conn, ctl::DAEMON_HELLO, b"{}", config.daemon.rpc_timeout_ms)
}

fn spawn_daemon(config: &Config, config_path: Option<&Path>) -> Result<Child, CliError> {
    let bin = daemon_binary();
    let log_path = config.log_dir().join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| CliError::new(EXIT_ERROR, format!("open {}: {err}", log_path.display())))?;
    let stderr = log
        .try_clone()
        .map_err(|err| CliError::new(EXIT_ERROR, err.to_string()))?;

    let mut command = Command::new(&bin);
    if let Some(path) = config_path {
        command.arg("--config").arg(path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // SAFETY: setsid 不访问共享内存；失败时返回错误而不是继续。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map_err(|err| {
        CliError::new(
            EXIT_ERROR,
            format!("spawn {}: {err}", bin.display()),
        )
    })
}

fn daemon_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("MOZART_DAEMON_BIN") {
        return PathBuf::from(path);
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("mozart-daemon")));
    sibling
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("mozart-daemon"))
}

fn rpc_timeout(config: &Config) -> Option<Duration> {
    Some(Duration::from_millis(config.daemon.rpc_timeout_ms))
}

fn connect(config: &Config) -> Result<Seqpacket, CliError> {
    Seqpacket::connect_timeout(&config.daemon_socket(), rpc_timeout(config)).map_err(|err| {
        if is_unreachable(&err) {
            CliError::new(EXIT_UNREACHABLE, format!("daemon not reachable: {err}"))
        } else {
            CliError::new(EXIT_UNREACHABLE, format!("connect: {err}"))
        }
    })
}

fn is_unreachable(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn classify_io(err: io::Error) -> CliError {
    match err.kind() {
        io::ErrorKind::TimedOut => CliError::new(EXIT_TIMEOUT, err.to_string()),
        io::ErrorKind::NotFound
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::UnexpectedEof
        | io::ErrorKind::NotConnected => CliError::new(EXIT_UNREACHABLE, err.to_string()),
        _ => CliError::new(EXIT_ERROR, err.to_string()),
    }
}

fn request(conn: &Seqpacket, type_: u16, body: &[u8], timeout_ms: u64) -> io::Result<Response> {
    let timeout = Duration::from_millis(timeout_ms);
    conn.send_timeout(type_, 1, body, Some(timeout))?;
    let msg = conn.recv(Some(timeout))?;
    serde_json::from_slice(&msg.body).map_err(io::Error::other)
}

fn expect_ok(response: Response) -> Result<Value, CliError> {
    if response.status == "ok" {
        Ok(response.data)
    } else {
        Err(CliError::new(
            EXIT_REJECTED,
            format!("{}: {}", response.code, response.reason),
        ))
    }
}

fn print_state(json_output: bool, state: &str, human: &str) {
    if json_output {
        println!("{}", json!({"state": state}));
    } else {
        println!("{human}");
    }
}

fn print_status(status: &Status) {
    println!(
        "daemon   running pid={} version={} uptime={}s",
        status.daemon.pid, status.daemon.version, status.daemon.uptime_s
    );
    println!(
        "route    {} (desired {}, gpu {})",
        route_name(status.route.active),
        route_name(status.route.desired),
        activity_name(status.arbiter.gpu.owner)
    );
    println!("mute     {}", on_off(status.mute));
    for (name, info) in [
        ("pre", &status.processes.pre),
        ("rvc", &status.processes.rvc),
        ("asr", &status.processes.asr),
        ("tts", &status.processes.tts),
        ("bridge", &status.processes.bridge),
        ("llama", &status.processes.llama),
    ] {
        let pid = info
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string());
        let last_error = info
            .last_error
            .as_deref()
            .map(|err| format!(" last_error={err}"))
            .unwrap_or_default();
        println!(
            "process  {name} {} pid={pid} restarts={}{last_error}",
            process_state_name(info.state),
            info.restarts
        );
    }
    if let Some(pre) = &status.pre {
        println!(
            "pre      route={} mute={} speaker={} frames={}/{}/{} queue={}",
            route_name(pre.route),
            on_off(pre.mute),
            pre.speaker_state,
            pre.frames_captured,
            pre.frames_played,
            pre.frames_dropped,
            pre.queue_depth
        );
    }
    if let Some(rvc) = &status.rvc {
        println!(
            "rvc      state={} kind={} frames={}/{}/{} queue={}",
            rvc.state,
            rvc.stream_kind.as_deref().unwrap_or("-"),
            rvc.frames_in,
            rvc.frames_out,
            rvc.frames_dropped,
            rvc.queue_depth
        );
    }
    if let Some(asr) = &status.asr {
        println!(
            "asr      state={} finals={} frames={} bridge={}",
            asr.state,
            asr.finals,
            asr.frames_received,
            on_off(asr.bridge_connected)
        );
    }
    if let Some(tts) = &status.tts {
        println!(
            "tts      state={} synthesized={} failed={} queue={} model={}",
            tts.state,
            tts.synthesized,
            tts.failed,
            tts.queue_depth,
            on_off(tts.model_loaded)
        );
    }
    if let Some(bridge) = &status.bridge {
        println!(
            "bridge   translate={} speech={} translated={} delivered={} llama={} tts={}",
            on_off(bridge.translate_enabled),
            on_off(bridge.speech_enabled),
            bridge.translated,
            bridge.speech_delivered,
            on_off(bridge.llama_connected),
            on_off(bridge.tts_connected)
        );
    }
    if let Some(hw) = &status.hw {
        let cpu = hw
            .cpu_percent
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| "-".to_string());
        let gpu = hw
            .gpu
            .load_percent
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "hw       cpu={} mem={}/{} MiB gpu={}",
            cpu,
            hw.memory.used_bytes / (1024 * 1024),
            hw.memory.total_bytes / (1024 * 1024),
            gpu
        );
    }
    if status.degraded.is_empty() {
        println!("degraded -");
    } else {
        println!("degraded {}", status.degraded.join("; "));
    }
}

fn route_name(route: Route) -> &'static str {
    match route {
        Route::Direct => "direct",
        Route::Rvc => "rvc",
    }
}

fn activity_name(activity: Option<Activity>) -> &'static str {
    match activity {
        Some(Activity::Realtime) => "realtime",
        Some(Activity::File) => "file",
        None => "-",
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn process_state_name(state: ProcessState) -> &'static str {
    match state {
        ProcessState::Stopped => "stopped",
        ProcessState::Starting => "starting",
        ProcessState::Running => "running",
        ProcessState::Restarting => "restarting",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("mozart-ctl").chain(args.iter().copied()))
    }

    #[test]
    fn translate_parses_on_off() {
        assert!(matches!(
            parse(&["translate", "on"]).unwrap().command,
            Action::Translate { state: OnOff::On }
        ));
        assert!(matches!(
            parse(&["translate", "off"]).unwrap().command,
            Action::Translate { state: OnOff::Off }
        ));
        assert!(parse(&["translate", "bogus"]).is_err());
        assert!(parse(&["translate"]).is_err());
    }

    #[test]
    fn speech_parses_on_off_stop() {
        assert!(matches!(
            parse(&["speech", "on"]).unwrap().command,
            Action::Speech { action: SpeechAction::On }
        ));
        assert!(matches!(
            parse(&["speech", "off"]).unwrap().command,
            Action::Speech { action: SpeechAction::Off }
        ));
        assert!(matches!(
            parse(&["speech", "stop"]).unwrap().command,
            Action::Speech { action: SpeechAction::Stop }
        ));
        assert!(parse(&["speech", "bogus"]).is_err());
        assert!(parse(&["speech"]).is_err());
    }

    #[test]
    fn global_flags_parse_with_language_commands() {
        let cli = parse(&["--json", "translate", "on"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Action::Translate { state: OnOff::On }));
        let cli = parse(&["speech", "stop", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Action::Speech { action: SpeechAction::Stop }));
    }
}
