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
fn output_modes_switch_speech_and_route_without_restarting_asr() {
    use mozart_core::ctl::schema::{OutputMode, OutputPhase, PlayoutPolicy, Route};
    let daemon = DaemonProcess::start("lang-output-mode", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    wait_language_ready(&conn);
    let startup = status(&conn).asr.unwrap().startup_id;
    let pid = status(&conn).processes.asr.pid;
    for (name, requested, route, speaking) in [
        ("broadcast", OutputMode::Broadcast, Route::Direct, true),
        ("rvc", OutputMode::Rvc, Route::Rvc, false),
        ("direct", OutputMode::Direct, Route::Direct, false),
        ("broadcast", OutputMode::Broadcast, Route::Direct, true),
    ] {
        let body = serde_json::json!({"mode": name}).to_string();
        let accepted = call(&conn, ctl::DAEMON_OUTPUT_MODE_SET, body.as_bytes());
        assert_eq!(accepted.status, "ok", "{}", accepted.reason);
        assert_eq!(accepted.data["requested"], name);
        assert_eq!(accepted.data["phase"], "closing_speech");
        assert!(accepted.data["applied"].is_null());
        assert!(wait_until(Duration::from_secs(5), || {
            let view = status(&conn);
            view.output_mode.is_some_and(|m| m.phase == OutputPhase::Applied && m.applied == Some(requested))
                && view.route.active == route
                && view.bridge.is_some_and(|b| b.speech_enabled == speaking && b.translate_enabled)
                && view.pre.is_some_and(|p| p.route == route && p.playout_policy == if speaking {
                    PlayoutPolicy::BroadcastOnly
                } else {
                    PlayoutPolicy::RealtimeOnly
                })
        }), "mode did not apply: {:?}", status(&conn).output_mode);
        let current = status(&conn);
        assert_eq!(current.processes.asr.pid, pid);
        assert_eq!(current.asr.unwrap().startup_id, startup);
    }
    let invalid = call(&conn, ctl::DAEMON_OUTPUT_MODE_SET, br#"{"mode":"unknown"}"#);
    assert_eq!(invalid.code, ctl::CODE_INVALID_ARGUMENT);
    assert_eq!(status(&conn).output_mode.unwrap().applied, Some(OutputMode::Broadcast));
    let bridge_pid = status(&conn).processes.bridge.pid.unwrap();
    // SAFETY: this PID belongs to the supervised child started by this test.
    assert_eq!(unsafe { libc::kill(bridge_pid as i32, libc::SIGKILL) }, 0);
    assert!(wait_until(Duration::from_secs(5), || {
        let view = status(&conn);
        view.processes.bridge.pid.is_some_and(|current| current != bridge_pid)
            && view.output_mode.is_some_and(|m| m.applied == Some(OutputMode::Broadcast))
            && view.bridge.is_some_and(|b| b.speech_enabled)
    }));
    let pre_pid = status(&conn).processes.pre.pid.unwrap();
    // SAFETY: this PID is the supervised pre child of this fixture.
    assert_eq!(unsafe { libc::kill(pre_pid as i32, libc::SIGKILL) }, 0);
    assert!(wait_until(Duration::from_secs(5), || {
        let view = status(&conn);
        view.processes.pre.pid.is_some_and(|current| current != pre_pid
            && view.pre.as_ref().is_some_and(|pre| pre.startup_id == format!("stub-{current}")
                && pre.playout_policy == PlayoutPolicy::BroadcastOnly))
            && view.output_mode.is_some_and(|mode| mode.applied == Some(OutputMode::Broadcast))
    }));
    assert_eq!(status(&conn).asr.unwrap().startup_id, startup);
    let reply = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"direct"}"#);
    assert_eq!(reply.status, "ok", "{}", reply.reason);
    assert!(wait_until(Duration::from_secs(5), || {
        let view = status(&conn);
        view.output_mode.is_none() && view.pre.is_some_and(|pre| pre.playout_policy == PlayoutPolicy::Legacy)
    }));
}

#[test]
fn startup_enters_realtime_mode_without_a_user_command() {
    use mozart_core::ctl::schema::{OutputMode, PlayoutPolicy, Route};
    let storage = std::env::temp_dir().join(format!("mozart-startup-file-{}", std::process::id()));
    let daemon = DaemonProcess::start_file(
        "startup-output-mode", &storage, "", true, &[("MOZART_STARTUP_MODE", "rvc".to_string())],
    );
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || {
        let view = status(&conn);
        view.route.active == Route::Rvc
            && view.output_mode.is_some_and(|mode| mode.applied == Some(OutputMode::Rvc))
            && view.pre.is_some_and(|pre| pre.playout_policy == PlayoutPolicy::RealtimeOnly)
            && view.bridge.is_some_and(|bridge| !bridge.speech_enabled && bridge.translate_enabled)
    }));
    drop(daemon);
    std::fs::remove_dir_all(storage).unwrap();
}

