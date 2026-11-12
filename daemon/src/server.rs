use crate::file::{worker::FileWorker, FileCoordinator};
use crate::poller::Poller;
use crate::services::{self, Services};
use crate::state::{lock_state, Role, State};
use crate::supervisor::Supervisor;
use mozart_core::config::{ensure_run_dir, Config};
use mozart_core::ctl::schema::{
    self, FileCancelRequest, FileStatusRequest, FileSubmitRequest, MuteSetRequest, RouteSetRequest,
    SwitchSetRequest,
};
use mozart_core::ctl::{self, Message, Seqpacket};
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn serve(config: &Config, config_path: Option<&Path>) -> io::Result<()> {
    ensure_run_dir(&config.daemon.run_dir)?;
    std::fs::create_dir_all(config.log_dir())?;
    let _lock = lock_instance(config)?;

    let socket_path = config.daemon_socket();
    let _ = std::fs::remove_file(&socket_path);
    let listener = Seqpacket::bind(&socket_path)?;
    listener.listen(8)?;
    eprintln!("mozart-daemon: listening on {}", socket_path.display());

    let config = Arc::new(config.clone());
    // 子进程与 daemon 读同一 TOML：解析出的路径随 spawn 传递（--config）。
    let config_path = config_path.map(|path| path.to_path_buf());
    let state = Arc::new(Mutex::new(State::new()));
    let services = Arc::new(Services::new(&config));
    let running = Arc::new(AtomicBool::new(true));
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
            config.clone(),
            config_path.clone(),
            state.clone(),
            running.clone(),
        )
    })
    .collect::<io::Result<Vec<_>>>()?;
    // 文件数据面：解码/喂帧/收帧/编码在独立 worker 线程跑，不占控制面。
    let file = Arc::new(FileCoordinator::new(
        config.clone(),
        state.clone(),
        services.clone(),
        running.clone(),
        Arc::new(FileWorker),
    )?);
    let poller = Poller::spawn(
        config.clone(),
        state.clone(),
        services.clone(),
        file.clone(),
        running.clone(),
    )?;

    let outcome = accept_loop(&listener, &state, &services, &file, &config, &running);
    running.store(false, Ordering::Release);
    for supervisor in supervisors {
        supervisor.join();
    }
    poller.join();
    file.shutdown();
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

fn accept_loop(
    listener: &Seqpacket,
    state: &Arc<Mutex<State>>,
    services: &Arc<Services>,
    file: &Arc<FileCoordinator>,
    config: &Arc<Config>,
    running: &Arc<AtomicBool>,
) -> io::Result<()> {
    while running.load(Ordering::Acquire) {
        match listener.wait_readable(Some(Duration::from_millis(100))) {
            Ok(()) => match listener.accept() {
                Ok(conn) => {
                    let state = state.clone();
                    let services = services.clone();
                    let file = file.clone();
                    let config = config.clone();
                    let running = running.clone();
                    thread::spawn(move || {
                        if let Err(err) =
                            serve_connection(&conn, &state, &services, &file, &config, &running)
                        {
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

fn serve_connection(
    conn: &Seqpacket,
    state: &Mutex<State>,
    services: &Services,
    file: &FileCoordinator,
    config: &Config,
    running: &AtomicBool,
) -> io::Result<()> {
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
        let (reply, shutdown) = handle_request(state, services, file, config, &msg);
        let type_ = msg.header.type_;
        let request_id = msg.header.request_id;
        conn.send_timeout(
            type_,
            request_id,
            &reply,
            Some(Duration::from_millis(config.daemon.rpc_timeout_ms)),
        )?;
        if shutdown {
            running.store(false, Ordering::Release);
            return Ok(());
        }
    }
}

fn handle_request(
    state: &Mutex<State>,
    services: &Services,
    file: &FileCoordinator,
    config: &Config,
    msg: &Message,
) -> (Vec<u8>, bool) {
    let type_ = msg.header.type_;
    if type_ == ctl::DAEMON_SHUTDOWN {
        lock_state(state).begin_shutdown();
        return (schema::ok_body(json!({})), true);
    }
    if lock_state(state).is_shutting_down() {
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
            let status = lock_state(state).status();
            match serde_json::to_value(status) {
                Ok(status) => (schema::ok_body(status), false),
                Err(err) => (schema::error_body(ctl::CODE_INTERNAL, &err.to_string()), false),
            }
        }
        ctl::DAEMON_ROUTE_SET => match parse_body::<RouteSetRequest>(&msg.body) {
            Ok(request) => match services::route_set(state, services, config, request.route) {
                Ok(route) => (schema::ok_body(json!({"route": route})), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_MUTE_SET => match parse_body::<MuteSetRequest>(&msg.body) {
            Ok(request) => match services::mute_set(state, services, request.mute) {
                Ok(mute) => (schema::ok_body(json!({"mute": mute})), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_TRANSLATE_SET => match parse_body::<SwitchSetRequest>(&msg.body) {
            Ok(request) => match services::translate_set(services, request.enabled) {
                Ok(data) => (schema::ok_body(data), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_SPEECH_SET => match parse_body::<SwitchSetRequest>(&msg.body) {
            Ok(request) => match services::speech_set(services, request.enabled) {
                Ok(data) => (schema::ok_body(data), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_SPEECH_STOP => match services::speech_stop(state, services) {
            Ok(data) => (schema::ok_body(data), false),
            Err((code, reason)) => (schema::error_body(&code, &reason), false),
        },
        ctl::DAEMON_FILE_SUBMIT => match parse_body::<FileSubmitRequest>(&msg.body) {
            Ok(request) => file_reply(file.submit(request)),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_FILE_PAUSE => file_reply(file.pause()),
        ctl::DAEMON_FILE_RESUME => file_reply(file.resume()),
        ctl::DAEMON_FILE_CANCEL => match parse_body::<FileCancelRequest>(&msg.body) {
            Ok(request) => file_reply(file.cancel(&request.job_id)),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_FILE_STATUS => match parse_body::<FileStatusRequest>(&msg.body) {
            Ok(request) => file_reply(file.status(&request.job_id)),
            Err(reply) => (reply, false),
        },
        ctl::DAEMON_FILE_LIST => file_reply(file.list()),
        ctl::DAEMON_MODEL_LIST => file_reply(
            services::list_models(services).map(|models| json!({"models": models})),
        ),
        _ => (
            schema::error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
            false,
        ),
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
