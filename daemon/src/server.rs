use crate::device;
use crate::file::{worker::FileWorker, FileCoordinator};
use crate::poller::Poller;
use crate::preset::PresetManager;
use crate::runtime::SharedConfig;
use crate::services::{self, Services};
use crate::state::{lock_state, Role, State};
use crate::state_file::{self, BridgeRestore, StateFile};
use crate::supervisor::{RestartHandle, Supervisor};
use mozart_core::config::{
    device_key_kind, ensure_run_dir, get_key, runtime_key_owner, save_overrides, set_key,
    state_file_path, validate_device_name, Config,
};
use mozart_core::ctl::schema::{
    self, ConfigGetRequest, ConfigSetRequest, DeviceKind, DeviceListRequest, DeviceUseRequest,
    FileCancelRequest, FileFetchRequest, FileStatusRequest, FileSubmitRequest, MuteSetRequest,
    RestartRequest, RouteSetRequest, RvcParams, SwitchSetRequest,
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
    presets: Arc<PresetManager>,
    config: SharedConfig,
    state_file: Arc<Mutex<StateFile>>,
    bridge_restore: Arc<Mutex<BridgeRestore>>,
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
    let mut initial_state = State::new();
    crate::mode::initialize(&mut initial_state, &effective);
    let mut restore = state_file.bridge_restore();
    if initial_state.output_mode.is_some() {
        restore.speech = None;
    }
    let bridge_restore = Arc::new(Mutex::new(restore));
    let state_file = Arc::new(Mutex::new(state_file));
    let shared = SharedConfig::new(effective);

    // 子进程与 daemon 读同一 TOML：解析出的路径随 spawn 传递（--config）。
    let config_path = config_path.map(|path| path.to_path_buf());
    let state = Arc::new(Mutex::new(initial_state));
    let services = Arc::new(Services::new(&shared));
    let running = Arc::new(AtomicBool::new(true));
    // 预设载入与 config set/save 共用 TOML 串行锁与 transition 锁。
    let toml_lock = Arc::new(Mutex::new(()));
    let presets = Arc::new(PresetManager::new(
        shared.clone(),
        state.clone(),
        services.clone(),
        state_file.clone(),
        config_path.clone(),
        toml_lock.clone(),
    ));
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
        Role::Enhance,
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
        presets.clone(),
        running.clone(),
        Arc::new(FileWorker),
    )?);
    let poller = Poller::spawn(
        shared.clone(),
        state.clone(),
        services.clone(),
        presets.clone(),
        file.clone(),
        bridge_restore.clone(),
        state_file.clone(),
        running.clone(),
    )?;

    let ctx = Ctx {
        state,
        services,
        file,
        presets,
        config: shared,
        state_file,
        bridge_restore,
        config_path,
        restarts,
        toml_lock,
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

fn lock_bridge_restore(restore: &Mutex<BridgeRestore>) -> MutexGuard<'_, BridgeRestore> {
    restore
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
            let effective = ctx.config.snapshot();
            let rvc = &effective.rvc;
            status.rvc_params = Some(RvcParams {
                pitch_shift: rvc.pitch_shift,
                index_rate: rvc.index_rate,
                rms_mix_rate: rvc.rms_mix_rate,
                protect: rvc.protect,
                f0_method: rvc.f0_method.clone(),
            });
            match serde_json::to_value(status) {
                Ok(mut status) => {
                    status["daemon"]["poll_interval_ms"] = json!(effective.daemon.poll_interval_ms);
                    status["file_limits"] =
                        json!({"max_upload_bytes": effective.file.max_upload_bytes});
                    status["api_capabilities"] = json!(["file_fetch_safe_errors_v1"]);
                    (schema::ok_body(status), false)
                }
                Err(err) => (
                    schema::error_body(ctl::CODE_INTERNAL, &err.to_string()),
                    false,
                ),
            }
        }
        ctl::DAEMON_OUTPUT_MODE_SET => match parse_body::<schema::OutputModeSetRequest>(&msg.body) {
            Ok(request) => {
                let _transition = ctx.services.transition();
                let mut restore = lock_bridge_restore(&ctx.bridge_restore);
                restore.speech = None;
                let mode = crate::mode::request(&mut lock_state(&ctx.state), request.mode);
                (schema::ok_body(json!(mode)), false)
            }
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_ROUTE_SET => match parse_body::<RouteSetRequest>(&msg.body) {
            Ok(request) => {
                let _transition = ctx.services.transition();
                crate::mode::unmanage(&mut lock_state(&ctx.state));
                let config = ctx.config.snapshot();
                match services::route_set_locked(
                    &ctx.state,
                    &ctx.services,
                    &config,
                    &ctx.presets,
                    request.route,
                ) {
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
            Ok(request) => apply_bridge_switch(
                ctx,
                "bridge.translate_enabled",
                request.enabled,
                || services::translate_set(&ctx.services, request.enabled),
            ),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_SPEECH_SET => match parse_body::<SwitchSetRequest>(&msg.body) {
            Ok(request) => (speech_set_reply(ctx, request.enabled), false),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_SPEECH_STOP => (speech_stop_reply(ctx, &msg.body), false),
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
        ctl::DAEMON_MODEL_LIST => {
            file_reply(services::list_models(&ctx.services).map(|models| json!({"models": models})))
        }
        ctl::DAEMON_MODEL_USE => match parse_body::<schema::ModelUseRequest>(&msg.body) {
            Ok(request) => (model_use_reply(ctx, &request.model_id), false),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_RESTART => match parse_body::<RestartRequest>(&msg.body) {
            Ok(request) => (restart_service(ctx, &request.service), false),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_DEVICE_LIST => match parse_body::<DeviceListRequest>(&msg.body) {
            Ok(request) => device::list_reply(request.all, device::list),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_DEVICE_USE => match parse_body::<DeviceUseRequest>(&msg.body) {
            Ok(request) => (device_use_reply(ctx, &request), false),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_CONFIG_SET => match parse_body::<ConfigSetRequest>(&msg.body) {
            Ok(request) => (config_set_reply(ctx, &request), false),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_CONFIG_GET => match parse_body::<ConfigGetRequest>(&msg.body) {
            Ok(request) => match get_key(ctx.config_path.as_deref(), request.key.as_deref()) {
                // 读到的基线值可能尚未生效：待重启键恒随应答给出，无则空数组。
                Ok(value) => {
                    let pending = lock_state(&ctx.state).pending_restart_keys();
                    let data = json!({"value": value, "pending_restart_keys": pending});
                    (schema::ok_body(data), false)
                }
                Err(err) => (
                    schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
                    false,
                ),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_CONFIG_SAVE => (config_save_reply(ctx), false),
        ctl::DAEMON_PRESET_SAVE => preset_reply(ctx.presets.save_current()),
        ctl::DAEMON_PRESET_USE => preset_reply(ctx.presets.use_default()),
        ctl::DAEMON_PRESET_LIST => preset_reply(ctx.presets.list()),
        _ => (
            schema::error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
            false,
        ),
    }
}

/// 字幕开关转发与存档在 bridge_restore 锁内串行执行，避免与 Poller 的
/// 重载/恢复逻辑交错覆写（锁序：bridge_restore → state_file / services.bridge）。
/// 落盘失败只记降级：开关已在桥生效且内存 overrides 已更新供桥重启恢复，
/// 快照滞后原因经 status 可查，不阻断命令（仅 daemon 自身重启后会失去未落盘更改）。
fn apply_bridge_switch<F>(ctx: &Ctx, key: &str, enabled: bool, call: F) -> (Vec<u8>, bool)
where
    F: FnOnce() -> Result<serde_json::Value, (String, String)>,
{
    let mut restore = lock_bridge_restore(&ctx.bridge_restore);
    match call() {
        Ok(data) => {
            save_bridge_switch(ctx, &mut restore, key, enabled);
            (schema::ok_body(data), false)
        }
        Err((code, reason)) => (schema::error_body(&code, &reason), false),
    }
}

fn save_bridge_switch(ctx: &Ctx, restore: &mut BridgeRestore, key: &str, enabled: bool) {
    if let Err(err) = lock_state_file(&ctx.state_file).set_key(key, json!(enabled)) {
        lock_state(&ctx.state).push_degraded(format!("state file write {key}: {err}"));
    }
    match key {
        "bridge.translate_enabled" => restore.translate = None,
        "bridge.speech_enabled" => restore.speech = None,
        _ => {}
    }
}

fn speech_set_reply(ctx: &Ctx, enabled: bool) -> Vec<u8> {
    let _transition = ctx.services.transition();
    crate::mode::unmanage(&mut lock_state(&ctx.state));
    let mut restore = lock_bridge_restore(&ctx.bridge_restore);
    if enabled && restore.speech_stop.is_some() {
        if let Err((code, reason)) = services::advance_speech_stop(&ctx.state, &ctx.services, &ctx.state_file, &mut restore) {
            return schema::error_body(&code, &reason);
        }
    }
    match services::speech_set(&ctx.services, enabled) {
        Ok(data) => {
            save_bridge_switch(ctx, &mut restore, "bridge.speech_enabled", enabled);
            schema::ok_body(data)
        }
        Err((code, reason)) => schema::error_body(&code, &reason),
    }
}

fn speech_stop_reply(ctx: &Ctx, body: &[u8]) -> Vec<u8> {
    if !body.is_empty() && !serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(body).is_ok_and(|value| value.is_empty()) {
        return schema::error_body(ctl::CODE_INVALID_ARGUMENT, "speech stop body must be an empty object");
    }
    let _transition = ctx.services.transition();
    crate::mode::unmanage(&mut lock_state(&ctx.state));
    let mut restore = lock_bridge_restore(&ctx.bridge_restore);
    match services::advance_speech_stop(&ctx.state, &ctx.services, &ctx.state_file, &mut restore) {
        Ok(data) => schema::ok_body(data),
        Err((code, reason)) => schema::error_body(&code, &reason),
    }
}

/// 单服务重启（SPEC-04 重启两粒度）：入口重读 TOML，失败拒绝、不动运行进程；
/// 成功则交监管线程停子进程后以新配置重起，整栈不受影响。重启受理即清待重启标记。
fn restart_service(ctx: &Ctx, service: &str) -> Vec<u8> {
    let _transition = ctx.services.transition();
    let _toml = ctx
        .toml_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    let active = ctx.config.snapshot();
    if matches!(role, Role::Pre | Role::Enhance)
        && config.enhance.is_some() != active.enhance.is_some()
    {
        return schema::error_body(
            ctl::CODE_INVALID_ARGUMENT,
            "changing the denoise backend requires restarting daemon",
        );
    }
    let coupled = matches!(role, Role::Pre | Role::Enhance) && config.enhance.is_some();
    if matches!(role, Role::Pre) || coupled {
        let mut live = (*active).clone();
        // 显式 restart pre 清空 device use 临时设备，回到 TOML 基线。
        live.pre.capture_device = config.pre.capture_device.clone();
        live.pre.playback_device = config.pre.playback_device.clone();
        if coupled {
            ctx.services.refresh_enhance(&config);
            live.enhance = config.enhance.clone();
            live.process.enhance_bin = config.process.enhance_bin.clone();
        }
        ctx.config.replace(live);
    }
    let config = Arc::new(config);
    for handle in ctx.restarts.iter().filter(|handle| {
        handle.role() == role || (coupled && matches!(handle.role(), Role::Pre | Role::Enhance))
    }) {
        if handle.restart(config.clone()).is_err() {
            return schema::error_body(ctl::CODE_BUSY, "daemon is shutting down");
        }
        lock_state(&ctx.state).clear_pending_restart(handle.role().name());
    }
    schema::ok_body(json!({"service": service, "state": "restarting"}))
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

fn preset_reply<T: serde::Serialize>(result: Result<T, (String, String)>) -> (Vec<u8>, bool) {
    match result {
        Ok(data) => match serde_json::to_value(data) {
            Ok(data) => (schema::ok_body(data), false),
            Err(err) => (
                schema::error_body(ctl::CODE_INTERNAL, &err.to_string()),
                false,
            ),
        },
        Err((code, reason)) => (schema::error_body(&code, &reason), false),
    }
}

fn validate_device_exists(device: &str, kind: &str) -> Result<(), (String, String)> {
    match device::exists(device, kind) {
        Ok(true) => Ok(()),
        Ok(false) => Err((
            ctl::CODE_INVALID_ARGUMENT.to_string(),
            format!("设备不存在：{device}（{kind}；用 device list 查看可用设备）"),
        )),
        Err(err) => Err(err),
    }
}

fn device_use_reply(ctx: &Ctx, request: &DeviceUseRequest) -> Vec<u8> {
    if let Err(err) = validate_device_name(&request.device, request.kind.config_key()) {
        return schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string());
    }
    if let Err((code, reason)) = validate_device_exists(&request.device, request.kind.as_str()) {
        return schema::error_body(&code, &reason);
    }
    let _transition = ctx.services.transition();
    match ctx.services.set_device(request) {
        Ok(data) => {
            let mut live = (*ctx.config.snapshot()).clone();
            match request.kind {
                DeviceKind::Capture => live.pre.capture_device = request.device.clone(),
                DeviceKind::Playback => live.pre.playback_device = request.device.clone(),
            }
            ctx.config.replace(live);
            schema::ok_body(data)
        }
        Err((code, reason)) => schema::error_body(&code, &reason),
    }
}

/// config set（SPEC-04）：设备键先对照当前清单校验存在（防呆），再原子写基线，
/// 回重启提示契约 restart_required + affected，并登记待重启。daemon 运行时键与
/// rvc 会话参数键写基线后即时应用：daemon/file 键清覆盖并重建生效配置；rvc 会话
/// 参数按变更键下发 SetParams 或 StopStream→StartStream，失败保留旧生效配置。其余
/// 键由 core::set_key 分类拒绝。
fn config_set_reply(ctx: &Ctx, request: &ConfigSetRequest) -> Vec<u8> {
    if let Some(kind) = device_key_kind(&request.key) {
        if let Err((code, reason)) = validate_device_exists(&request.value, kind) {
            return schema::error_body(&code, &reason);
        }
    }
    // 锁顺序 transition → toml：预设载入在持 transition 时也要写 TOML 基线，
    // 反向加锁会与预设路径互等。非 rvc 键也持 transition，代价是短暂延后轮询。
    let _transition = ctx.services.transition();
    let _toml = ctx
        .toml_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // 写键前的生效配置：rvc 会话参数判断变化与失败回退都以它为旧值。
    let old = ctx.config.snapshot();
    match set_key(ctx.config_path.as_deref(), &request.key, &request.value) {
        Ok(effect) => {
            if effect.restart_required {
                lock_state(&ctx.state).mark_pending_restart(&request.key, &effect.affected);
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

/// 实时换模型（SPEC-04 `model use <id>`）：StopStream → LoadModel → 预设提交
/// （model_id 基线 + 五键基线 + applied + 生效配置）→ StartStream，全程持
/// transition 锁。编排与失败恢复见 `services::switch_rvc_model_locked`：恢复
/// 一律用切换前的生效配置（旧模型 + 旧参数），不套用目标预设。
fn model_use_reply(ctx: &Ctx, model_id: &str) -> Vec<u8> {
    let _transition = ctx.services.transition();
    let old = ctx.config.snapshot();
    match services::switch_rvc_model_locked(&ctx.state, &ctx.services, &ctx.presets, &old, model_id)
    {
        Ok(previous) => schema::ok_body(json!({
            "model_id": model_id,
            "previous_model_id": previous,
        })),
        Err((code, reason)) => schema::error_body(&code, &reason),
    }
}

/// 运行时键即时生效（SPEC-04）：清该键状态覆盖，重读 TOML 基线叠加剩余覆盖
/// 重建生效配置，更新共享句柄与文件队列/存储阈值。服务侧每次操作读最新快照。
/// rvc 会话键的生效配置只吸收本次写入的键（见 services::merge_session_key），
/// 再按变更键下发 SetParams 或重建实时流；编排与替换同处 transition 临界区，失败不替换生效配置
/// （TOML 新基线保留，下一次对应键写入或 daemon 重启再尝试）。
/// 调用方须已持 transition 锁。
fn apply_runtime_key(ctx: &Ctx, key: &str, old: &Config) -> Result<(), (String, String)> {
    lock_state_file(&ctx.state_file)
        .remove_key(key)
        .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
    let mut fresh = Config::load_from(ctx.config_path.as_deref())
        .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
    let overrides = lock_state_file(&ctx.state_file).overrides().clone();
    state_file::overlay(&mut fresh, &overrides);
    let requested_dsp = fresh.pre.dsp.clone();
    crate::runtime::preserve_live_pre_state(&mut fresh, old);
    if runtime_key_owner(key) == Some("pre") {
        match key {
            "pre.dsp.denoise" => fresh.pre.dsp.denoise = requested_dsp.denoise,
            "pre.dsp.gate" => fresh.pre.dsp.gate = requested_dsp.gate,
            "pre.dsp.agc" => fresh.pre.dsp.agc = requested_dsp.agc,
            "pre.dsp.limiter" => fresh.pre.dsp.limiter = requested_dsp.limiter,
            _ => {
                return Err((
                    ctl::CODE_INTERNAL.to_string(),
                    format!("no DSP orchestration for {key}"),
                ))
            }
        }
        ctx.services.set_dsp(&fresh.pre.dsp).map_err(|(code, reason)| (
            code,
            format!("pre SetDsp: {reason}; effective DSP unchanged; TOML baseline kept; retry config set for the same key or restart daemon to apply"),
        ))?;
    }
    if runtime_key_owner(key) == Some("rvc") {
        // 未生效的 rvc 基线（如失败恢复留下的 pitch）不得混入生效配置，
        // 否则 status 与在跑流不一致。merge_session_key 从旧生效配置克隆、
        // 只改本次写入的键，model_id 等非会话键天然保留。
        fresh.rvc = services::merge_session_key(key, &old.rvc, &fresh.rvc)?;
        // 替换与 Stop/Start 同处调用方持有的 transition 临界区：释放后再替换
        // 会给轮询留下用旧快照起流的窄窗口。失败路径不替换，TOML 新基线保留。
        services::apply_rvc_session_params_locked(&ctx.state, &ctx.services, &old.rvc, &fresh.rvc)
            .map_err(|(code, reason)| {
                (
                    code,
                    format!(
                        "{reason}; TOML baseline kept; retry config set for the same key or restart daemon to apply"
                    ),
                )
            })?;
        ctx.file.apply_file_config(&fresh);
        ctx.config.replace(fresh);
    } else {
        // 其他运行时键不得把尚未应用的 rvc 参数基线静默混入生效配置。
        fresh.rvc = old.rvc.clone();
        ctx.file.apply_file_config(&fresh);
        ctx.config.replace(fresh);
    }
    Ok(())
}

/// config save（SPEC-04）：状态文件覆盖批量写进 TOML 基线，随后清空状态文件。
/// 不改生效值（覆盖本已生效），无需重启。写 TOML 失败则保留状态文件。
fn config_save_reply(ctx: &Ctx) -> Vec<u8> {
    let _toml = ctx
        .toml_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
