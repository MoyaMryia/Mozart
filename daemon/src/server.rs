use crate::device;
use crate::file::{worker::FileWorker, FileCoordinator};
use crate::poller::Poller;
use crate::runtime::SharedConfig;
use crate::services::{self, Services};
use crate::state::{lock_state, Role, State};
use crate::state_file::{self, StateFile};
use crate::supervisor::{RestartHandle, Supervisor};
use mozart_core::config::{
    device_key_kind, ensure_run_dir, get_key, runtime_key_owner, save_overrides, set_key,
    state_file_path, Config,
};
use mozart_core::ctl::schema::{
    self, ConfigGetRequest, ConfigSetRequest, DeviceListRequest, FileCancelRequest, FileFetchRequest,
    FileStatusRequest, FileSubmitRequest, MuteSetRequest, RestartRequest, RouteSetRequest,
    RvcParams, SwitchSetRequest,
};
use mozart_core::ctl::{self, Message, Seqpacket};
use serde_json::json;
use signal_hook::iterator::Signals;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 控制面共享上下文：每条控制连接持一份克隆。
#[derive(Clone)]
struct Ctx {
    state: Arc<Mutex<State>>,
    services: Arc<Services>,
    file: Arc<FileCoordinator>,
    config: SharedConfig,
    state_file: Arc<Mutex<StateFile>>,
    config_path: Option<PathBuf>,
    restarts: Arc<Vec<RestartHandle>>,
    /// TOML 写回串行锁：config set / config save 的读改写不得交错。
    toml_lock: Arc<Mutex<()>>,
    running: Arc<AtomicBool>,
}

pub fn serve(config: &Config, config_path: Option<&Path>) -> io::Result<()> {
    ensure_run_dir(&config.daemon.run_dir)?;
    std::fs::create_dir_all(config.log_dir())?;
    let _lock = lock_instance(config)?;

    let socket_path = config.daemon_socket();
    let _ = std::fs::remove_file(&socket_path);
    let listener = Seqpacket::bind(&socket_path)?;
    listener.listen(8)?;
    eprintln!("mozart-daemon: listening on {}", socket_path.display());

    // 生效配置 = TOML 基线 + 运行时状态覆盖（SPEC-04）。状态文件是存档快照，
    // 启动叠加恢复一次；字幕开关覆盖待桥就绪后下发（bridge_restore）。
    let state_file = StateFile::load(state_file_path())?;
    let mut effective = config.clone();
    state_file::overlay(&mut effective, state_file.overrides());
    let bridge_restore = Arc::new(Mutex::new(state_file.bridge_restore()));
    let state_file = Arc::new(Mutex::new(state_file));
    let shared = SharedConfig::new(effective);

    // 子进程与 daemon 读同一 TOML：解析出的路径随 spawn 传递（--config）。
    let config_path = config_path.map(|path| path.to_path_buf());
    let state = Arc::new(Mutex::new(State::new()));
    let services = Arc::new(Services::new(&shared));
    let running = Arc::new(AtomicBool::new(true));
    // SIGTERM/SIGINT 与 Shutdown 同一优雅关停（SPEC-04），幂等。
    // signal-hook 经自管道递送：handler 只写管道（异步信号安全），状态改动
    // 在下面的迭代线程做。线程与 handler 常驻到进程退出：重复信号只是再次
    // 置标志；若中途注销 handler，第二发信号会按默认动作直接杀死进程。
    let mut signals = Signals::new([libc::SIGTERM, libc::SIGINT])
        .map_err(|err| io::Error::other(format!("signal handler: {err}")))?;
    let signal_state = state.clone();
    let signal_running = running.clone();
    thread::spawn(move || {
        for signal in signals.forever() {
            eprintln!("mozart-daemon: received signal {signal}, shutting down");
            lock_state(&signal_state).begin_shutdown();
            signal_running.store(false, Ordering::Release);
        }
    });
    let supervisors = [
        Role::Pre,
        Role::Rvc,
        Role::Asr,
        Role::Tts,
        Role::Bridge,
        Role::Llama,
    ]
    .into_iter()
    .map(|role| {
        Supervisor::spawn(
            role,
            shared.clone(),
            config_path.clone(),
            state.clone(),
            running.clone(),
        )
    })
    .collect::<io::Result<Vec<_>>>()?;
    let restarts = Arc::new(
        supervisors
            .iter()
            .map(Supervisor::restart_handle)
            .collect::<Vec<_>>(),
    );
    // 文件数据面：解码/喂帧/收帧/编码在独立 worker 线程跑，不占控制面。
    let file = Arc::new(FileCoordinator::new(
        shared.clone(),
        state.clone(),
        services.clone(),
        running.clone(),
        Arc::new(FileWorker),
    )?);
    let poller = Poller::spawn(
        shared.clone(),
        state.clone(),
        services.clone(),
        file.clone(),
        bridge_restore,
        running.clone(),
    )?;

    let ctx = Ctx {
        state,
        services,
        file,
        config: shared,
        state_file,
        config_path,
        restarts,
        toml_lock: Arc::new(Mutex::new(())),
        running,
    };
    let outcome = accept_loop(&listener, &ctx);
    ctx.running.store(false, Ordering::Release);
    for supervisor in supervisors {
        supervisor.join();
    }
    poller.join();
    ctx.file.shutdown();
    drop(listener);
    let _ = std::fs::remove_file(&socket_path);
    outcome
}

