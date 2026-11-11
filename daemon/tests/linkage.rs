mod common;

use common::{call, example, status, wait_until, DaemonProcess};
use mozart_core::ctl::schema::{ProcessState, Route, Status};
use mozart_core::ctl::{self, Seqpacket};
use std::time::{Duration, Instant};

fn wait_running(conn: &Seqpacket) -> Status {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = status(conn);
        if status.processes.pre.state == ProcessState::Running
            && status.processes.rvc.state == ProcessState::Running
        {
            return status;
        }
        if Instant::now() >= deadline {
            panic!("children did not start: {:?}", status.processes);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn kill(pid: u32) {
    // SAFETY: 只向测试内拉起的子进程发送信号。
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[test]
fn reapplies_route_and_stream_after_restart() {
    let daemon = DaemonProcess::start("reapply", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    wait_running(&conn);

    let reply = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"rvc"}"#);
    assert_eq!(reply.status, "ok");
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.rvc.as_ref().map(|rvc| rvc.state.as_str()) == Some("streaming")
                && status.pre.as_ref().map(|pre| pre.route) == Some(Route::Rvc)
        }),
        "route rvc was not applied"
    );

    let rvc_pid = status(&conn).processes.rvc.pid.unwrap();
    kill(rvc_pid);
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            let new_pid = status.processes.rvc.pid;
            new_pid.is_some()
                && new_pid != Some(rvc_pid)
                && status.rvc.as_ref().map(|rvc| rvc.state.as_str()) == Some("streaming")
        }),
        "rvc stream was not reapplied after restart"
    );

    let pre_pid = status(&conn).processes.pre.pid.unwrap();
    kill(pre_pid);
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            let new_pid = status.processes.pre.pid;
            new_pid.is_some()
                && new_pid != Some(pre_pid)
                && status.pre.as_ref().map(|pre| pre.route) == Some(Route::Rvc)
        }),
        "pre route was not reapplied after restart"
    );
}

#[test]
fn route_rejected_without_model_id() {
    let daemon = DaemonProcess::start_with_config(
        "nomodel",
        &example("pre_stub"),
        &example("rvc_stub"),
        "[daemon]\nrpc_timeout_ms = 300\n",
    );
    let conn = daemon.connect();
    wait_running(&conn);

    let reply = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"rvc"}"#);
    assert_eq!(reply.status, "error");
    assert_eq!(reply.code, "NotReady");
    assert!(reply.reason.contains("model_id"));
}
