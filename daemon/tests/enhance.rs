mod common;

use common::{call, example, status, wait_exit, wait_until, DaemonProcess};
use mozart_core::ctl::{self, schema::ProcessState};
use std::time::Duration;

fn child_arg(pid: u32, flag: &str) -> String {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let args: Vec<_> = bytes.split(|byte| *byte == 0).collect();
    let index = args
        .iter()
        .position(|arg| *arg == flag.as_bytes())
        .expect("child argument");
    String::from_utf8(args[index + 1].to_vec()).unwrap()
}

fn effective_arg(pid: u32) -> serde_json::Value {
    serde_json::from_str(&child_arg(pid, "--effective-enhance")).unwrap()
}

#[test]
fn enhancement_data_plane_degradation_recovers_and_ignores_disabled_history() {
    use mozart_core::ctl::schema::PreEnhanceStatus;
    let path = std::env::temp_dir().join(format!(
        "mozart-pre-enhancement-{}.json",
        std::process::id()
    ));
    let write = |enabled, connected, error: Option<&str>| {
        let value = PreEnhanceStatus {
            enabled,
            connected,
            dropped_frames: 42,
            last_error: error.map(str::to_string),
            ..Default::default()
        };
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::rename(temp, &path).unwrap();
    };
    write(true, false, Some("audio socket timeout"));
    let config = format!(
        "[process.env]\nMOZART_PRE_STUB_ENHANCEMENT_FILE = {:?}\n",
        path.to_str().unwrap()
    );
    let daemon = DaemonProcess::start_with_config(
        "enhance-data-plane",
        &example("pre_stub"),
        &example("rvc_stub"),
        &config,
    );
    let conn = daemon.connect();
    let wait = |enabled, connected, degraded: bool| {
        assert!(wait_until(Duration::from_secs(5), || {
            let snapshot = status(&conn);
            snapshot
                .pre
                .as_ref()
                .and_then(|pre| pre.enhancement.as_ref())
                .is_some_and(|e| e.enabled == enabled && e.connected == connected)
                && snapshot
                    .degraded
                    .iter()
                    .any(|reason| reason.starts_with("pre enhancement:"))
                    == degraded
        }));
    };
    wait(true, false, true);
    write(true, true, Some("output failed"));
    wait(true, true, true);
    write(false, false, Some("retained disabled error"));
    wait(false, false, false);
    write(true, false, None);
    wait(true, false, true);
    write(true, true, None);
    wait(true, true, false);
    assert_eq!(
        status(&conn)
            .pre
            .unwrap()
            .enhancement
            .unwrap()
            .dropped_frames,
        42
    );
    drop(daemon);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn absent_enhance_remains_stopped() {
    let daemon = DaemonProcess::start("enhance-absent", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || status(&conn)
        .pre
        .is_some()));
    let snapshot = status(&conn);
    assert_eq!(snapshot.processes.enhance.state, ProcessState::Stopped);
    assert!(snapshot.enhance.is_none());
    assert!(!snapshot
        .degraded
        .iter()
        .any(|reason| reason.starts_with("enhance")));
    // Old pre snapshots deserialize the new fields without requiring a new pre binary.
    let pre = snapshot.pre.unwrap();
    assert_eq!(pre.denoise_backend, "wola");
    assert!(pre.enhancement.is_none());
    assert_eq!(
        call(
            &conn,
            ctl::DAEMON_CONFIG_SET,
            br#"{"key":"enhance.model","value":"test.onnx"}"#
        )
        .status,
        "ok"
    );
    // An unrelated runtime write must not activate a restart-only backend.
    assert_eq!(
        call(
            &conn,
            ctl::DAEMON_CONFIG_SET,
            br#"{"key":"daemon.poll_interval_ms","value":"50"}"#
        )
        .status,
        "ok"
    );
    let pid = status(&conn).processes.pre.pid.unwrap();
    assert_eq!(effective_arg(pid), serde_json::Value::Null);
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    assert!(wait_until(Duration::from_secs(5), || status(&conn)
        .processes
        .pre
        .pid
        .is_some_and(|new| new != pid)));
    let after = status(&conn);
    assert_eq!(
        effective_arg(after.processes.pre.pid.unwrap()),
        serde_json::Value::Null
    );
    assert_eq!(after.processes.enhance.state, ProcessState::Stopped);
    assert!(after.enhance.is_none());
    let reply = call(&conn, ctl::DAEMON_RESTART, br#"{"service":"pre"}"#);
    assert_eq!(reply.status, "error");
    assert!(reply.reason.contains("restarting daemon"));
    assert_eq!(status(&conn).pending_restart, ["enhance", "pre"]);
    // live enhance 尚未生效前，禁止写 pre.dsp.denoise_mode=ulunas 并不得污染 TOML 基线。
    let rejected = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"pre.dsp.denoise_mode","value":"ulunas"}"#,
    );
    assert_eq!(rejected.status, "error");
    assert_eq!(rejected.code, ctl::CODE_INVALID_ARGUMENT);
    let baseline = mozart_core::config::Config::load_from(Some(&daemon.dir.join("config.toml"))).unwrap();
    assert!(baseline.pre.dsp.backend.is_none());
}

