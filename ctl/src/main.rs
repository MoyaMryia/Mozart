use clap::{Parser, Subcommand, ValueEnum};
use mozart_core::config::{ensure_run_dir, Config};
use mozart_core::ctl::schema::{
    Activity, DeviceEntry, FileCancelRequest, FileFetchRequest, FileJob, FileJobState, FileProgress,
    FileStatusRequest, FileSubmitRequest, ModelEntry, ProcessState, Response, RestartRequest, Route,
    Status,
};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::fs;
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
    /// 文件转换任务
    File {
        #[command(subcommand)]
        action: FileAction,
    },
    /// 模型清单
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// 重启单个服务（重读配置生效，整栈不动）
    Restart {
        #[arg(value_enum)]
        service: ServiceArg,
    },
    /// 音频设备
    Device {
        #[command(subcommand)]
        action: DeviceAction,
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

#[derive(Subcommand)]
enum FileAction {
    /// 提交本地文件入队（daemon 复制到受管存储）
    Submit {
        /// 源文件路径
        path: PathBuf,
        /// 模型 id；缺省用当前已加载模型
        #[arg(long, value_name = "ID")]
        model: Option<String>,
    },
    /// 暂停队列：不再启动新任务，在跑任务跑完
    Pause,
    /// 恢复队列：继续启动排队任务
    Resume,
    /// 取消任务
    Cancel {
        /// 任务 id
        job_id: String,
    },
    /// 查询任务状态与进度
    Status {
        /// 任务 id
        job_id: String,
    },
    /// 列出队列任务
    List,
    /// 取回结果文件（复制到本地）
    Fetch {
        /// 任务 id
        job_id: String,
        /// 目标路径；缺省当前目录下原文件名主干 + .wav
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// 列出模型（id、是否存在、是否已加载）
    List,
}

#[derive(Clone, Copy, ValueEnum)]
enum ServiceArg {
    Pre,
    Rvc,
    Asr,
    Tts,
    Bridge,
    Llama,
}

#[derive(Subcommand)]
enum DeviceAction {
    /// 列出采集/播放设备（ALSA 名、人读标签、方向）
    List,
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
        Action::File { action } => file_command(&config, action, cli.json),
        Action::Model { action } => match action {
            ModelAction::List => model_list(&config, cli.json),
        },
        Action::Restart { service } => restart(&config, service, cli.json),
        Action::Device { action } => match action {
            DeviceAction::List => device_list(&config, cli.json),
        },
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

fn file_command(config: &Config, action: FileAction, json_output: bool) -> Result<(), CliError> {
    match action {
        FileAction::Submit { path, model } => file_submit(config, path, model, json_output),
        FileAction::Pause => file_pause_resume(config, ctl::DAEMON_FILE_PAUSE, json_output),
        FileAction::Resume => file_pause_resume(config, ctl::DAEMON_FILE_RESUME, json_output),
        FileAction::Cancel { job_id } => file_cancel(config, &job_id, json_output),
        FileAction::Status { job_id } => file_status(config, &job_id, json_output),
        FileAction::List => file_list(config, json_output),
        FileAction::Fetch { job_id, output } => file_fetch(config, &job_id, output, json_output),
    }
}

fn file_submit(
    config: &Config,
    path: PathBuf,
    model: Option<String>,
    json_output: bool,
) -> Result<(), CliError> {
    // 路径按原样交给 daemon：存在性/扩展名/大小由 daemon 入口校验。
    let body = body_json(&FileSubmitRequest {
        path: path.to_string_lossy().into_owned(),
        model_id: model,
    })?;
    let data = call(config, ctl::DAEMON_FILE_SUBMIT, &body)?;
    if json_output {
        println!("{data}");
    } else {
        print_job_ack(&data)?;
    }
    Ok(())
}

fn file_pause_resume(config: &Config, type_: u16, json_output: bool) -> Result<(), CliError> {
    let data = call(config, type_, b"{}")?;
    if json_output {
        println!("{data}");
    } else {
        let paused = bool_field(&data, "paused")?;
        println!("file queue: {}", if paused { "paused" } else { "running" });
    }
    Ok(())
}

fn file_cancel(config: &Config, job_id: &str, json_output: bool) -> Result<(), CliError> {
    let body = body_json(&FileCancelRequest {
        job_id: job_id.to_string(),
    })?;
    let data = call(config, ctl::DAEMON_FILE_CANCEL, &body)?;
    if json_output {
        println!("{data}");
    } else {
        print_job_ack(&data)?;
    }
    Ok(())
}

fn file_status(config: &Config, job_id: &str, json_output: bool) -> Result<(), CliError> {
    let body = body_json(&FileStatusRequest {
        job_id: job_id.to_string(),
    })?;
    let data = call(config, ctl::DAEMON_FILE_STATUS, &body)?;
    if json_output {
        println!("{data}");
    } else {
        let job: FileJob = serde_json::from_value(data)
            .map_err(|err| CliError::new(EXIT_ERROR, format!("invalid file job payload: {err}")))?;
        print_file_job(&job);
    }
    Ok(())
}

fn file_list(config: &Config, json_output: bool) -> Result<(), CliError> {
    let data = call(config, ctl::DAEMON_FILE_LIST, b"{}")?;
    if json_output {
        println!("{data}");
        return Ok(());
    }
    let jobs = data
        .get("jobs")
        .ok_or_else(|| CliError::new(EXIT_ERROR, "invalid file list payload: missing jobs"))?;
    let jobs: Vec<FileJob> = serde_json::from_value(jobs.clone())
        .map_err(|err| CliError::new(EXIT_ERROR, format!("invalid file list payload: {err}")))?;
    let paused = bool_field(&data, "paused")?;
    print_file_table(&jobs, paused);
    Ok(())
}

fn file_fetch(
    config: &Config,
    job_id: &str,
    output: Option<PathBuf>,
    json_output: bool,
) -> Result<(), CliError> {
    let body = body_json(&FileFetchRequest {
        job_id: job_id.to_string(),
        output: output
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
    })?;
    let data = call(config, ctl::DAEMON_FILE_FETCH, &body)?;
    // v1 同机：daemon 回受管结果绝对路径与缺省名，CLI 复制到目标。
    let source = PathBuf::from(text_field(&data, "output_path")?);
    let dest = match output {
        Some(path) => path,
        None => PathBuf::from(text_field(&data, "name")?),
    };
    copy_result(&source, &dest)?;
    if json_output {
        println!(
            "{}",
            json!({"job_id": job_id, "output_path": dest.to_string_lossy()})
        );
    } else {
        println!("fetched: {}", dest.display());
    }
    Ok(())
}

fn model_list(config: &Config, json_output: bool) -> Result<(), CliError> {
    let data = call(config, ctl::DAEMON_MODEL_LIST, b"{}")?;
    if json_output {
        println!("{data}");
        return Ok(());
    }
    let models = data
        .get("models")
        .ok_or_else(|| CliError::new(EXIT_ERROR, "invalid model list payload: missing models"))?;
    let models: Vec<ModelEntry> = serde_json::from_value(models.clone())
        .map_err(|err| CliError::new(EXIT_ERROR, format!("invalid model list payload: {err}")))?;
    if models.is_empty() {
        println!("no models");
        return Ok(());
    }
    println!("{:<24} {:<7} LOADED", "MODEL", "EXISTS");
    for model in &models {
        println!(
            "{:<24} {:<7} {}",
            model.id,
            yes_no(model.exists),
            yes_no(model.loaded)
        );
    }
    Ok(())
}

/// 文件/模型命令共用：连接 → 发命令 → 收应答 → ok 取 data。
fn call(config: &Config, type_: u16, body: &[u8]) -> Result<Value, CliError> {
    let conn = connect(config)?;
    let response = request(&conn, type_, body, config.daemon.rpc_timeout_ms).map_err(classify_io)?;
    expect_ok(response)
}

fn restart(config: &Config, service: ServiceArg, json_output: bool) -> Result<(), CliError> {
    let name = service_name(service);
    let body = body_json(&RestartRequest {
        service: name.to_string(),
    })?;
    let data = call(config, ctl::DAEMON_RESTART, &body)?;
    if json_output {
        println!("{data}");
    } else {
        println!("{name}: {}", text_field(&data, "state")?);
    }
    Ok(())
}

fn service_name(service: ServiceArg) -> &'static str {
    match service {
        ServiceArg::Pre => "pre",
        ServiceArg::Rvc => "rvc",
        ServiceArg::Asr => "asr",
        ServiceArg::Tts => "tts",
        ServiceArg::Bridge => "bridge",
        ServiceArg::Llama => "llama",
    }
}

fn device_list(config: &Config, json_output: bool) -> Result<(), CliError> {
    let data = call(config, ctl::DAEMON_DEVICE_LIST, b"{}")?;
    if json_output {
        println!("{data}");
        return Ok(());
    }
    let devices = data
        .get("devices")
        .ok_or_else(|| CliError::new(EXIT_ERROR, "invalid device list payload: missing devices"))?;
    let devices: Vec<DeviceEntry> = serde_json::from_value(devices.clone())
        .map_err(|err| CliError::new(EXIT_ERROR, format!("invalid device list payload: {err}")))?;
    if devices.is_empty() {
        println!("no devices");
        return Ok(());
    }
    println!("{:<24} {:<9} LABEL", "DEVICE", "KIND");
    for device in &devices {
        println!("{:<24} {:<9} {}", device.id, device.kind, device.label);
    }
    Ok(())
}

fn body_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, CliError> {
    serde_json::to_vec(value).map_err(|err| CliError::new(EXIT_ERROR, err.to_string()))
}

fn text_field<'a>(data: &'a Value, key: &str) -> Result<&'a str, CliError> {
    data.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::new(EXIT_ERROR, format!("invalid payload: missing {key}")))
}