fn output_mode(conn: &Seqpacket, name: &str) {
    let body = serde_json::json!({"mode": name}).to_string();
    let response = call(conn, ctl::DAEMON_OUTPUT_MODE_SET, body.as_bytes());
    assert_eq!(response.status, "ok", "{}", response.reason);
    assert_eq!(response.data["requested"], name);
}

#[test]
fn known_drained_realtime_modes_switch_without_tts_but_broadcast_waits() {
    use mozart_core::ctl::schema::{OutputMode, OutputPhase};
    let storage = std::env::temp_dir().join(format!("mozart-no-tts-file-{}", std::process::id()));
    let daemon = DaemonProcess::start_file("drained-no-tts", &storage, "", true, &[
        ("MOZART_STARTUP_MODE", "direct".into()),
        ("MOZART_TTS_BIN", "/bin/false".into()),
    ]);
    let conn = daemon.connect();
    let applied = |mode| status(&conn).output_mode.is_some_and(|m| m.applied == Some(mode));
    let window = Duration::from_secs(5);
    assert!(wait_until(window, || applied(OutputMode::Direct) && status(&conn).asr.is_some()));
    let startup = status(&conn).asr.unwrap().startup_id;
    output_mode(&conn, "rvc");
    assert!(wait_until(window, || applied(OutputMode::Rvc)));
    output_mode(&conn, "direct");
    assert!(wait_until(window, || applied(OutputMode::Direct)));
    output_mode(&conn, "broadcast");
    assert!(wait_until(window, || status(&conn).output_mode.is_some_and(|m|
        m.phase == OutputPhase::ClosingSpeech && m.applied.is_none()
            && m.last_error.is_some_and(|error| error.contains("speech stop fence: tts"))
    )));
    assert!(status(&conn).bridge.is_some_and(|bridge| !bridge.speech_enabled && bridge.translate_enabled));
    output_mode(&conn, "rvc");
    assert!(wait_until(window, || applied(OutputMode::Rvc)), "unused broadcast fence must be withdrawable");
    assert!(!status(&conn).degraded.iter().any(|why| why.starts_with("speech stop fence:")));
    assert_eq!(status(&conn).asr.unwrap().startup_id, startup);
    drop(daemon);
    std::fs::remove_dir_all(storage).unwrap();
}

