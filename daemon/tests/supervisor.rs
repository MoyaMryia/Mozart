mod common;

use common::{call, example, status, wait_exit, wait_until, DaemonProcess};
use mozart_core::ctl::schema::{ProcessState, Status};
use mozart_core::ctl::{self, Seqpacket};
use std::path::PathBuf;
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

#[test]
fn restarts_crashed_child() {
    let daemon = DaemonProcess::start("restart", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    let before = wait_running(&conn);
    let old_pid = before.processes.pre.pid.unwrap();

    // SAFETY: 只向测试内拉起的子进程发送信号。
    unsafe {
        libc::kill(old_pid as i32, libc::SIGKILL);
    }

    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.processes.pre.restarts >= 1
                && status.processes.pre.pid.is_some()
                && status.processes.pre.pid != Some(old_pid)
        }),
        "pre was not restarted"
    );
    let after = status(&conn);
    assert_eq!(after.processes.pre.state, ProcessState::Running);
    assert!(after.processes.pre.last_exit.is_some());
}

#[test]
fn reports_spawn_failure() {
    let missing = std::env::temp_dir().join(format!("mozart-missing-pre-{}", std::process::id()));
    let daemon = DaemonProcess::start("spawnfail", &missing, &example("rvc_stub"));
    let conn = daemon.connect();

    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.processes.pre.restarts >= 1 && status.processes.pre.last_error.is_some()
        }),
        "spawn failure was not reported"
    );
    let status = status(&conn);
    assert_eq!(status.processes.pre.state, ProcessState::Restarting);
    assert!(status.processes.pre.last_error.unwrap().contains("spawn"));
}

#[test]
fn shutdown_terminates_children() {
    let mut daemon = DaemonProcess::start("children", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    let running = wait_running(&conn);
    let pre_pid = running.processes.pre.pid.unwrap();
    let rvc_pid = running.processes.rvc.pid.unwrap();

    let reply = call(&conn, ctl::DAEMON_SHUTDOWN, b"");
    assert_eq!(reply.status, "ok");
    drop(conn);

    let status = wait_exit(&mut daemon.child, Duration::from_secs(5));
    assert!(status.success());
    assert!(
        wait_until(Duration::from_secs(2), || {
            !PathBuf::from(format!("/proc/{pre_pid}")).exists()
                && !PathBuf::from(format!("/proc/{rvc_pid}")).exists()
        }),
        "children survived daemon shutdown"
    );
}
