use crate::poller::Poller;
use crate::services::{self, Services};
use crate::state::{lock_state, Role, State};
use crate::supervisor::Supervisor;
use mozart_core::config::{ensure_run_dir, Config};
use mozart_core::ctl::schema::{self, MuteSetRequest, RouteSetRequest};
use mozart_core::ctl::{self, Message, Seqpacket};
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn serve(config: &Config) -> io::Result<()> {
    ensure_run_dir(&config.daemon.run_dir)?;
    std::fs::create_dir_all(config.log_dir())?;
    let _lock = lock_instance(config)?;

    let socket_path = config.daemon_socket();
    let _ = std::fs::remove_file(&socket_path);
    let listener = Seqpacket::bind(&socket_path)?;
    listener.listen(8)?;
    eprintln!("mozart-daemon: listening on {}", socket_path.display());

    let config = Arc::new(config.clone());
    let state = Arc::new(Mutex::new(State::new()));
    let services = Arc::new(Services::new(&config));
    let running = Arc::new(AtomicBool::new(true));
    let supervisors = [
        Supervisor::spawn(Role::Pre, config.clone(), state.clone(), running.clone())?,
        Supervisor::spawn(Role::Rvc, config.clone(), state.clone(), running.clone())?,
    ];
    let poller = Poller::spawn(
        config.clone(),
        state.clone(),
        services.clone(),
        running.clone(),
    )?;

    let outcome = accept_loop(&listener, &state, &services, &config, &running);
    running.store(false, Ordering::Release);
    for supervisor in supervisors {
        supervisor.join();
    }
    poller.join();
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
    config: &Arc<Config>,
    running: &Arc<AtomicBool>,
) -> io::Result<()> {
    while running.load(Ordering::Acquire) {
        match listener.wait_readable(Some(Duration::from_millis(100))) {
            Ok(()) => match listener.accept() {
                Ok(conn) => {
                    let state = state.clone();
                    let services = services.clone();
                    let config = config.clone();
                    let running = running.clone();
                    thread::spawn(move || {
                        if let Err(err) =
                            serve_connection(&conn, &state, &services, &config, &running)
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
        let (reply, shutdown) = handle_request(state, services, config, &msg);
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
        ctl::DAEMON_ROUTE_SET => match serde_json::from_slice::<RouteSetRequest>(&msg.body) {
            Ok(request) => match services::route_set(state, services, config, request.route) {
                Ok(route) => (schema::ok_body(json!({"route": route})), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(err) => (
                schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
                false,
            ),
        },
        ctl::DAEMON_MUTE_SET => match serde_json::from_slice::<MuteSetRequest>(&msg.body) {
            Ok(request) => match services::mute_set(state, services, request.mute) {
                Ok(mute) => (schema::ok_body(json!({"mute": mute})), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(err) => (
                schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
                false,
            ),
        },
        _ => (
            schema::error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
            false,
        ),
    }
}
