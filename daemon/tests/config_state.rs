//! 配置写回后半段（SPEC-04）：运行时状态文件、config save、启动恢复、
//! 运行时键即时下发、写重启键后 status 与 config get 的待重启显示。

mod common;

use common::{call, example, status, submit_file, wait_ready, wait_until, DaemonProcess};
use mozart_core::ctl::schema::Response;
use mozart_core::ctl::{self, Seqpacket};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

const BASELINE: &str = "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\n\n\
     [bridge]\ntranslate_enabled = true\nspeech_enabled = true\n";

fn state_path(dir: &Path) -> std::path::PathBuf {
    dir.join("state").join("state.json")
}

fn wait_bridge_ready(conn: &Seqpacket) {
    assert!(
        wait_until(Duration::from_secs(5), || status(conn).bridge.is_some()),
        "bridge was not polled"
    );
}

/// 取一个真实存在的采集设备；无声卡主机返回 None（设备校验依赖 ALSA 枚举）。
fn capture_device(conn: &Seqpacket) -> Option<String> {
    let devices = call(conn, ctl::DAEMON_DEVICE_LIST, br#"{"all":true}"#);
    assert_eq!(devices.status, "ok");
    let found = devices.data["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["kind"] == "capture")
        .and_then(|d| d["id"].as_str())
        .map(str::to_string);
    if found.is_none() {
        eprintln!("跳过：本机无采集设备，待重启键用例未跑断言");
    }
    found
}

/// config get 应答：key 为 None 读整份基线。
fn config_get(conn: &Seqpacket, key: Option<&str>) -> Response {
    let body = json!({ "key": key }).to_string();
    let reply = call(conn, ctl::DAEMON_CONFIG_GET, body.as_bytes());
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    reply
}

#[test]
fn switch_writes_state_and_config_save_promotes_to_baseline() {
    let daemon = DaemonProcess::start_with_config(
        "cfg-save",
        &example("pre_stub"),
        &example("rvc_stub"),
        BASELINE,
    );
    let conn = daemon.connect();
    wait_bridge_ready(&conn);

    // 字幕开关转发成功后记入状态文件。
    let off = call(&conn, ctl::DAEMON_TRANSLATE_SET, br#"{"enabled":false}"#);
    assert_eq!(off.status, "ok", "reason: {}", off.reason);
    let speech = call(&conn, ctl::DAEMON_SPEECH_SET, br#"{"enabled":false}"#);
    assert_eq!(speech.status, "ok");
    let state = std::fs::read_to_string(state_path(&daemon.dir)).unwrap();
    assert!(state.contains("\"bridge.translate_enabled\": false"), "{state}");
    assert!(state.contains("\"bridge.speech_enabled\": false"), "{state}");
    // 运行时改动不自动写 TOML（模型甲）。
    let toml = std::fs::read_to_string(daemon.dir.join("config.toml")).unwrap();
    assert!(toml.contains("translate_enabled = true"), "{toml}");

    // config save：覆盖批量写回基线，随后清空状态文件。
    let saved = call(&conn, ctl::DAEMON_CONFIG_SAVE, b"{}");
    assert_eq!(saved.status, "ok", "reason: {}", saved.reason);
    assert_eq!(saved.data["saved"], 2);
    let toml = std::fs::read_to_string(daemon.dir.join("config.toml")).unwrap();
    assert!(toml.contains("translate_enabled = false"), "{toml}");
    assert!(toml.contains("speech_enabled = false"), "{toml}");
    assert!(!state_path(&daemon.dir).exists(), "config save 后状态文件应清空");

    // 空状态再 save：saved=0，幂等。
    let again = call(&conn, ctl::DAEMON_CONFIG_SAVE, b"{}");
    assert_eq!(again.status, "ok");
    assert_eq!(again.data["saved"], 0);
}

#[test]
fn restart_restores_switches_from_state_file() {
    let mut daemon = DaemonProcess::start_full_fresh(
        "cfg-restore",
        &example("pre_stub"),
        &example("rvc_stub"),
        &example("asr_stub"),
        &example("tts_stub"),
        &example("bridge_stub"),
        &example("llama_stub"),
        BASELINE,
        true,
    );
    let conn = daemon.connect();
    wait_bridge_ready(&conn);
    assert_eq!(call(&conn, ctl::DAEMON_TRANSLATE_SET, br#"{"enabled":false}"#).status, "ok");
    assert_eq!(call(&conn, ctl::DAEMON_SPEECH_SET, br#"{"enabled":false}"#).status, "ok");
    assert!(state_path(&daemon.dir).exists());

    // 优雅关停后保留目录与状态文件（forget 跳过 Drop 的删目录）。
    assert_eq!(call(&conn, ctl::DAEMON_SHUTDOWN, b"{}").status, "ok");
    drop(conn);
    common::wait_exit(&mut daemon.child, Duration::from_secs(5));
    std::mem::forget(daemon);

    let daemon = DaemonProcess::start_full_fresh(
        "cfg-restore",
        &example("pre_stub"),
        &example("rvc_stub"),
        &example("asr_stub"),
        &example("tts_stub"),
        &example("bridge_stub"),
        &example("llama_stub"),
        BASELINE,
        false,
    );
    let conn = daemon.connect();
    wait_bridge_ready(&conn);
    // 桥桩基线恒为 true；daemon 启动恢复把状态覆盖（false）下发到桥。
    assert!(
        wait_until(Duration::from_secs(5), || {
            let bridge = status(&conn).bridge;
            bridge.map(|b| !b.translate_enabled && !b.speech_enabled).unwrap_or(false)
        }),
        "switch overrides were not restored on startup"
    );
}

#[test]
fn bridge_restart_restores_switches_from_state_file() {
    let daemon = DaemonProcess::start_with_config(
        "cfg-bridge-restart",
        &example("pre_stub"),
        &example("rvc_stub"),
        BASELINE,
    );
    let conn = daemon.connect();
    wait_bridge_ready(&conn);
    assert_eq!(call(&conn, ctl::DAEMON_TRANSLATE_SET, br#"{"enabled":false}"#).status, "ok");
    assert_eq!(call(&conn, ctl::DAEMON_SPEECH_SET, br#"{"enabled":false}"#).status, "ok");
    let old_pid = status(&conn).processes.bridge.pid.unwrap();

    // 单服务重启桥进程：新桥进程无状态文件，daemon 检测到 pid 变化后重下发 state.json 覆盖。
    assert_eq!(
        call(&conn, ctl::DAEMON_RESTART, br#"{"service":"bridge"}"#).status,
        "ok"
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            let snapshot = status(&conn);
            let new_pid = snapshot.processes.bridge.pid;
            new_pid.is_some()
                && new_pid != Some(old_pid)
                && snapshot
                    .bridge
                    .map(|b| !b.translate_enabled && !b.speech_enabled)
                    .unwrap_or(false)
        }),
        "switch overrides were not restored after bridge restart"
    );
}

#[test]
fn switch_write_failure_records_degraded_and_keeps_in_memory_restore() {
    use std::os::unix::fs::PermissionsExt;

    struct RestorePermissions(std::path::PathBuf);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    // SAFETY: geteuid 无参数、线程安全。
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("跳过：root 下目录权限位不拦写，无法构造 state.json 写盘失败");
        return;
    }

    let daemon = DaemonProcess::start_with_config(
        "cfg-write-fail",
        &example("pre_stub"),
        &example("rvc_stub"),
        BASELINE,
    );
    let conn = daemon.connect();
    wait_bridge_ready(&conn);
    let old_pid = status(&conn).processes.bridge.pid.unwrap();

    let state_dir = daemon.dir.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let _guard = RestorePermissions(state_dir.clone());
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let reply = call(&conn, ctl::DAEMON_TRANSLATE_SET, br#"{"enabled":false}"#);
    assert_eq!(reply.status, "ok", "写盘失败不得阻断已生效的开关命令: {}", reply.reason);
    assert!(!state_path(&daemon.dir).exists());
    assert!(
        status(&conn)
            .degraded
            .iter()
            .any(|reason| reason.contains("state file write bridge.translate_enabled")),
        "写盘失败须进入 degraded: {:?}",
        status(&conn).degraded
    );

    // 只重启桥进程：daemon 内存 overrides 已更新，仍可把 false 推回新桥。
    assert_eq!(
        call(&conn, ctl::DAEMON_RESTART, br#"{"service":"bridge"}"#).status,
        "ok"
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            let snapshot = status(&conn);
            let new_pid = snapshot.processes.bridge.pid;
            new_pid.is_some()
                && new_pid != Some(old_pid)
                && snapshot
                    .bridge
                    .map(|b| !b.translate_enabled && b.speech_enabled)
                    .unwrap_or(false)
        }),
        "bridge restart should restore switch from in-memory overrides even when save failed"
    );
}

#[test]
fn runtime_key_applies_without_restart() {
    let storage = std::env::temp_dir().join(format!("mozart-cfg-runtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&storage);
    let daemon = DaemonProcess::start_file("cfg-runtime", &storage, "", true, &[]);
    let conn = daemon.connect();
    wait_ready(&conn);

    // 运行时键：写基线、回 restart_required=false、affected 空。
    let reply = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"file.max_upload_bytes","value":"1024"}"#,
    );
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(reply.data["restart_required"], false);
    assert_eq!(reply.data["affected"], json!([]));
    let toml = std::fs::read_to_string(daemon.dir.join("config.toml")).unwrap();
    assert!(toml.contains("max_upload_bytes = 1024"), "{toml}");

    // 即时生效：超过新上限的源文件被拒（无需重启 daemon）。
    let source = storage.join("big.wav");
    std::fs::write(&source, vec![0u8; 2048]).unwrap();
    let rejected = submit_file(&conn, &source, Some("demo"));
    assert_eq!(rejected.status, "error", "reason: {}", rejected.reason);
    assert_eq!(rejected.code, "InvalidArgument");
    assert!(rejected.reason.contains("upload limit"), "{}", rejected.reason);

    let _ = std::fs::remove_dir_all(&storage);
}

#[test]
fn restart_key_marks_pending_until_restart() {
    let daemon = DaemonProcess::start_with_config(
        "cfg-pending",
        &example("pre_stub"),
        &example("rvc_stub"),
        BASELINE,
    );
    let conn = daemon.connect();
    wait_ready(&conn);

    // 取一个真实存在的采集设备；无声卡主机跳过（设备校验依赖 ALSA 枚举）。
    let Some(capture) = capture_device(&conn) else {
        return;
    };

    let body = json!({"key": "pre.capture_device", "value": capture}).to_string();
    let reply = call(&conn, ctl::DAEMON_CONFIG_SET, body.as_bytes());
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(reply.data["restart_required"], true);
    assert!(
        wait_until(Duration::from_secs(2), || {
            status(&conn).pending_restart.iter().any(|s| s == "pre")
        }),
        "status should show pre pending restart"
    );

    // 重启 pre 后清除待重启标记。
    let restart = call(&conn, ctl::DAEMON_RESTART, br#"{"service":"pre"}"#);
    assert_eq!(restart.status, "ok", "reason: {}", restart.reason);
    assert!(
        wait_until(Duration::from_secs(2), || {
            !status(&conn).pending_restart.iter().any(|s| s == "pre")
        }),
        "pending restart should clear after restart"
    );
}

/// 键级待重启标记：config get 读到的是基线新值，应答须指出该键尚未生效；
/// restart 受影响服务后标记清空。
#[test]
fn config_get_marks_pending_key_until_restart() {
    let daemon = DaemonProcess::start_with_config(
        "cfg-pending-key",
        &example("pre_stub"),
        &example("rvc_stub"),
        BASELINE,
    );
    let conn = daemon.connect();
    wait_ready(&conn);

    let Some(capture) = capture_device(&conn) else {
        return;
    };
    let body = json!({"key": "pre.capture_device", "value": capture}).to_string();
    let reply = call(&conn, ctl::DAEMON_CONFIG_SET, body.as_bytes());
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);

    let got = config_get(&conn, Some("pre.capture_device"));
    assert_eq!(got.data["value"], json!(capture));
    assert_eq!(
        got.data["pending_restart_keys"],
        json!(["pre.capture_device"]),
        "基线新值尚未生效，应答须带该键"
    );
    // 整份基线带同一份列表；单键查询也回全量，过滤归 CLI。
    assert_eq!(
        config_get(&conn, None).data["pending_restart_keys"],
        json!(["pre.capture_device"])
    );
    assert_eq!(
        config_get(&conn, Some("rvc.model_id")).data["pending_restart_keys"],
        json!(["pre.capture_device"])
    );

    let restart = call(&conn, ctl::DAEMON_RESTART, br#"{"service":"pre"}"#);
    assert_eq!(restart.status, "ok", "reason: {}", restart.reason);
    assert_eq!(
        config_get(&conn, Some("pre.capture_device")).data["pending_restart_keys"],
        json!([]),
        "重启受理后键级标记应清空"
    );
    assert_eq!(config_get(&conn, None).data["pending_restart_keys"], json!([]));
}

/// 运行时键即时生效，不进待重启集合；无待重启键时应答仍带空数组（契约稳定）。
#[test]
fn runtime_key_not_marked_pending() {
    let daemon = DaemonProcess::start_with_config(
        "cfg-runtime-pending",
        &example("pre_stub"),
        &example("rvc_stub"),
        BASELINE,
    );
    let conn = daemon.connect();
    wait_ready(&conn);

    let reply = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"rvc.pitch_shift","value":"3"}"#,
    );
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(reply.data["restart_required"], false);

    let got = config_get(&conn, Some("rvc.pitch_shift"));
    assert_eq!(got.data["value"], 3);
    assert_eq!(got.data["pending_restart_keys"], json!([]));
    assert_eq!(config_get(&conn, None).data["pending_restart_keys"], json!([]));
    assert!(status(&conn).pending_restart_keys.is_empty());
}
