//! 配置写回后半段（SPEC-04）：运行时状态文件、config save、启动恢复、
//! 运行时键即时下发、写重启键后 status 待重启显示。

mod common;

use common::{call, example, status, submit_file, wait_ready, wait_until, DaemonProcess};
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
    let devices = call(&conn, ctl::DAEMON_DEVICE_LIST, br#"{"all":true}"#);
    assert_eq!(devices.status, "ok");
    let capture = devices.data["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["kind"] == "capture")
        .and_then(|d| d["id"].as_str())
        .map(str::to_string);
    let Some(capture) = capture else {
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