fn bool_field(data: &Value, key: &str) -> Result<bool, CliError> {
    data.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| CliError::new(EXIT_ERROR, format!("invalid payload: missing {key}")))
}

/// submit/cancel 应答的人读行：job_id 与 state。
fn print_job_ack(data: &Value) -> Result<(), CliError> {
    let job_id = text_field(data, "job_id")?;
    let state = text_field(data, "state")?;
    println!("job: {job_id} ({state})");
    Ok(())
}

fn print_file_job(job: &FileJob) {
    println!(
        "job      {} name={} model={}",
        job.job_id, job.name, job.model_id
    );
    println!("state    {}", file_state_name(job.state));
    println!("progress {}", progress_text(&job.progress));
    println!("result   {}", if job.result_ready { "ready" } else { "-" });
    if let Some(error) = &job.error {
        println!("error    {error}");
    }
}

fn print_file_table(jobs: &[FileJob], paused: bool) {
    if paused {
        println!("queue paused");
    }
    if jobs.is_empty() {
        println!("no jobs");
        return;
    }
    println!(
        "{:<22} {:<20} {:<12} {:<10} PROGRESS",
        "JOB", "NAME", "MODEL", "STATE"
    );
    for job in jobs {
        println!(
            "{:<22} {:<20} {:<12} {:<10} {}",
            job.job_id,
            job.name,
            job.model_id,
            file_state_name(job.state),
            progress_text(&job.progress)
        );
    }
}

