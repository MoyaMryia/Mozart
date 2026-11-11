use mozart_core::ctl::schema::{ProcessState, Response, Route, Status};
use mozart_core::ctl::{self, Seqpacket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct DaemonProcess {
    child: Child,
    dir: PathBuf,
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(tag: &str) -> (DaemonProcess, PathBuf) {
    let dir = std::env::temp_dir().join(format!("mozart-daemon-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(&config, "").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_mozart-daemon"))
        .arg("--config")
        .arg(&config)
        .env("MOZART_RUN_DIR", &dir)
        .env("MOZART_PRE_BIN", "unused-pre")
        .env("MOZART_RVC_BIN", "unused-rvc")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let socket = dir.join("daemon.sock");
    (DaemonProcess { child, dir }, socket)
}

fn connect(socket: &Path) -> Seqpacket {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Seqpacket::connect(socket) {
            Ok(conn) => return conn,
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!("connect {}: {err}", socket.display());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn call(conn: &Seqpacket, type_: u16, body: &[u8]) -> Response {
    conn.send(type_, 7, body).unwrap();
    let msg = conn.recv(Some(Duration::from_secs(2))).unwrap();
    let reply_type = msg.header.type_;
    assert_eq!(reply_type, type_, "响应 type 必须回显");
    serde_json::from_slice(&msg.body).unwrap()
}

fn wait_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("daemon did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serves_status_and_validates_commands() {
    let (_daemon, socket) = start("status");
    let conn = connect(&socket);

    let hello = call(&conn, ctl::DAEMON_HELLO, b"");
    assert_eq!(hello.status, "ok");
    assert_eq!(hello.data["version"], env!("CARGO_PKG_VERSION"));

    let reply = call(&conn, ctl::DAEMON_GET_STATUS, b"");
    let status: Status = serde_json::from_value(reply.data).unwrap();
    assert_eq!(status.route.desired, Route::Direct);
    assert_eq!(status.route.active, Route::Direct);
    assert_eq!(status.processes.pre.state, ProcessState::Stopped);
    assert_eq!(status.processes.rvc.state, ProcessState::Stopped);
    assert!(status.pre.is_none());
    assert!(status.rvc.is_none());
    assert!(status.hw.is_none());
    assert!(status.degraded.is_empty());
    assert_eq!(status.daemon.version, env!("CARGO_PKG_VERSION"));

    let direct = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"direct"}"#);
    assert_eq!(direct.status, "ok");
    assert_eq!(direct.data["route"], "direct");

    let rvc = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"rvc"}"#);
    assert_eq!(rvc.status, "error");
    assert_eq!(rvc.code, "NotReady");

    let mute = call(&conn, ctl::DAEMON_MUTE_SET, br#"{"mute":true}"#);
    assert_eq!(mute.code, "NotReady");

    let invalid_route = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"bogus"}"#);
    assert_eq!(invalid_route.code, "InvalidArgument");

    let invalid_json = call(&conn, ctl::DAEMON_ROUTE_SET, b"not json");
    assert_eq!(invalid_json.code, "InvalidArgument");

    let unknown = call(&conn, 0x01FF, b"");
    assert_eq!(unknown.code, "UnknownCommand");
}

#[test]
fn shutdown_stops_daemon() {
    let (mut daemon, socket) = start("shutdown");
    let conn = connect(&socket);

    let reply = call(&conn, ctl::DAEMON_SHUTDOWN, b"");
    assert_eq!(reply.status, "ok");
    drop(conn);

    let status = wait_exit(&mut daemon.child, Duration::from_secs(3));
    assert!(status.success());
    assert!(!socket.exists());
}

#[test]
fn second_instance_is_refused() {
    let (daemon, socket) = start("lock");
    let _conn = connect(&socket);

    let mut second = Command::new(env!("CARGO_BIN_EXE_mozart-daemon"))
        .arg("--config")
        .arg(daemon.dir.join("config.toml"))
        .env("MOZART_RUN_DIR", &daemon.dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = wait_exit(&mut second, Duration::from_secs(3));
    assert!(!status.success());
}
