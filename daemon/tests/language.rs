mod common;

use common::{call, example, status, wait_until, DaemonProcess};
use mozart_core::ctl::schema::ProcessState;
use mozart_core::ctl::{self, Seqpacket};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

/// 等四个语言链进程都 Running，且 asr/tts/bridge 的业务快照已汇总进 status。
fn wait_language_ready(conn: &Seqpacket) {
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(conn);
            status.processes.asr.state == ProcessState::Running
                && status.processes.tts.state == ProcessState::Running
                && status.processes.bridge.state == ProcessState::Running
                && status.processes.llama.state == ProcessState::Running
                && status.asr.is_some()
                && status.tts.is_some()
                && status.bridge.is_some()
        }),
        "language children did not become ready"
    );
}

#[test]
fn supervises_and_aggregates_language_children() {
    let daemon = DaemonProcess::start("lang-supervise", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    wait_language_ready(&conn);

    let snapshot = status(&conn);
    assert_eq!(snapshot.asr.unwrap().startup_id, "stub");
    assert_eq!(snapshot.tts.unwrap().state, "idle");
    assert!(snapshot.bridge.unwrap().translate_enabled);
    assert!(snapshot.processes.llama.pid.is_some());

    // 崩溃重启：与 pre/rvc 同一退避监管逻辑。
    let asr_pid = status(&conn).processes.asr.pid.unwrap();
    // SAFETY: 只向测试内拉起的子进程发送信号。
    unsafe {
        libc::kill(asr_pid as i32, libc::SIGKILL);
    }
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.processes.asr.restarts >= 1
                && status.processes.asr.pid.is_some()
                && status.processes.asr.pid != Some(asr_pid)
                && status.asr.is_some()
        }),
        "asr was not restarted and re-polled"
    );
}

#[test]
fn forwards_translate_and_speech_to_bridge() {
    let daemon = DaemonProcess::start("lang-forward", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    wait_language_ready(&conn);

    let off = call(&conn, ctl::DAEMON_TRANSLATE_SET, br#"{"enabled":false}"#);
    assert_eq!(off.status, "ok");
    assert_eq!(off.data["translate_enabled"], false);
    assert!(
        wait_until(Duration::from_secs(5), || {
            status(&conn).bridge.map(|b| !b.translate_enabled).unwrap_or(false)
        }),
        "translate off was not aggregated into status"
    );

    let speech_off = call(&conn, ctl::DAEMON_SPEECH_SET, br#"{"enabled":false}"#);
    assert_eq!(speech_off.status, "ok");
    assert_eq!(speech_off.data["speech_enabled"], false);

    let stop = call(&conn, ctl::DAEMON_SPEECH_STOP, b"{}");
    assert_eq!(stop.status, "ok");
    assert_eq!(stop.data["speech_enabled"], false);
    assert!(
        wait_until(Duration::from_secs(5), || {
            status(&conn).bridge.map(|b| !b.speech_enabled).unwrap_or(false)
        }),
        "speech stop was not aggregated into status"
    );

    let on = call(&conn, ctl::DAEMON_TRANSLATE_SET, br#"{"enabled":true}"#);
    assert_eq!(on.status, "ok");
    assert_eq!(on.data["translate_enabled"], true);

    // 入口校验：坏体在 daemon 侧拒绝，不下发到桥。
    let bad_json = call(&conn, ctl::DAEMON_TRANSLATE_SET, b"not json");
    assert_eq!(bad_json.code, "InvalidArgument");
    let missing = call(&conn, ctl::DAEMON_SPEECH_SET, b"{}");
    assert_eq!(missing.code, "InvalidArgument");
    let extra = call(&conn, ctl::DAEMON_SPEECH_SET, br#"{"enabled":true,"x":1}"#);
    assert_eq!(extra.code, "InvalidArgument");
}

#[test]
fn forwarding_rejected_when_bridge_down() {
    let missing = std::env::temp_dir().join(format!("mozart-missing-bridge-{}", std::process::id()));
    let daemon = DaemonProcess::start_full(
        "lang-nobridge",
        &example("pre_stub"),
        &example("rvc_stub"),
        &example("asr_stub"),
        &example("tts_stub"),
        &missing,
        &example("llama_stub"),
        "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\n",
    );
    let conn = daemon.connect();
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.processes.bridge.restarts >= 1 && status.processes.bridge.last_error.is_some()
        }),
        "bridge spawn failure was not reported"
    );

    let reply = call(&conn, ctl::DAEMON_TRANSLATE_SET, br#"{"enabled":false}"#);
    assert_eq!(reply.status, "error");
    assert_eq!(reply.code, "NotReady");
}

#[test]
fn llama_startup_args_use_host_socket() {
    // llama-server 不说控制协议：验证 daemon 用 -m/--host/--jinja 启动，
    // 不传 --socket/--config（SPEC-05）。wrapper 记录 argv 后 exec 监管桩。
    let tag = std::process::id();
    let record = std::env::temp_dir().join(format!("mozart-llama-argv-{tag}.txt"));
    let script = std::env::temp_dir().join(format!("mozart-llama-argv-{tag}.sh"));
    let _ = std::fs::remove_file(&record);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexec '{}' \"$@\"\n",
            record.display(),
            example("llama_stub").display(),
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let daemon = DaemonProcess::start_full(
        "llama-argv",
        &example("pre_stub"),
        &example("rvc_stub"),
        &example("asr_stub"),
        &example("tts_stub"),
        &example("bridge_stub"),
        &script,
        "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\n\n\
         [bridge]\nllama_model = \"/opt/mozart/translate.gguf\"\n",
    );
    let conn = daemon.connect();
    assert!(
        wait_until(Duration::from_secs(5), || {
            status(&conn).processes.llama.state == ProcessState::Running
        }),
        "llama did not start"
    );
    assert!(
        wait_until(Duration::from_secs(5), || record.exists()),
        "llama argv record was not written"
    );
    let argv: Vec<String> = std::fs::read_to_string(&record)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let value_after = |flag: &str| {
        argv.iter()
            .position(|arg| arg == flag)
            .and_then(|index| argv.get(index + 1))
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(value_after("-m"), "/opt/mozart/translate.gguf");
    assert_eq!(value_after("--host"), daemon.dir.join("llama.sock").to_str().unwrap());
    assert_eq!(value_after("-c"), "2048");
    assert_eq!(value_after("-ngl"), "99");
    assert!(argv.iter().any(|arg| arg == "--jinja"));
    assert!(!argv.iter().any(|arg| arg == "--socket"), "llama 不走 --socket 契约");
    assert!(!argv.iter().any(|arg| arg == "--config"), "llama 不走 --config 契约");

    drop(conn);
    let _ = std::fs::remove_file(&record);
    let _ = std::fs::remove_file(&script);
}