fn lock_instance(config: &Config) -> io::Result<File> {
    let path = config.daemon.run_dir.join("daemon.lock");
    let file = OpenOptions::new().create(true).write(true).open(&path)?;
    file.try_lock().map_err(|err| match err {
        std::fs::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::AddrInUse, "daemon already running")
        }
        std::fs::TryLockError::Error(source) => source,
    })?;
    Ok(file)
}

fn lock_state_file(state_file: &Mutex<StateFile>) -> MutexGuard<'_, StateFile> {
    state_file
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn accept_loop(listener: &Seqpacket, ctx: &Ctx) -> io::Result<()> {
    while ctx.running.load(Ordering::Acquire) {
        match listener.wait_readable(Some(Duration::from_millis(100))) {
            Ok(()) => match listener.accept() {
                Ok(conn) => {
                    let ctx = ctx.clone();
                    thread::spawn(move || {
                        if let Err(err) = serve_connection(&conn, &ctx) {
                            eprintln!("mozart-daemon: control connection: {err}");
                        }
                    });
                }
                Err(err) => eprintln!("mozart-daemon: accept: {err}"),
            },
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn serve_connection(conn: &Seqpacket, ctx: &Ctx) -> io::Result<()> {
    loop {
        let msg = match conn.recv(Some(IDLE_TIMEOUT)) {
            Ok(msg) => msg,
            Err(err)
                if err.kind() == io::ErrorKind::TimedOut
                    || err.kind() == io::ErrorKind::UnexpectedEof =>
            {
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        let (reply, shutdown) = handle_request(ctx, &msg);
        let type_ = msg.header.type_;
        let request_id = msg.header.request_id;
        let rpc_timeout = ctx.config.snapshot().daemon.rpc_timeout_ms;
        conn.send_timeout(
            type_,
            request_id,
            &reply,
            Some(Duration::from_millis(rpc_timeout)),
        )?;
        if shutdown {
            ctx.running.store(false, Ordering::Release);
            return Ok(());
        }
    }
}

fn handle_request(ctx: &Ctx, msg: &Message) -> (Vec<u8>, bool) {
    let type_ = msg.header.type_;
    if type_ == ctl::DAEMON_SHUTDOWN {
        lock_state(&ctx.state).begin_shutdown();
        return (schema::ok_body(json!({})), true);
    }
    if lock_state(&ctx.state).is_shutting_down() {
        return (
            schema::error_body(ctl::CODE_BUSY, "daemon is shutting down"),
            false,
        );
    }
    match type_ {
        ctl::DAEMON_HELLO => (
            schema::ok_body(json!({"version": env!("CARGO_PKG_VERSION")})),
            false,
        ),
        ctl::DAEMON_GET_STATUS => {
            let mut status = lock_state(&ctx.state).status();
            // 当前生效配置里的会话参数：config set 失败回退后仍显示旧值。
            let rvc = ctx.config.snapshot().rvc.clone();
            status.rvc_params = Some(RvcParams {
                pitch_shift: rvc.pitch_shift,
                index_rate: rvc.index_rate,
                rms_mix_rate: rvc.rms_mix_rate,
                protect: rvc.protect,
                f0_method: rvc.f0_method,
            });
            match serde_json::to_value(status) {
                Ok(status) => (schema::ok_body(status), false),
                Err(err) => (schema::error_body(ctl::CODE_INTERNAL, &err.to_string()), false),
            }
        }
        ctl::DAEMON_ROUTE_SET => match parse_body::<RouteSetRequest>(&msg.body) {
            Ok(request) => {
                let config = ctx.config.snapshot();
                match services::route_set(&ctx.state, &ctx.services, &config, request.route) {
                    Ok(route) => (schema::ok_body(json!({"route": route})), false),
                    Err((code, reason)) => (schema::error_body(&code, &reason), false),
                }
            }
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_MUTE_SET => match parse_body::<MuteSetRequest>(&msg.body) {
            Ok(request) => match services::mute_set(&ctx.state, &ctx.services, request.mute) {
                Ok(mute) => (schema::ok_body(json!({"mute": mute})), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_TRANSLATE_SET => match parse_body::<SwitchSetRequest>(&msg.body) {
            Ok(request) => match services::translate_set(&ctx.services, request.enabled) {
                Ok(data) => {
                    record_switch(ctx, "bridge.translate_enabled", request.enabled);
                    (schema::ok_body(data), false)
                }
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_SPEECH_SET => match parse_body::<SwitchSetRequest>(&msg.body) {
            Ok(request) => match services::speech_set(&ctx.services, request.enabled) {
                Ok(data) => {
                    record_switch(ctx, "bridge.speech_enabled", request.enabled);
                    (schema::ok_body(data), false)
                }
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_SPEECH_STOP => match services::speech_stop(&ctx.state, &ctx.services) {
            Ok(data) => {
                // 桥已关播报（speech_enabled=false）：同步记入状态覆盖。
                record_switch(ctx, "bridge.speech_enabled", false);
                (schema::ok_body(data), false)
            }
            Err((code, reason)) => (schema::error_body(&code, &reason), false),
        },
        ctl::DAEMON_FILE_SUBMIT => match parse_body::<FileSubmitRequest>(&msg.body) {
            Ok(request) => file_reply(ctx.file.submit(request)),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_FILE_PAUSE => file_reply(ctx.file.pause()),
        ctl::DAEMON_FILE_RESUME => file_reply(ctx.file.resume()),
        ctl::DAEMON_FILE_CANCEL => match parse_body::<FileCancelRequest>(&msg.body) {
            Ok(request) => file_reply(ctx.file.cancel(&request.job_id)),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_FILE_FETCH => match parse_body::<FileFetchRequest>(&msg.body) {
            Ok(request) => file_reply(ctx.file.fetch(request)),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_FILE_STATUS => match parse_body::<FileStatusRequest>(&msg.body) {
            Ok(request) => file_reply(ctx.file.status(&request.job_id)),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_FILE_LIST => file_reply(ctx.file.list()),
        ctl::DAEMON_MODEL_LIST => file_reply(
            services::list_models(&ctx.services).map(|models| json!({"models": models})),
        ),
        ctl::DAEMON_RESTART => match parse_body::<RestartRequest>(&msg.body) {
            Ok(request) => (restart_service(ctx, &request.service), false),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_DEVICE_LIST => match parse_body::<DeviceListRequest>(&msg.body) {
            Ok(request) => device::list_reply(request.all, device::list),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_CONFIG_SET => match parse_body::<ConfigSetRequest>(&msg.body) {
            Ok(request) => (config_set_reply(ctx, &request), false),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_CONFIG_GET => match parse_body::<ConfigGetRequest>(&msg.body) {
            Ok(request) => match get_key(ctx.config_path.as_deref(), request.key.as_deref()) {
                Ok(value) => (schema::ok_body(json!({"value": value})), false),
                Err(err) => (schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()), false),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_CONFIG_SAVE => (config_save_reply(ctx), false),
        _ => (
            schema::error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
            false,
        ),
    }
}

/// 字幕开关转发成功后记入运行时状态文件（SPEC-04）。落盘失败只记降级：
/// 开关已在桥生效（活权威），快照滞后原因 status 可查，不阻断命令。
fn record_switch(ctx: &Ctx, key: &str, enabled: bool) {
    if let Err(err) = lock_state_file(&ctx.state_file).set_key(key, json!(enabled)) {
        lock_state(&ctx.state).push_degraded(format!("state file write {key}: {err}"));
    }
}

/// 单服务重启（SPEC-04 重启两粒度）：入口重读 TOML，失败拒绝、不动运行进程；
/// 成功则交监管线程停子进程后以新配置重起，整栈不受影响。重启受理即清待重启标记。
fn restart_service(ctx: &Ctx, service: &str) -> Vec<u8> {
    let Some(role) = Role::from_name(service) else {
        return schema::error_body(
            ctl::CODE_INVALID_ARGUMENT,
            &format!("unknown service: {service}"),
        );
    };
    let config = match Config::load_from(ctx.config_path.as_deref()) {
        Ok(config) => config,
        Err(err) => return schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
    };
    let handle = ctx
        .restarts
        .iter()
        .find(|handle| handle.role() == role)
        .expect("serve 为每个 Role 建了重启句柄");
    match handle.restart(Arc::new(config)) {
        Ok(()) => {
            lock_state(&ctx.state).clear_pending_restart(service);
            schema::ok_body(json!({"service": service, "state": "restarting"}))
        }
        // 发送失败仅当监管线程已退出：daemon 正在关停。
        Err(_) => schema::error_body(ctl::CODE_BUSY, "daemon is shutting down"),
    }
}

/// 请求体解析：失败即 InvalidArgument 应答。
fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Vec<u8>> {
    serde_json::from_slice(body)
        .map_err(|err| schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()))
}

fn file_reply(result: Result<serde_json::Value, (String, String)>) -> (Vec<u8>, bool) {
    match result {
        Ok(data) => (schema::ok_body(data), false),
        Err((code, reason)) => (schema::error_body(&code, &reason), false),
    }
}

/// config set（SPEC-04）：设备键先对照当前清单校验存在（防呆），再原子写基线，
/// 回重启提示契约 restart_required + affected，并登记待重启。daemon 运行时键与
/// rvc 会话参数键写基线后即时应用：daemon/file 键清覆盖并重建生效配置；rvc 会话
/// 参数按参数决定是否 StopStream→StartStream，失败保留旧生效配置。其余键由
/// core::set_key 分类拒绝。
fn config_set_reply(ctx: &Ctx, request: &ConfigSetRequest) -> Vec<u8> {
    if let Some(kind) = device_key_kind(&request.key) {
        match device::exists(&request.value, kind) {
            Ok(true) => {}
            Ok(false) => {
                return schema::error_body(
                    ctl::CODE_INVALID_ARGUMENT,
                    &format!(
                        "设备不存在：{}（{kind}；用 device list 查看可用设备）",
                        request.value
                    ),
                );
            }
            Err((code, reason)) => return schema::error_body(&code, &reason),
        }
    }
    let _toml = ctx.toml_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // 写键前的生效配置：rvc 会话参数判断变化与失败回退都以它为旧值。
    let old = ctx.config.snapshot();
    match set_key(ctx.config_path.as_deref(), &request.key, &request.value) {
        Ok(effect) => {
            if effect.restart_required {
                lock_state(&ctx.state).mark_pending_restart(&effect.affected);
            } else if let Err((code, reason)) = apply_runtime_key(ctx, &request.key, &old) {
                return schema::error_body(&code, &reason);
            }
            schema::ok_body(json!({
                "key": request.key,
                "value": request.value,
                "restart_required": effect.restart_required,
                "affected": effect.affected,
            }))
        }
        Err(err) => schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
    }
}

/// 运行时键即时生效（SPEC-04）：清该键状态覆盖，重读 TOML 基线叠加剩余覆盖
/// 重建生效配置，更新共享句柄与文件队列/存储阈值。服务侧每次操作读最新快照。
/// rvc 会话参数先按需重启实时流，成功才替换生效配置；失败保留旧值（TOML 新
/// 基线保留，下一次 rvc 键写入或 daemon 重启再尝试）。
fn apply_runtime_key(ctx: &Ctx, key: &str, old: &Config) -> Result<(), (String, String)> {
    lock_state_file(&ctx.state_file)
        .remove_key(key)
        .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
    let mut fresh = Config::load_from(ctx.config_path.as_deref())
        .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
    let overrides = lock_state_file(&ctx.state_file).overrides().clone();
    state_file::overlay(&mut fresh, &overrides);
    if runtime_key_owner(key) == Some("rvc") {
        services::apply_rvc_session_params(&ctx.state, &ctx.services, old, &fresh)?;
    } else {
        // 其他运行时键不得把尚未应用的 rvc 参数基线静默混入生效配置。
        fresh.rvc = old.rvc.clone();
    }
    ctx.file.apply_file_config(&fresh);
    ctx.config.replace(fresh);
    Ok(())
}

/// config save（SPEC-04）：状态文件覆盖批量写进 TOML 基线，随后清空状态文件。
/// 不改生效值（覆盖本已生效），无需重启。写 TOML 失败则保留状态文件。
fn config_save_reply(ctx: &Ctx) -> Vec<u8> {
    let _toml = ctx.toml_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state_file = lock_state_file(&ctx.state_file);
    let overrides = state_file.overrides().clone();
    match save_overrides(ctx.config_path.as_deref(), &overrides) {
        Ok(saved) => {
            if let Err(err) = state_file.clear() {
                lock_state(&ctx.state).push_degraded(format!("state file clear: {err}"));
            }
            schema::ok_body(json!({"saved": saved}))
        }
        Err(err) => schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
    }
}