#[test]
fn enhance_stub_lifecycle_poll_restart_and_coupled_config() {
    let config = format!(
        "[daemon]\nrpc_timeout_ms = 300\n[process]\nenhance_bin = {:?}\n[enhance]\nmodel = 'test.onnx'\n",
        example("enhance_stub").to_str().unwrap()
    );
    let mut daemon = DaemonProcess::start_with_config(
        "enhance-enabled",
        &example("pre_stub"),
        &example("rvc_stub"),
        &config,
    );
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || status(&conn)
        .enhance
        .is_some()));
    let before = status(&conn);
    assert_eq!(before.enhance.unwrap().backend, "stub-only");
    assert!(before.arbiter.gpu.owner.is_none());
    let old_pid = before.processes.enhance.pid.unwrap();
    assert_eq!(
        child_arg(old_pid, "--audio-socket"),
        daemon.dir.join("enhance-audio.sock").to_str().unwrap()
    );
    unsafe {
        libc::kill(old_pid as i32, libc::SIGKILL);
    }
    assert!(wait_until(Duration::from_secs(5), || {
        let current = status(&conn);
        current.processes.enhance.restarts >= 1
            && current.enhance.is_some()
            && current
                .processes
                .enhance
                .pid
                .is_some_and(|pid| pid != old_pid)
    }));
    let reply = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"enhance.audio_socket","value":"next-enhance-audio.sock"}"#,
    );
    assert_eq!(reply.status, "ok", "{reply:?}");
    let pending = status(&conn);
    assert_eq!(pending.pending_restart, ["enhance", "pre"]);
    let previous = pending.processes.enhance.pid;
    let pre_pid = pending.processes.pre.pid.unwrap();
    let old_arg = effective_arg(pre_pid);
    assert_eq!(old_arg["audio_socket"], "enhance-audio.sock");
    unsafe {
        libc::kill(pre_pid as i32, libc::SIGKILL);
    }
    assert!(wait_until(Duration::from_secs(5), || status(&conn)
        .processes
        .pre
        .pid
        .is_some_and(|new| new != pre_pid)));
    let pre_pid = status(&conn).processes.pre.pid;
    assert_eq!(effective_arg(pre_pid.unwrap()), old_arg);
    assert_eq!(status(&conn).processes.enhance.pid, previous);
    assert_eq!(
        call(&conn, ctl::DAEMON_RESTART, br#"{"service":"pre"}"#).status,
        "ok"
    );
    assert!(wait_until(Duration::from_secs(5), || {
        let current = status(&conn);
        current.processes.enhance.pid.is_some()
            && current.processes.enhance.pid != previous
            && current.processes.pre.pid.is_some()
            && current.processes.pre.pid != pre_pid
            && current.pending_restart.is_empty()
            && current.enhance.is_some()
    }));
    assert_eq!(
        effective_arg(status(&conn).processes.pre.pid.unwrap())["audio_socket"],
        "next-enhance-audio.sock"
    );
    let pid = status(&conn).processes.enhance.pid.unwrap();
    assert_eq!(
        child_arg(pid, "--audio-socket"),
        daemon.dir.join("next-enhance-audio.sock").to_str().unwrap()
    );
    assert_eq!(
        call(&conn, ctl::DAEMON_RESTART, br#"{"service":"enhance"}"#).status,
        "ok"
    );
    assert!(wait_until(Duration::from_secs(5), || status(&conn)
        .processes
        .enhance
        .pid
        .is_some_and(|new| new != pid)));
    let pid = status(&conn).processes.enhance.pid.unwrap();
    assert_eq!(call(&conn, ctl::DAEMON_SHUTDOWN, b"").status, "ok");
    assert!(wait_exit(&mut daemon.child, Duration::from_secs(5)).success());
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
}

#[test]
fn runtime_denoise_mode_switches_between_off_wola_and_ulunas_without_restart() {
    let config = format!(
        "[daemon]\nrpc_timeout_ms = 300\n[process]\nenhance_bin = {:?}\n[enhance]\nmodel = 'test.onnx'\n",
        example("enhance_stub").to_str().unwrap()
    );
    let mut daemon = DaemonProcess::start_with_config(
        "denoise-mode-runtime",
        &example("pre_stub"),
        &example("rvc_stub"),
        &config,
    );
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || status(&conn)
        .pre
        .is_some()));

    let initial = status(&conn).pre.unwrap();
    assert_eq!(initial.denoise_backend, "ulunas");
    assert!(initial.dsp.unwrap().denoise);
    assert!(initial.denoise_backends.iter().any(|b| b.id == "wola" && b.available));
    assert!(initial.denoise_backends.iter().any(|b| b.id == "ulunas" && b.available));
    let initial_pid = status(&conn).processes.pre.pid.unwrap();
    assert_eq!(child_arg(initial_pid, "--effective-denoise-backend"), "ulunas");

    // 切到 wola：免重启立即生效且不重建 pre。
    let r = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"pre.dsp.denoise_mode","value":"wola"}"#,
    );
    assert_eq!(r.status, "ok", "{r:?}");
    let after_wola = status(&conn);
    assert_eq!(after_wola.processes.pre.pid.unwrap(), initial_pid);
    assert!(after_wola.pending_restart.is_empty());
    let pre = after_wola.pre.unwrap();
    assert_eq!(pre.denoise_backend, "wola");
    assert!(pre.dsp.unwrap().denoise);

    // 切到 off：保留最近模型并关闭 denoise。
    let r = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"pre.dsp.denoise_mode","value":"off"}"#,
    );
    assert_eq!(r.status, "ok", "{r:?}");
    let pre = status(&conn).pre.unwrap();
    assert!(!pre.dsp.unwrap().denoise);
    assert_eq!(pre.denoise_backend, "wola");

    // 从 off 原子一步切回 ulunas：同时开启 denoise 并切换后端。
    let r = call(
        &conn,
        ctl::DAEMON_CONFIG_SET,
        br#"{"key":"pre.dsp.denoise_mode","value":"ulunas"}"#,
    );
    assert_eq!(r.status, "ok", "{r:?}");
    let pre = status(&conn).pre.unwrap();
    assert!(pre.dsp.unwrap().denoise);
    assert_eq!(pre.denoise_backend, "ulunas");

    assert_eq!(call(&conn, ctl::DAEMON_SHUTDOWN, b"").status, "ok");
    assert!(wait_exit(&mut daemon.child, Duration::from_secs(5)).success());
}
