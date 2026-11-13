//! 控制面补齐（SPEC-04）：SIGTERM/SIGINT 优雅关停、单服务重启、设备枚举。

mod common;

use common::{call, example, status, wait_exit, wait_ready, wait_until, DaemonProcess};
use mozart_core::ctl::schema::{ProcessState, Status};
use mozart_core::ctl::{self, Seqpacket};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn wait_all_running(conn: &Seqpacket) -> Status {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = status(conn);
        let processes = &status.processes;
        let all = [
            &processes.pre,
            &processes.rvc,
            &processes.asr,
            &processes.tts,
            &processes.bridge,
            &processes.llama,
        ]
        .iter()
        .all(|info| info.state == ProcessState::Running && info.pid.is_some());
        if all {
            return status;
        }
        if Instant::now() >= deadline {
            panic!("children did not start: {:?}", status.processes);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pid_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[test]
fn sigterm_triggers_graceful_shutdown() {
    let mut daemon = DaemonProcess::start("sigterm", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    wait_ready(&conn);
    let running = status(&conn);
    let pre_pid = running.processes.pre.pid.unwrap();
    let rvc_pid = running.processes.rvc.pid.unwrap();
    drop(conn);

    // SAFETY: 只向测试内拉起的 daemon 发送信号。
    unsafe {
        libc::kill(daemon.child.id() as i32, libc::SIGTERM);
    }

    let exit = wait_exit(&mut daemon.child, Duration::from_secs(5));
    assert!(exit.success(), "SIGTERM 应走优雅关停而非信号致死：{exit:?}");
    assert!(!daemon.socket.exists(), "socket 未清理");
    assert!(
        wait_until(Duration::from_secs(2), || {
            !pid_alive(pre_pid) && !pid_alive(rvc_pid)
        }),
        "children survived SIGTERM shutdown"
    );
}

#[test]
fn signals_during_shutdown_are_idempotent() {
    let mut daemon = DaemonProcess::start("sigtwice", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    wait_ready(&conn);
    let running = status(&conn);
    let pre_pid = running.processes.pre.pid.unwrap();
    let rvc_pid = running.processes.rvc.pid.unwrap();
    drop(conn);

    // 两发信号紧连：第二发落在关停进行中。handler 常驻，重复信号只再置标志，
    // 不得 panic、不得按默认动作直接杀死进程。
    // SAFETY: 只向测试内拉起的 daemon 发送信号。
    unsafe {
        libc::kill(daemon.child.id() as i32, libc::SIGINT);
        libc::kill(daemon.child.id() as i32, libc::SIGTERM);
    }

    let exit = wait_exit(&mut daemon.child, Duration::from_secs(5));
    assert!(exit.success(), "重复信号应仍优雅关停：{exit:?}");
    assert!(!daemon.socket.exists(), "socket 未清理");
    assert!(
        wait_until(Duration::from_secs(2), || {
            !pid_alive(pre_pid) && !pid_alive(rvc_pid)
        }),
        "children survived repeated-signal shutdown"
    );
}

#[test]
fn restart_single_service_leaves_others_untouched() {
    let daemon = DaemonProcess::start("restart-one", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    let before = wait_all_running(&conn);
    let old_rvc = before.processes.rvc.pid.unwrap();

    let reply = call(&conn, ctl::DAEMON_RESTART, br#"{"service":"rvc"}"#);
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(reply.data["service"], "rvc");
    assert_eq!(reply.data["state"], "restarting");

    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.processes.rvc.state == ProcessState::Running
                && status.processes.rvc.pid.is_some_and(|pid| pid != old_rvc)
        }),
        "rvc was not restarted"
    );
    let after = status(&conn);
    assert_eq!(after.processes.pre.pid, before.processes.pre.pid);
    assert_eq!(after.processes.asr.pid, before.processes.asr.pid);
    assert_eq!(after.processes.tts.pid, before.processes.tts.pid);
    assert_eq!(after.processes.bridge.pid, before.processes.bridge.pid);
    assert_eq!(after.processes.llama.pid, before.processes.llama.pid);
    assert!(
        wait_until(Duration::from_secs(2), || !pid_alive(old_rvc)),
        "old rvc process survived restart"
    );
}

#[test]
fn restart_unknown_service_is_invalid_argument() {
    let daemon = DaemonProcess::start("restart-bad", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    wait_ready(&conn);

    let unknown = call(&conn, ctl::DAEMON_RESTART, br#"{"service":"bogus"}"#);
    assert_eq!(unknown.code, "InvalidArgument");
    assert!(unknown.reason.contains("bogus"), "reason: {}", unknown.reason);

    let malformed = call(&conn, ctl::DAEMON_RESTART, b"not json");
    assert_eq!(malformed.code, "InvalidArgument");

    // 拒绝后整机不受影响。
    let status = status(&conn);
    assert_eq!(status.processes.pre.state, ProcessState::Running);
    assert_eq!(status.processes.rvc.state, ProcessState::Running);
}

#[test]
fn restart_rereads_config() {
    // wrapper 记录 argv 后 exec 真 rvc 桩：重启重读 TOML 后，新子进程应拿到
    // 改过的 socket 路径（SPEC-04 重启重读配置生效）。
    let tag = std::process::id();
    let record = std::env::temp_dir().join(format!("mozart-rvc-argv-{tag}.txt"));
    let script = std::env::temp_dir().join(format!("mozart-rvc-argv-{tag}.sh"));
    let _ = std::fs::remove_file(&record);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexec '{}' \"$@\"\n",
            record.display(),
            example("rvc_stub").display(),
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let daemon = DaemonProcess::start("restart-config", &example("pre_stub"), &script);
    let conn = daemon.connect();
    wait_ready(&conn);
    let before = status(&conn);
    let old_rvc = before.processes.rvc.pid.unwrap();
    let pre_pid = before.processes.pre.pid.unwrap();

    std::fs::write(
        daemon.dir.join("config.toml"),
        "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\nsocket = \"rvc2.sock\"\n",
    )
    .unwrap();
    let reply = call(&conn, ctl::DAEMON_RESTART, br#"{"service":"rvc"}"#);
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);

    assert!(
        wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(&record)
                .map(|text| text.contains("rvc2.sock"))
                .unwrap_or(false)
        }),
        "respawned rvc did not get the re-read config"
    );
    assert!(
        wait_until(Duration::from_secs(2), || !pid_alive(old_rvc)),
        "old rvc process survived restart"
    );
    assert_eq!(status(&conn).processes.pre.pid, Some(pre_pid));

    drop(conn);
    let _ = std::fs::remove_file(&record);
    let _ = std::fs::remove_file(&script);
}

#[test]
fn device_list_returns_entries() {
    let daemon = DaemonProcess::start("devices", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();

    let reply = call(&conn, ctl::DAEMON_DEVICE_LIST, b"{}");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    // 主机可能无声卡：容忍空清单；有条目则字段必须完整、方向合法。
    let devices = reply.data["devices"].as_array().expect("devices 应为数组");
    for device in devices {
        let id = device["id"].as_str().expect("id 应为字符串");
        let label = device["label"].as_str().expect("label 应为字符串");
        let kind = device["kind"].as_str().expect("kind 应为字符串");
        assert!(!id.is_empty());
        assert!(!label.is_empty());
        assert!(kind == "capture" || kind == "playback", "非法方向：{kind}");
    }
}

#[test]
fn config_set_validates_and_get_reads_baseline() {
    let daemon = DaemonProcess::start_with_config(
        "config-set",
        &example("pre_stub"),
        &example("rvc_stub"),
        "[daemon]\nrpc_timeout_ms = 300\n\n[pre]\ncapture_device = \"default\"\n\n[rvc]\nmodel_id = \"demo\"\n",
    );
    let conn = daemon.connect();

    // 不支持的键：core 分类拒绝。
    let reply = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"rvc.pitch_shift","value":"3"}"#,
    );
    assert_eq!(reply.status, "error", "reason: {}", reply.reason);

    // 不存在的设备：防呆拒绝（对照当前清单）。
    let reply = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"pre.capture_device","value":"plughw:CARD=NoSuchCard,DEV=99"}"#,
    );
    assert_eq!(reply.status, "error", "reason: {}", reply.reason);

    // config get 读基线值。
    let reply = call(
        &conn,
        ctl::DAEMON_CONFIG_GET,
        br#"{"key":"pre.capture_device"}"#,
    );
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(reply.data["value"].as_str(), Some("default"));

    // 被拒绝的 set 未改动基线。
    let config = std::fs::read_to_string(daemon.dir.join("config.toml")).unwrap();
    assert!(config.contains("capture_device = \"default\""), "config: {config}");
}