#[test]
fn newer_mode_cancels_waiting_realtime_restart_during_unknown_stop() {
    use mozart_core::ctl::schema::{Activity, OutputMode, Route};
    let root = std::env::temp_dir().join(format!("mozart-supersede-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let fault = root.join("stop-fault");
    let log = root.join("stub.log");
    let daemon = DaemonProcess::start_file("supersede-realtime", &root.join("files"), "", true, &[
        ("MOZART_STARTUP_MODE", "direct".into()),
        ("MOZART_RVC_STUB_STOP_FAULT_FILE", fault.display().to_string()),
        ("MOZART_STUB_LOG", log.display().to_string()),
    ]);
    let conn = daemon.connect();
    let window = Duration::from_secs(5);
    let applied = |mode| status(&conn).output_mode.is_some_and(|m| m.applied == Some(mode));
    // Isolate stop-fault counts from cold-start retries while pre is still starting.
    assert!(wait_until(window, || {
        let view = status(&conn);
        view.rvc.is_some() && view.output_mode.is_some_and(|m| m.applied == Some(OutputMode::Direct))
    }));
    output_mode(&conn, "rvc");
    assert!(wait_until(window, || applied(OutputMode::Rvc)));
    let start_prefix = format!("mozart-rvc-stub {} ", ctl::RVC_START_STREAM);
    let starts = || std::fs::read_to_string(&log).unwrap().lines().filter(|line| line.starts_with(&start_prefix)).count();
    assert_eq!(starts(), 1);
    std::fs::write(&fault, "after").unwrap();

    output_mode(&conn, "direct");
    assert!(wait_until(window, || {
        let view = status(&conn);
        view.route.active == Route::Direct && view.arbiter.gpu.owner == Some(Activity::Realtime)
            && view.degraded.iter().any(|why| why.starts_with("rvc realtime return:"))
    }));
    output_mode(&conn, "rvc");
    assert!(wait_until(window, || status(&conn).route.desired == Route::Rvc));
    assert_eq!(starts(), 1, "timeout must not release the old lease");
    output_mode(&conn, "rvc");
    assert_eq!(status(&conn).route.desired, Route::Rvc, "duplicate intent must not clear its wait slot");
    output_mode(&conn, "direct");
    assert_eq!(status(&conn).route.desired, Route::Direct);
    std::fs::remove_file(fault).unwrap();
    assert!(wait_until(window, || applied(OutputMode::Direct) && status(&conn).arbiter.gpu.owner.is_none()));
    assert_eq!(starts(), 1, "old waiting intent must not open another stream");
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn lost_broadcast_open_reply_still_requires_a_real_stop_fence() {
    use mozart_core::ctl::schema::{OutputMode, OutputPhase};
    let root = std::env::temp_dir().join(format!("mozart-lost-open-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let open_fault = root.join("lost-open");
    let stop_fault = root.join("stop-fault");
    let daemon = DaemonProcess::start_file("mode-lost-open", &root.join("files"), "", true, &[
        ("MOZART_STARTUP_MODE", "direct".into()),
        ("MOZART_BRIDGE_STUB_LOST_OPEN_REPLY", open_fault.display().to_string()),
        ("MOZART_TTS_STUB_STOP_FAULT", stop_fault.display().to_string()),
    ]);
    let conn = daemon.connect();
    let window = Duration::from_secs(5);
    let applied = |mode| status(&conn).output_mode.is_some_and(|m| m.applied == Some(mode));
    assert!(wait_until(window, || applied(OutputMode::Direct)));
    std::fs::write(&open_fault, "").unwrap();
    output_mode(&conn, "broadcast");
    assert!(wait_until(window, || {
        let view = status(&conn);
        view.bridge.is_some_and(|b| b.speech_enabled)
            && view.output_mode.is_some_and(|m| m.phase == OutputPhase::OpeningSpeech
                && m.applied.is_none() && m.last_error.is_some())
    }), "bridge opened but daemon did not get the ACK");

    std::fs::write(&stop_fault, "").unwrap();
    output_mode(&conn, "direct");
    assert!(wait_until(window, || {
        let view = status(&conn);
        view.bridge.is_some_and(|b| !b.speech_enabled)
            && view.output_mode.is_some_and(|m| m.phase == OutputPhase::ClosingSpeech
                && m.applied.is_none() && m.last_error.is_some_and(|err| err.contains("stop fault")))
    }), "unknown admission must be fenced before the replacement mode applies");
    std::fs::remove_file(stop_fault).unwrap();
    assert!(wait_until(window, || applied(OutputMode::Direct)));
    assert!(!status(&conn).bridge.unwrap().speech_enabled);
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn restarted_pre_keeps_the_gate_closed_until_realtime_is_ready() {
    use mozart_core::ctl::schema::{OutputMode, PlayoutPolicy, Route};
    let root = std::env::temp_dir().join(format!("mozart-pre-gate-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let fault = root.join("rvc-status-fault");
    let daemon = DaemonProcess::start_file("pre-gate-restart", &root.join("files"), "", true, &[
        ("MOZART_STARTUP_MODE", "rvc".into()),
        ("MOZART_RVC_STUB_STATUS_FAIL_FILE", fault.display().to_string()),
    ]);
    let conn = daemon.connect();
    let window = Duration::from_secs(5);
    assert!(wait_until(window, || status(&conn).output_mode.is_some_and(|m| m.applied == Some(OutputMode::Rvc))));
    assert_eq!(call(&conn, ctl::DAEMON_MUTE_SET, br#"{"mute":true}"#).status, "ok");
    std::fs::write(&fault, "").unwrap();
    assert!(wait_until(window, || status(&conn).rvc.is_none()));
    let before = status(&conn).processes.pre.pid.unwrap();
    // SAFETY: the PID is the supervised fixture child.
    assert_eq!(unsafe { libc::kill(before as i32, libc::SIGKILL) }, 0);
    assert!(wait_until(window, || {
        let view = status(&conn);
        view.processes.pre.pid.is_some_and(|pid| pid != before
            && view.pre.is_some_and(|pre| pre.startup_id == format!("stub-{pid}")
                && pre.playout_policy == PlayoutPolicy::Silent))
            && view.output_mode.is_some_and(|mode| mode.applied.is_none()
                && mode.phase == mozart_core::ctl::schema::OutputPhase::ReattachingPlayout)
    }), "pre must not open its startup direct route before RVC is ready");
    std::fs::remove_file(fault).unwrap();
    assert!(wait_until(window, || {
        let view = status(&conn);
        view.output_mode.is_some_and(|mode| mode.applied == Some(OutputMode::Rvc))
            && view.pre.is_some_and(|pre| pre.route == Route::Rvc && pre.mute
                && pre.playout_policy == PlayoutPolicy::RealtimeOnly)
    }));
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn missing_tts_fence_is_not_success_and_blocks_reopening() {
    let daemon = DaemonProcess::start_full(
        "lang-fence-without-tts", &example("pre_stub"), &example("rvc_stub"),
        &example("asr_stub"), std::path::Path::new("/bin/false"),
        &example("bridge_stub"), &example("llama_stub"),
        "[daemon]\nrpc_timeout_ms = 300\n[rvc]\nmodel_id = \"demo\"\n[bridge]\nspeech_enabled = true\n",
    );
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || {
        let view = status(&conn);
        view.pre.is_some() && view.bridge.as_ref().is_some_and(|b| b.speech_enabled) && view.asr.is_some()
    }));
    let asr_before = status(&conn).processes.asr.pid;
    let stop = call(&conn, ctl::DAEMON_SPEECH_STOP, b"{}");
    assert_eq!(stop.status, "error");
    assert!(stop.reason.contains("speech stop fence: tts"), "{}", stop.reason);
    let reopen = call(&conn, ctl::DAEMON_SPEECH_SET, br#"{"enabled":true}"#);
    assert_eq!(reopen.status, "error");
    assert!(wait_until(Duration::from_secs(5), || {
        let view = status(&conn);
        view.bridge.is_some_and(|b| !b.speech_enabled && b.translate_enabled)
            && view.degraded.iter().any(|reason| reason.starts_with("speech stop fence: tts"))
    }));
    assert_eq!(status(&conn).processes.asr.pid, asr_before);
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
    assert!(stop.data["cutoff_ns"].as_u64().is_some());
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

#[test]
fn llama_startup_args_use_selected_bridge_model_config() {
    let tag = std::process::id();
    let record = std::env::temp_dir().join(format!("mozart-llama-model-argv-{tag}.txt"));
    let script = std::env::temp_dir().join(format!("mozart-llama-model-argv-{tag}.sh"));
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
        "llama-model-argv",
        &example("pre_stub"),
        &example("rvc_stub"),
        &example("asr_stub"),
        &example("tts_stub"),
        &example("bridge_stub"),
        &script,
        "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\n\n\
         [bridge]\nllama_model_id = \"hymt\"\n\n\
         [[bridge.models]]\nid = \"hymt\"\npath = \"/opt/mozart/hymt.gguf\"\n\
         prompt_profile = \"hy_mt\"\nctx_size = 1024\ncache_ram_mb = 64\n\
         batch_size = 256\nubatch_size = 128\ngpu_layers = 99\nthreads = 3\n",
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
    assert_eq!(value_after("-m"), "/opt/mozart/hymt.gguf");
    assert_eq!(value_after("-c"), "1024");
    assert_eq!(value_after("-b"), "256");
    assert_eq!(value_after("-ub"), "128");
    assert_eq!(value_after("--cache-ram"), "64");
    assert_eq!(value_after("-ngl"), "99");
    assert_eq!(value_after("-t"), "3");
    assert_eq!(value_after("-tb"), "3");
    assert_eq!(value_after("--host"), daemon.dir.join("llama.sock").to_str().unwrap());
    assert!(argv.iter().any(|arg| arg == "--jinja"));

    drop(conn);
    let _ = std::fs::remove_file(&record);
    let _ = std::fs::remove_file(&script);
}

#[test]
fn default_startup_enters_direct_mode_with_speech_disabled_and_translate_enabled() {
    use mozart_core::ctl::schema::{OutputMode, OutputPhase, PlayoutPolicy, Route};
    use std::process::{Command, Stdio};

    let dir = std::env::temp_dir().join(format!("mozart-daemon-{}-default-startup", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(&config, "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\n").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_mozart-daemon"))
        .arg("--config")
        .arg(&config)
        .env_remove("MOZART_STARTUP_MODE")
        .env("MOZART_RUN_DIR", &dir)
        .env("MOZART_STATE_DIR", dir.join("state"))
        .env("MOZART_PRE_BIN", example("pre_stub"))
        .env("MOZART_RVC_BIN", example("rvc_stub"))
        .env("MOZART_ASR_BIN", example("asr_stub"))
        .env("MOZART_TTS_BIN", example("tts_stub"))
        .env("MOZART_BRIDGE_BIN", example("bridge_stub"))
        .env("MOZART_LLAMA_BIN", example("llama_stub"))
        .env("MOZART_POLL_INTERVAL_MS", "100")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let daemon = DaemonProcess {
        child,
        dir: dir.clone(),
        socket: dir.join("daemon.sock"),
    };
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || {
        let view = status(&conn);
        view.output_mode.is_some_and(|mode| mode.phase == OutputPhase::Applied && mode.applied == Some(OutputMode::Direct))
            && view.route.active == Route::Direct
            && view.pre.is_some_and(|pre| pre.playout_policy == PlayoutPolicy::RealtimeOnly)
            && view.bridge.is_some_and(|bridge| !bridge.speech_enabled && bridge.translate_enabled)
    }));
}

#[test]
fn speech_clear_keeps_broadcast_mode_and_speech_enabled() {
    use mozart_core::ctl::schema::{OutputMode, OutputPhase, PlayoutPolicy};

    let daemon = DaemonProcess::start("lang-speech-clear", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();
    wait_language_ready(&conn);

    output_mode(&conn, "broadcast");
    assert!(wait_until(Duration::from_secs(5), || {
        let view = status(&conn);
        view.output_mode.is_some_and(|mode| mode.phase == OutputPhase::Applied && mode.applied == Some(OutputMode::Broadcast))
            && view.bridge.is_some_and(|bridge| bridge.speech_enabled && bridge.translate_enabled)
            && view.pre.is_some_and(|pre| pre.playout_policy == PlayoutPolicy::BroadcastOnly)
    }));

    let bad = call(&conn, ctl::DAEMON_SPEECH_CLEAR, br#"{"extra":1}"#);
    assert_eq!(bad.code, ctl::CODE_INVALID_ARGUMENT);

    let clear = call(&conn, ctl::DAEMON_SPEECH_CLEAR, b"{}");
    assert_eq!(clear.status, "ok", "{}", clear.reason);
    assert_eq!(clear.data["speech_enabled"], true);
    assert!(clear.data["cutoff_ns"].as_u64().unwrap() > 0);

    let view = status(&conn);
    assert!(view.output_mode.is_some_and(|mode| mode.phase == OutputPhase::Applied && mode.applied == Some(OutputMode::Broadcast)));
    assert!(view.bridge.is_some_and(|bridge| bridge.speech_enabled && bridge.translate_enabled));
    assert!(view.pre.is_some_and(|pre| pre.playout_policy == PlayoutPolicy::BroadcastOnly));
    assert!(view.degraded.is_empty(), "{:?}", view.degraded);
}

#[test]
fn speech_clear_fence_reconciles_when_tts_recovers() {
    use mozart_core::ctl::schema::{OutputMode, OutputPhase};

    let root = std::env::temp_dir().join(format!("mozart-speech-clear-fault-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let stop_fault = root.join("stop-fault");
    let daemon = DaemonProcess::start_file(
        "speech-clear-fault",
        &root.join("files"),
        "",
        true,
        &[
            ("MOZART_STARTUP_MODE", "broadcast".into()),
            ("MOZART_TTS_STUB_STOP_FAULT", stop_fault.display().to_string()),
        ],
    );
    let conn = daemon.connect();
    let window = Duration::from_secs(5);
    assert!(wait_until(window, || {
        let view = status(&conn);
        view.output_mode.is_some_and(|mode| mode.phase == OutputPhase::Applied && mode.applied == Some(OutputMode::Broadcast))
            && view.bridge.is_some_and(|bridge| bridge.speech_enabled)
    }));

    std::fs::write(&stop_fault, "").unwrap();
    let reply = call(&conn, ctl::DAEMON_SPEECH_CLEAR, b"{}");
    assert_eq!(reply.status, "error");
    let view = status(&conn);
    assert!(
        view.degraded.iter().any(|entry| entry.starts_with("speech stop fence: tts:")),
        "{:?}",
        view.degraded
    );
    assert!(view.output_mode.is_some_and(|mode| mode.phase == OutputPhase::Applied && mode.applied == Some(OutputMode::Broadcast)));
    assert!(view.bridge.is_some_and(|bridge| bridge.speech_enabled));

    std::fs::remove_file(&stop_fault).unwrap();
    assert!(wait_until(window, || status(&conn).degraded.is_empty()));
    let recovered = status(&conn);
    assert!(recovered.output_mode.is_some_and(|mode| mode.phase == OutputPhase::Applied && mode.applied == Some(OutputMode::Broadcast)));
    assert!(recovered.bridge.is_some_and(|bridge| bridge.speech_enabled));

    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}