use crate::state::{lock_state, Role, State};
use crate::supervisor::Supervisor;
use mozart_core::config::Config;
use mozart_core::ctl::schema::{self, MuteSetRequest, RouteSetRequest};
use mozart_core::ctl::{self, Message, Seqpacket};
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn serve(config: &Config) -> io::Result<()> {
    prepare_run_dir(config)?;
    std::fs::create_dir_all(config.log_dir())?;
    let _lock = lock_instance(config)?;

    let socket_path = config.daemon_socket();
    let _ = std::fs::remove_file(&socket_path);
    let listener = Seqpacket::bind(&socket_path)?;
    listener.listen(8)?;
    eprintln!("mozart-daemon: listening on {}", socket_path.display());

    let config = Arc::new(config.clone());
    let state = Arc::new(Mutex::new(State::new()));
    let running = Arc::new(AtomicBool::new(true));
    let supervisors = [
        Supervisor::spawn(Role::Pre, config.clone(), state.clone(), running.clone())?,
        Supervisor::spawn(Role::Rvc, config.clone(), state.clone(), running.clone())?,
    ];

    let outcome = accept_loop(&listener, &state);
    running.store(false, Ordering::Release);
    for supervisor in supervisors {
        supervisor.join();
    }
    drop(listener);
    let _ = std::fs::remove_file(&socket_path);
    outcome
}

fn prepare_run_dir(config: &Config) -> io::Result<()> {
    let dir = &config.daemon.run_dir;
    let created = !dir.exists();
    std::fs::create_dir_all(dir)?;
    if created {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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

enum Flow {
    Continue,
    Shutdown,
}

fn accept_loop(listener: &Seqpacket, state: &Mutex<State>) -> io::Result<()> {
    loop {
        let conn = listener.accept()?;
        match serve_connection(&conn, state) {
            Ok(Flow::Continue) => {}
            Ok(Flow::Shutdown) => return Ok(()),
            Err(err) => eprintln!("mozart-daemon: control connection: {err}"),
        }
    }
}

fn serve_connection(conn: &Seqpacket, state: &Mutex<State>) -> io::Result<Flow> {
    loop {
        let msg = match conn.recv(Some(IDLE_TIMEOUT)) {
            Ok(msg) => msg,
            Err(err)
                if err.kind() == io::ErrorKind::TimedOut
                    || err.kind() == io::ErrorKind::UnexpectedEof =>
            {
                return Ok(Flow::Continue);
            }
            Err(err) => return Err(err),
        };
        let (reply, shutdown) = handle_request(state, &msg);
        let type_ = msg.header.type_;
        let request_id = msg.header.request_id;
        conn.send(type_, request_id, &reply)?;
        if shutdown {
            return Ok(Flow::Shutdown);
        }
    }
}

fn handle_request(state: &Mutex<State>, msg: &Message) -> (Vec<u8>, bool) {
    let mut state = lock_state(state);
    let type_ = msg.header.type_;
    match type_ {
        ctl::DAEMON_HELLO => (
            schema::ok_body(json!({"version": env!("CARGO_PKG_VERSION")})),
            false,
        ),
        ctl::DAEMON_GET_STATUS => match serde_json::to_value(state.status()) {
            Ok(status) => (schema::ok_body(status), false),
            Err(err) => (schema::error_body(ctl::CODE_INTERNAL, &err.to_string()), false),
        },
        ctl::DAEMON_ROUTE_SET => match serde_json::from_slice::<RouteSetRequest>(&msg.body) {
            Ok(request) => match state.set_route(request.route) {
                Ok(route) => (schema::ok_body(json!({"route": route})), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(err) => (
                schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
                false,
            ),
        },
        ctl::DAEMON_MUTE_SET => match serde_json::from_slice::<MuteSetRequest>(&msg.body) {
            Ok(request) => match state.set_mute(request.mute) {
                Ok(mute) => (schema::ok_body(json!({"mute": mute})), false),
                Err((code, reason)) => (schema::error_body(&code, &reason), false),
            },
            Err(err) => (
                schema::error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
                false,
            ),
        },
        ctl::DAEMON_SHUTDOWN => (schema::ok_body(json!({})), true),
        _ => (
            schema::error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
            false,
        ),
    }
}