fn progress_text(progress: &FileProgress) -> String {
    let blocks = match (progress.blocks_done, progress.blocks_total) {
        (Some(done), Some(total)) => format!(" blocks={done}/{total}"),
        _ => String::new(),
    };
    format!("{} {}%{blocks}", progress.stage, progress.percent)
}

fn file_state_name(state: FileJobState) -> &'static str {
    match state {
        FileJobState::Queued => "queued",
        FileJobState::Processing => "processing",
        FileJobState::Cancelling => "cancelling",
        FileJobState::Completed => "completed",
        FileJobState::Failed => "failed",
        FileJobState::Cancelled => "cancelled",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// 复制结果到目标。目标已存在时不覆盖：同一文件（含硬链接/符号链接）视为
/// 已取回，幂等成功；不同文件报错退出，不静默清掉用户数据。
fn copy_result(source: &Path, dest: &Path) -> Result<(), CliError> {
    if dest.exists() {
        let same = same_file(source, dest).map_err(|err| {
            CliError::new(EXIT_ERROR, format!("fetch {}: {err}", source.display()))
        })?;
        if same {
            return Ok(());
        }
        return Err(CliError::new(
            EXIT_ERROR,
            format!("destination exists: {}", dest.display()),
        ));
    }
    fs::copy(source, dest).map_err(|err| {
        CliError::new(
            EXIT_ERROR,
            format!("copy {} -> {}: {err}", source.display(), dest.display()),
        )
    })?;
    Ok(())
}

/// 同一文件判定：设备号 + inode（metadata 跟随符号链接，硬链接也算同一文件）。
fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let a = fs::metadata(a)?;
    let b = fs::metadata(b)?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
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

    #[test]
    fn file_submit_parses_path_and_optional_model() {
        let cli = parse(&["file", "submit", "/tmp/a.wav"]).unwrap();
        match cli.command {
            Action::File {
                action: FileAction::Submit { path, model },
            } => {
                assert_eq!(path, PathBuf::from("/tmp/a.wav"));
                assert_eq!(model, None);
            }
            _ => panic!("expected file submit"),
        }
        let cli = parse(&["file", "submit", "/tmp/a.wav", "--model", "demo"]).unwrap();
        match cli.command {
            Action::File {
                action: FileAction::Submit { model, .. },
            } => assert_eq!(model.as_deref(), Some("demo")),
            _ => panic!("expected file submit"),
        }
        assert!(parse(&["file", "submit"]).is_err());
        assert!(parse(&["file", "submit", "/tmp/a.wav", "--model"]).is_err());
    }

    #[test]
    fn file_queue_and_job_commands_parse() {
        assert!(matches!(
            parse(&["file", "pause"]).unwrap().command,
            Action::File { action: FileAction::Pause }
        ));
        assert!(matches!(
            parse(&["file", "resume"]).unwrap().command,
            Action::File { action: FileAction::Resume }
        ));
        assert!(matches!(
            parse(&["file", "list"]).unwrap().command,
            Action::File { action: FileAction::List }
        ));
        match parse(&["file", "cancel", "job_1"]).unwrap().command {
            Action::File {
                action: FileAction::Cancel { job_id },
            } => assert_eq!(job_id, "job_1"),
            _ => panic!("expected file cancel"),
        }
        match parse(&["file", "status", "job_1"]).unwrap().command {
            Action::File {
                action: FileAction::Status { job_id },
            } => assert_eq!(job_id, "job_1"),
            _ => panic!("expected file status"),
        }
        assert!(parse(&["file", "cancel"]).is_err());
        assert!(parse(&["file", "status"]).is_err());
        assert!(parse(&["file", "bogus"]).is_err());
        assert!(parse(&["file"]).is_err());
    }

    #[test]
    fn file_fetch_parses_output_short_and_long() {
        for flag in ["-o", "--output"] {
            let cli = parse(&["file", "fetch", "job_1", flag, "/tmp/out.wav"]).unwrap();
            match cli.command {
                Action::File {
                    action: FileAction::Fetch { job_id, output },
                } => {
                    assert_eq!(job_id, "job_1");
                    assert_eq!(output, Some(PathBuf::from("/tmp/out.wav")));
                }
                _ => panic!("expected file fetch"),
            }
        }
        let cli = parse(&["file", "fetch", "job_1"]).unwrap();
        assert!(matches!(
            cli.command,
            Action::File { action: FileAction::Fetch { output: None, .. } }
        ));
        assert!(parse(&["file", "fetch"]).is_err());
        assert!(parse(&["file", "fetch", "job_1", "-o"]).is_err());
    }

    #[test]
    fn model_list_parses_and_global_flags_apply() {
        let cli = parse(&["model", "list", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(
            cli.command,
            Action::Model { action: ModelAction::List }
        ));
        let cli = parse(&["--json", "file", "list"]).unwrap();
        assert!(cli.json);
        assert!(parse(&["model"]).is_err());
        assert!(parse(&["model", "bogus"]).is_err());
    }

    #[test]
    fn restart_parses_all_service_names() {
        assert!(matches!(
            parse(&["restart", "pre"]).unwrap().command,
            Action::Restart { service: ServiceArg::Pre }
        ));
        assert!(matches!(
            parse(&["restart", "rvc"]).unwrap().command,
            Action::Restart { service: ServiceArg::Rvc }
        ));
        assert!(matches!(
            parse(&["restart", "asr"]).unwrap().command,
            Action::Restart { service: ServiceArg::Asr }
        ));
        assert!(matches!(
            parse(&["restart", "tts"]).unwrap().command,
            Action::Restart { service: ServiceArg::Tts }
        ));
        assert!(matches!(
            parse(&["restart", "bridge"]).unwrap().command,
            Action::Restart { service: ServiceArg::Bridge }
        ));
        assert!(matches!(
            parse(&["restart", "llama"]).unwrap().command,
            Action::Restart { service: ServiceArg::Llama }
        ));
        assert!(parse(&["restart", "bogus"]).is_err());
        assert!(parse(&["restart"]).is_err());
        let cli = parse(&["restart", "rvc", "--json"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn device_list_parses_and_global_flags_apply() {
        let cli = parse(&["device", "list", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(
            cli.command,
            Action::Device { action: DeviceAction::List }
        ));
        assert!(parse(&["device"]).is_err());
        assert!(parse(&["device", "bogus"]).is_err());
    }
}
