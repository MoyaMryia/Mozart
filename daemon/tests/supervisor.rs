mod common;

use common::{call, example, status, wait_exit, wait_until, DaemonProcess};
use mozart_core::ctl::schema::{ProcessState, Status};
use mozart_core::ctl::{self, Seqpacket};
use std::os::unix::fs::PermissionsExt;
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

#[test]
fn passes_socket_and_config_to_child() {
    // wrapper 记录 argv 后 exec 真 pre 桩：验证 daemon 解析出的配置路径与
    // 控制 socket 路径确实传进子进程参数（SPEC-04 spawn 契约）。
    let tag = std::process::id();
    let record = std::env::temp_dir().join(format!("mozart-pre-argv-{tag}.txt"));
    let script = std::env::temp_dir().join(format!("mozart-pre-argv-{tag}.sh"));
    let _ = std::fs::remove_file(&record);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexec '{}' \"$@\"\n",
            record.display(),
            example("pre_stub").display(),
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let daemon = DaemonProcess::start("argv", &script, &example("rvc_stub"));
    let conn = daemon.connect();
    wait_running(&conn);

    // daemon 在 fork 后即报 Running，wrapper 的 printf 可能尚未落盘；等文件出现。
    assert!(
        wait_until(Duration::from_secs(5), || record.exists()),
        "pre argv record was not written"
    );
    let argv: Vec<String> = std::fs::read_to_string(&record)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let config = daemon.dir.join("config.toml");
    let socket = daemon.dir.join("pre.sock");
    let value_after = |flag: &str| {
        argv.iter()
            .position(|arg| arg == flag)
            .and_then(|index| argv.get(index + 1))
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(value_after("--socket"), socket.to_str().unwrap());
    assert_eq!(value_after("--config"), config.to_str().unwrap());

    drop(conn);
    let _ = std::fs::remove_file(&record);
    let _ = std::fs::remove_file(&script);
}
