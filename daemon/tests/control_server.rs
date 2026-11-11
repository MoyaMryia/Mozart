mod common;

use common::{call, example, status, wait_exit, wait_until, DaemonProcess};
use mozart_core::ctl::schema::{Activity, ProcessState, Route};
use mozart_core::ctl::{self};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn serves_status_and_validates_commands() {
    let daemon = DaemonProcess::start("status", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();

    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.processes.pre.state == ProcessState::Running
                && status.processes.rvc.state == ProcessState::Running
        }),
        "children did not start"
    );

    let hello = call(&conn, ctl::DAEMON_HELLO, b"");
    assert_eq!(hello.status, "ok");
    assert_eq!(hello.data["version"], env!("CARGO_PKG_VERSION"));

    let reply = call(&conn, ctl::DAEMON_GET_STATUS, b"");
    let status: mozart_core::ctl::schema::Status = serde_json::from_value(reply.data).unwrap();
    assert_eq!(status.route.desired, Route::Direct);
    assert_eq!(status.route.active, Route::Direct);
    assert_eq!(status.processes.pre.state, ProcessState::Running);
    assert_eq!(status.processes.rvc.state, ProcessState::Running);
    assert!(status.processes.pre.pid.is_some());
    assert_eq!(status.daemon.version, env!("CARGO_PKG_VERSION"));

    let direct = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"direct"}"#);
    assert_eq!(direct.status, "ok");
    assert_eq!(direct.data["route"], "direct");

    let invalid_route = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"bogus"}"#);
    assert_eq!(invalid_route.code, "InvalidArgument");

    let invalid_json = call(&conn, ctl::DAEMON_ROUTE_SET, b"not json");
    assert_eq!(invalid_json.code, "InvalidArgument");

    let unknown = call(&conn, 0x01FF, b"");
    assert_eq!(unknown.code, "UnknownCommand");
}

#[test]
fn switch_applies_route_and_mute() {
    let daemon = DaemonProcess::start("switch", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.processes.pre.state == ProcessState::Running
                && status.processes.rvc.state == ProcessState::Running
        }),
        "children did not start"
    );

    let rvc = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"rvc"}"#);
    assert_eq!(rvc.status, "ok");
    assert_eq!(rvc.data["route"], "rvc");
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.route.active == Route::Rvc
                && status.arbiter.gpu.owner == Some(Activity::Realtime)
                && status.pre.as_ref().map(|pre| pre.route) == Some(Route::Rvc)
                && status.rvc.as_ref().map(|rvc| rvc.state.as_str()) == Some("streaming")
        }),
        "route rvc was not applied"
    );

    let mute = call(&conn, ctl::DAEMON_MUTE_SET, br#"{"mute":true}"#);
    assert_eq!(mute.status, "ok");
    assert!(
        wait_until(Duration::from_secs(2), || {
            let status = status(&conn);
            status.mute && status.pre.as_ref().map(|pre| pre.mute) == Some(true)
        }),
        "mute was not applied"
    );

    let direct = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"direct"}"#);
    assert_eq!(direct.status, "ok");
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.route.active == Route::Direct
                && status.arbiter.gpu.owner.is_none()
                && status.pre.as_ref().map(|pre| pre.route) == Some(Route::Direct)
                && status.rvc.as_ref().map(|rvc| rvc.state.as_str()) == Some("loaded")
        }),
        "route direct was not applied"
    );
}

#[test]
fn idle_connection_does_not_block_control() {
    let daemon = DaemonProcess::start("idle", &example("pre_stub"), &example("rvc_stub"));
    let _idle = daemon.connect();
    let conn = daemon.connect();

    let reply = call(&conn, ctl::DAEMON_GET_STATUS, b"");
    assert_eq!(reply.status, "ok");
}

#[test]
fn shutdown_stops_daemon() {
    let mut daemon = DaemonProcess::start("shutdown", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();

    let reply = call(&conn, ctl::DAEMON_SHUTDOWN, b"");
    assert_eq!(reply.status, "ok");
    drop(conn);

    let status = wait_exit(&mut daemon.child, Duration::from_secs(5));
    assert!(status.success());
    assert!(!daemon.socket.exists());
}

#[test]
fn second_instance_is_refused() {
    let daemon = DaemonProcess::start("lock", &example("pre_stub"), &example("rvc_stub"));
    let _conn = daemon.connect();

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
