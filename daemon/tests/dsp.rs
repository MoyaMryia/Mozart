mod common;

use common::{call, connect, example, status, wait_until, DaemonProcess};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::time::Duration;

fn set(conn: &Seqpacket, key: &str, value: &str) -> mozart_core::ctl::schema::Response {
    call(
        conn,
        ctl::DAEMON_CONFIG_SET,
        &serde_json::to_vec(&json!({"key":key,"value":value})).unwrap(),
    )
}

fn arg(pid: u32, flag: &str) -> Value {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let args: Vec<_> = bytes.split(|byte| *byte == 0).collect();
    let index = args.iter().position(|arg| *arg == flag.as_bytes()).unwrap();
    serde_json::from_slice(args[index + 1]).unwrap()
}

fn dsp(daemon: &DaemonProcess) -> Value {
    serde_json::from_slice(&std::fs::read(daemon.dir.join("dsp-state.json")).unwrap()).unwrap()
}

fn crash_pre(daemon: &DaemonProcess, conn: &Seqpacket) -> u32 {
    let pid = status(conn).processes.pre.pid.unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    assert!(wait_until(Duration::from_secs(5), || {
        status(conn).processes.pre.pid.is_some_and(|new| new != pid)
    }));
    // Connecting also waits for the replacement stub to bind.
    let _ = connect(&daemon.dir.join("pre.sock"));
    status(conn).processes.pre.pid.unwrap()
}

#[test]
fn runtime_dsp_commit_failure_and_crash_recovery_preserve_effective_values() {
    let reject = std::env::temp_dir().join(format!("mozart-dsp-reject-{}", std::process::id()));
    let state_path = std::env::temp_dir().join(format!(
        "mozart-daemon-{}-dsp-runtime/dsp-state.json",
        std::process::id()
    ));
    let config = format!(
        "[daemon]\nrpc_timeout_ms = 300\n[process.env]\nMOZART_PRE_STUB_DSP_REJECT_FILE = {:?}\nMOZART_PRE_STUB_DSP_STATE_FILE = {:?}\n[enhance]\nmodel = 'test.onnx'\n",
        reject.to_str().unwrap(), state_path.to_str().unwrap()
    );
    let daemon = DaemonProcess::start_with_config(
        "dsp-runtime",
        &example("pre_stub"),
        &example("rvc_stub"),
        &config,
    );
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || status(&conn)
        .pre
        .is_some()));
    assert_eq!(
        set(&conn, "enhance.audio_socket", "pending-audio.sock").status,
        "ok"
    );
    let mut expected = json!({"denoise":true,"gate":false,"agc":false,"limiter":false});

    // Each command must send the complete four-boolean state.
    for (key, field, value) in [
        ("pre.dsp.denoise", "denoise", false),
        ("pre.dsp.gate", "gate", true),
        ("pre.dsp.agc", "agc", true),
        ("pre.dsp.limiter", "limiter", true),
    ] {
        let reply = set(&conn, key, if value { "true" } else { "false" });
        assert_eq!(reply.status, "ok", "{reply:?}");
        expected[field] = json!(value);
        assert_eq!(dsp(&daemon), expected);
    }
    // Failed new baselines must not replace effective values.
    std::fs::write(&reject, b"reject").unwrap();
    let failed = set(&conn, "pre.dsp.denoise", "true");
    assert_eq!(failed.status, "error");
    assert_eq!(failed.code, ctl::CODE_INVALID_ARGUMENT);
    assert!(failed.reason.contains("DSP rejected by stub"));
    assert!(failed.reason.contains("effective DSP unchanged"));
    assert!(failed.reason.contains("TOML baseline kept"));
    assert_eq!(dsp(&daemon), expected);
    assert!(
        mozart_core::config::Config::load_from(Some(&daemon.dir.join("config.toml")))
            .unwrap()
            .pre
            .dsp
            .denoise
    );
    let pid = crash_pre(&daemon, &conn);
    assert_eq!(arg(pid, "--effective-dsp"), expected);
    assert_eq!(dsp(&daemon), expected);
    assert_eq!(
        arg(pid, "--effective-enhance")["audio_socket"],
        "enhance-audio.sock"
    );

    std::fs::remove_file(&reject).unwrap();
    // A different successful key must not absorb the failed denoise baseline.
    assert_eq!(set(&conn, "pre.dsp.gate", "false").status, "ok");
    expected["gate"] = json!(false);
    assert_eq!(dsp(&daemon), expected);
    assert_eq!(set(&conn, "daemon.poll_interval_ms", "50").status, "ok");
    let pid = crash_pre(&daemon, &conn);
    assert_eq!(arg(pid, "--effective-dsp"), expected);
    assert_eq!(dsp(&daemon), expected);
    assert_eq!(
        arg(pid, "--effective-enhance")["audio_socket"],
        "enhance-audio.sock"
    );
    assert_eq!(status(&conn).pending_restart, ["enhance", "pre"]);
}

#[test]
fn preset_commit_does_not_apply_rejected_dsp_baseline_on_pre_recovery() {
    let temp = std::env::temp_dir();
    let root = temp.join(format!("mozart-dsp-preset-models-{}", std::process::id()));
    let model = root.join("model-a");
    std::fs::create_dir_all(&model).unwrap();
    mozart_core::preset::write_default(&model, &mozart_core::config::RvcSessionValues::default())
        .unwrap();
    let reject = temp.join(format!("mozart-dsp-preset-reject-{}", std::process::id()));
    let state_path = temp.join(format!(
        "mozart-daemon-{}-dsp-preset/dsp-state.json",
        std::process::id()
    ));
    let config = format!(
        "[daemon]\nrpc_timeout_ms=300\n[rvc]\nmodel_id='model-a'\nmodels_dir={:?}\n[process.env]\nMOZART_RVC_STUB_MODELS='model-a'\nMOZART_RVC_STUB_PRELOAD='model-a'\nMOZART_PRE_STUB_DSP_REJECT_FILE={:?}\nMOZART_PRE_STUB_DSP_STATE_FILE={:?}\n",
        root.to_str().unwrap(), reject.to_str().unwrap(), state_path.to_str().unwrap()
    );
    let daemon = DaemonProcess::start_with_config(
        "dsp-preset",
        &example("pre_stub"),
        &example("rvc_stub"),
        &config,
    );
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || {
        let s = status(&conn);
        s.pre.is_some() && s.rvc.and_then(|r| r.model_id).as_deref() == Some("model-a")
    }));
    assert_eq!(set(&conn, "pre.dsp.denoise", "false").status, "ok");
    std::fs::write(&reject, b"reject").unwrap();
    assert_eq!(set(&conn, "pre.dsp.denoise", "true").status, "error");
    let applied = call(&conn, ctl::DAEMON_PRESET_USE, b"{}");
    assert_eq!(applied.status, "ok", "{applied:?}");
    let pid = crash_pre(&daemon, &conn);
    assert_eq!(arg(pid, "--effective-dsp")["denoise"], false);
    assert_eq!(dsp(&daemon)["denoise"], false);
    assert!(
        mozart_core::config::Config::load_from(Some(&daemon.dir.join("config.toml")))
            .unwrap()
            .pre
            .dsp
            .denoise
    );
}
