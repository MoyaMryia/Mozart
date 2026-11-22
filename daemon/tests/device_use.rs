mod common;

use common::{call, connect, example, status, wait_until, DaemonProcess};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::json;
use std::time::Duration;

fn list_device(conn: &Seqpacket, kind: &str) -> Option<String> {
    let reply = call(conn, ctl::DAEMON_DEVICE_LIST, br#"{"all":false}"#);
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    reply.data["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["kind"].as_str() == Some(kind))
        .and_then(|device| device["id"].as_str())
        .map(str::to_string)
}

fn device_use(
    conn: &Seqpacket,
    kind: &str,
    device: &str,
) -> mozart_core::ctl::schema::Response {
    let body = json!({"kind": kind, "device": device}).to_string();
    call(conn, ctl::DAEMON_DEVICE_USE, body.as_bytes())
}

fn arg_str(pid: u32, flag: &str) -> String {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let args: Vec<_> = bytes.split(|byte| *byte == 0).collect();
    let index = args.iter().position(|arg| *arg == flag.as_bytes()).unwrap();
    String::from_utf8(args[index + 1].to_vec()).unwrap()
}

fn crash_pre(daemon: &DaemonProcess, conn: &Seqpacket) -> u32 {
    let pid = status(conn).processes.pre.pid.unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    assert!(wait_until(Duration::from_secs(5), || {
        status(conn).processes.pre.pid.is_some_and(|new| new != pid)
    }));
    let _ = connect(&daemon.dir.join("pre.sock"));
    status(conn).processes.pre.pid.unwrap()
}

#[test]
fn device_use_switches_temporarily_survives_crash_and_resets_on_restart() {
    let temp = std::env::temp_dir();
    let reject = temp.join(format!("mozart-device-reject-{}", std::process::id()));
    let _ = std::fs::remove_file(&reject);
    let models_dir = temp.join(format!("mozart-device-models-{}", std::process::id()));
    let model = models_dir.join("demo");
    let _ = std::fs::remove_dir_all(&models_dir);
    std::fs::create_dir_all(&model).unwrap();
    mozart_core::preset::write_default(&model, &mozart_core::config::RvcSessionValues::default())
        .unwrap();
    let baseline = format!(
        "[daemon]\nrpc_timeout_ms = 300\n[pre]\ncapture_device = \"unconfigured-cap\"\nplayback_device = \"unconfigured-play\"\n[process.env]\nMOZART_PRE_STUB_DEVICE_REJECT_FILE = {:?}\nMOZART_RVC_STUB_MODELS = \"demo\"\nMOZART_RVC_STUB_PRELOAD = \"demo\"\n[rvc]\nmodel_id = \"demo\"\nmodels_dir = {:?}\n",
        reject.to_str().unwrap(),
        models_dir.to_str().unwrap()
    );
    let daemon = DaemonProcess::start_with_config(
        "device-use",
        &example("pre_stub"),
        &example("rvc_stub"),
        &baseline,
    );
    let conn = daemon.connect();
    assert!(wait_until(Duration::from_secs(5), || status(&conn)
        .pre
        .is_some()));

    // 不存在的设备在入口被拒绝。
    let invalid = device_use(&conn, "capture", "plughw:CARD=NoSuchCard,DEV=99");
    assert_eq!(invalid.status, "error");
    assert_eq!(invalid.code, ctl::CODE_INVALID_ARGUMENT);

    // 真实 ALSA 清单有采集设备时测完整热切生命周期；无声卡主机跳过。
    let Some(capture) = list_device(&conn, "capture") else {
        let _ = std::fs::remove_dir_all(&models_dir);
        return;
    };
    assert_ne!(capture, "unconfigured-cap");

    // pre 拒绝切换时保留旧设备，不污染状态。
    std::fs::write(&reject, b"reject").unwrap();
    let pre_err = device_use(&conn, "capture", &capture);
    assert_eq!(pre_err.status, "error");
    assert_eq!(pre_err.code, ctl::CODE_INVALID_ARGUMENT);
    assert_eq!(
        status(&conn).pre.unwrap().capture_device,
        "unconfigured-cap",
        "pre 拒绝后应保留旧生效设备"
    );
    std::fs::remove_file(&reject).unwrap();

    // 热切换成功：状态更新、不改写 config.toml、不标记 pending_restart。
    let switched = device_use(&conn, "capture", &capture);
    assert_eq!(switched.status, "ok", "reason: {}", switched.reason);
    assert_eq!(switched.data["kind"], "capture");
    assert_eq!(switched.data["device"], capture);
    assert_eq!(switched.data["previous_device"], "unconfigured-cap");
    assert!(wait_until(Duration::from_secs(5), || {
        status(&conn)
            .pre
            .is_some_and(|pre| pre.capture_device == capture)
    }));
    assert!(status(&conn).pending_restart.is_empty());
    let toml_after = std::fs::read_to_string(daemon.dir.join("config.toml")).unwrap();
    assert!(
        toml_after.contains("capture_device = \"unconfigured-cap\""),
        "{toml_after}"
    );

    // preset use 重载 config.toml 时不得冲掉 device use 临时设备。
    let preset = call(&conn, ctl::DAEMON_PRESET_USE, b"{}");
    assert_eq!(preset.status, "ok", "reason: {}", preset.reason);

    // pre 意外崩溃由 daemon 自愈拉起时，沿用临时切换的设备。
    let new_pid = crash_pre(&daemon, &conn);
    assert_eq!(arg_str(new_pid, "--effective-capture-device"), capture);
    assert!(wait_until(Duration::from_secs(5), || {
        status(&conn)
            .pre
            .is_some_and(|pre| pre.capture_device == capture)
    }));

    // 显式 restart pre 后回到 config.toml 基线。
    let restart = call(&conn, ctl::DAEMON_RESTART, br#"{"service":"pre"}"#);
    assert_eq!(restart.status, "ok", "reason: {}", restart.reason);
    assert!(wait_until(Duration::from_secs(5), || {
        let s = status(&conn);
        s.processes.pre.pid.is_some_and(|p| p != new_pid)
            && s.pre
                .is_some_and(|pre| pre.capture_device == "unconfigured-cap")
    }));
    let _ = std::fs::remove_dir_all(&models_dir);
}
