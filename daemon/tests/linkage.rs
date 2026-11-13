mod common;

use common::{call, call_timeout, example, status, wait_until, DaemonProcess};
use mozart_core::ctl::schema::{ProcessState, Route, Status};
use mozart_core::ctl::{self, Seqpacket};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

fn wait_running(conn: &Seqpacket) -> Status {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = status(conn);
        // 控制面就绪：route set 直接向 rvc 发 GetStatus，进程 Running 不代表桩
        // 已绑定控制 socket；语言进程启动竞争会放大这段窗口。
        if status.processes.pre.state == ProcessState::Running
            && status.processes.rvc.state == ProcessState::Running
            && status.rvc.is_some()
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

/// 权威对账（SPEC-04 单一权威根因治愈）：daemon 启动即断言 pre 实时态
/// （mode_asserted=true、非文件接管）；pre 重启后 startup_id 变化，daemon 据此
/// 重新断言，pre 恢复实时态。旧代码 pre 启动单边绑定/恒发、重启无条件 reclaim；
/// 治愈后 pre 启动/重启均静默等 daemon 断言，不单边行动。
#[test]
fn reasserts_pre_mode_after_restart() {
    let daemon = DaemonProcess::start(
        "pre-mode-reassert",
        &example("pre_stub"),
        &example("rvc_stub"),
    );
    let conn = daemon.connect();
    wait_running(&conn);

    // 启动断言：daemon 首轮对账即置 pre 实时态（pre 启动静默，等断言才激活）。
    assert!(
        wait_until(Duration::from_secs(5), || {
            status(&conn)
                .pre
                .is_some_and(|pre| pre.mode_asserted && !pre.file_mode)
        }),
        "daemon 应在启动后断言 pre 实时态"
    );
    let first_startup_id = status(&conn).pre.unwrap().startup_id;
    assert!(!first_startup_id.is_empty(), "pre 应上报启动号供重启识别");

    // kill pre → 监管重启 → startup_id 变化 → daemon 重新断言实时态。
    let pre_pid = status(&conn).processes.pre.pid.unwrap();
    kill(pre_pid);
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            let new_pid = status.processes.pre.pid;
            new_pid.is_some()
                && new_pid != Some(pre_pid)
                && status.pre.as_ref().is_some_and(|pre| {
                    pre.mode_asserted && !pre.file_mode && pre.startup_id != first_startup_id
                })
        }),
        "pre 重启后 daemon 应据 startup_id 重新断言实时态"
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

#[test]
fn route_set_answer_matches_final_state_under_race() {
    // 用延迟桩拉长 rvc 切换窗口：切换进行中发出的 route set direct
    // 必须等切换完成后真实执行，应答与最终路由一致，不得假成功。
    let script = std::env::temp_dir().join(format!("mozart-slow-rvc-{}.sh", std::process::id()));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec '{}' --delay-ms 200 \"$@\"\n",
            example("rvc_stub").display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let daemon = DaemonProcess::start_with_config(
        "race",
        &example("pre_stub"),
        &script,
        "[daemon]\nrpc_timeout_ms = 2000\n\n[rvc]\nmodel_id = \"demo\"\n",
    );
    let conn_switch = daemon.connect();
    let conn_direct = daemon.connect();
    wait_running(&conn_switch);

    let switch_call = std::thread::spawn(move || {
        call_timeout(
            &conn_switch,
            ctl::DAEMON_ROUTE_SET,
            br#"{"route":"rvc"}"#,
            Duration::from_secs(8),
        )
    });
    std::thread::sleep(Duration::from_millis(300));
    let direct = call_timeout(
        &conn_direct,
        ctl::DAEMON_ROUTE_SET,
        br#"{"route":"direct"}"#,
        Duration::from_secs(8),
    );
    assert_eq!(direct.status, "ok");
    let switched = switch_call.join().unwrap();
    assert_eq!(switched.status, "ok");

    let final_status = status(&conn_direct);
    assert_eq!(
        final_status.route.active,
        Route::Direct,
        "应答 direct 后最终路由必须是 direct"
    );
    let _ = std::fs::remove_file(&script);
}
