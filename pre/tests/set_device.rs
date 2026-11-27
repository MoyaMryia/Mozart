//! 进程级集成测试：PRE_SET_DEVICE 运行中热切设备契约与边界（SPEC-02）。
//! 校验：capture_file 模式下拒绝切换采集设备；无效播放设备打开失败时保留旧设备、
//! 播放计数持续推进不崩溃；同设备切换幂等。

mod common;
use common::*;

use mozart_core::ctl::{self, Seqpacket};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

fn write_silent_wav(path: &Path) {
    let data = vec![0u8; 48_000 * 2];
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&48_000u32.to_le_bytes());
    fmt.extend_from_slice(&96_000u32.to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());

    let mut body = Vec::new();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&data);

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(&body);
    std::fs::write(path, wav).unwrap();
}

fn call_error(conn: &Seqpacket, type_: u16, request_id: u32, body: &[u8]) -> Value {
    conn.send(type_, request_id, body).unwrap();
    let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
    let reply_type = reply.header.type_;
    let reply_request = reply.header.request_id;
    assert_eq!(reply_type, type_);
    assert_eq!(reply_request, request_id);
    let envelope: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(envelope["status"], "error", "应答必须是 error 信封：{envelope}");
    envelope
}

#[test]
fn set_device_rejects_capture_file_and_keeps_old_playback_on_open_failure() {
    let dir = run_dir("set-device");
    let capture_path = dir.join("capture.wav");
    write_silent_wav(&capture_path);
    let config = format!(
        "[daemon]\nrun_dir = \"{}\"\n[pre]\ncapture_device = \"null\"\nplayback_device = \"null\"\ncapture_file = \"{}\"\n",
        dir.display(),
        capture_path.display()
    );
    let mut child = spawn_pre(&dir, &config);
    wait_sockets(&dir);
    let control = Seqpacket::connect(&dir.join("pre.sock")).unwrap();

    wait_until("播放帧开始推进", Duration::from_secs(3), || {
        get_status(&control)["frames_played"].as_u64().unwrap_or(0) >= 3
    });

    // capture_file 模式下禁止切采集设备。
    let rejected = call_error(
        &control,
        ctl::PRE_SET_DEVICE,
        2,
        br#"{"kind":"capture","device":"null"}"#,
    );
    assert_eq!(rejected["code"], ctl::CODE_INVALID_ARGUMENT);
    assert!(rejected["reason"].as_str().unwrap().contains("capture_file"));

    // 播放切到无效设备失败：保留旧设备 null，不退出进程。
    let failed = call_error(
        &control,
        ctl::PRE_SET_DEVICE,
        3,
        br#"{"kind":"playback","device":"plughw:CARD=NoSuchCard,DEV=99"}"#,
    );
    assert_eq!(failed["code"], ctl::CODE_INVALID_ARGUMENT);
    let before_played = get_status(&control)["frames_played"].as_u64().unwrap();
    assert_eq!(get_status(&control)["playback_device"], "null");
    wait_until("失败后旧播放设备继续推帧", Duration::from_secs(3), || {
        get_status(&control)["frames_played"].as_u64().unwrap() > before_played + 2
    });

    // 同设备幂等返回。
    let same = call_ok(
        &control,
        ctl::PRE_SET_DEVICE,
        4,
        br#"{"kind":"playback","device":"null"}"#,
    );
    assert_eq!(same["device"], "null");
    assert_eq!(same["previous_device"], "null");

    // 真实非幂等热切：null -> plug:null -> null，验证 apply_pending + FadeIn + 持续推帧。
    let switched = call_ok(
        &control,
        ctl::PRE_SET_DEVICE,
        5,
        br#"{"kind":"playback","device":"plug:null"}"#,
    );
    assert_eq!(switched["device"], "plug:null");
    assert_eq!(switched["previous_device"], "null");
    assert_eq!(get_status(&control)["playback_device"], "plug:null");
    let after_switch = get_status(&control)["frames_played"].as_u64().unwrap();
    wait_until("切换到 plug:null 后播放帧持续推进", Duration::from_secs(3), || {
        get_status(&control)["frames_played"].as_u64().unwrap() > after_switch + 2
    });

    let back = call_ok(
        &control,
        ctl::PRE_SET_DEVICE,
        6,
        br#"{"kind":"playback","device":"null"}"#,
    );
    assert_eq!(back["device"], "null");
    assert_eq!(back["previous_device"], "plug:null");
    assert_eq!(get_status(&control)["playback_device"], "null");

    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(3));
    assert!(status.success(), "pre 应优雅退出：{status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_devices_start_in_standby_and_hot_switch_recovers() {
    let dir = run_dir("standby-device");
    let config = format!(
        "[daemon]\nrun_dir = \"{}\"\n[pre]\ncapture_device = \"nonexistent-cap\"\nplayback_device = \"nonexistent-play\"\n",
        dir.display(),
    );
    let mut child = spawn_pre(&dir, &config);
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("pre.sock")).unwrap();
    let status = get_status(&control);
    assert_eq!(status["state"], "running");
    assert_eq!(status["frames_captured"], 0);
    assert_eq!(status["frames_played"], 0);
    let last_error = status["last_error"].as_str().unwrap_or("");
    assert!(
        last_error.contains("playback open (nonexistent-play)")
            && last_error.contains("capture open (nonexistent-cap)"),
        "缺失设备启动应保留可查询故障原因：{last_error:?}"
    );

    let switched_cap = call_ok(
        &control,
        ctl::PRE_SET_DEVICE,
        1,
        br#"{"kind":"capture","device":"null"}"#,
    );
    assert_eq!(switched_cap["device"], "null");
    assert_eq!(switched_cap["previous_device"], "nonexistent-cap");

    let switched_play = call_ok(
        &control,
        ctl::PRE_SET_DEVICE,
        2,
        br#"{"kind":"playback","device":"null"}"#,
    );
    assert_eq!(switched_play["device"], "null");
    assert_eq!(switched_play["previous_device"], "nonexistent-play");

    wait_until("采集与播放切回有效设备后恢复计数", Duration::from_secs(3), || {
        let st = get_status(&control);
        st["frames_captured"].as_u64().unwrap_or(0) > 0
            && st["frames_played"].as_u64().unwrap_or(0) > 0
    });
    assert_eq!(
        get_status(&control)["last_error"],
        Value::Null,
        "双侧热切恢复后应清除启动期待机打开报错"
    );

    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(3));
    assert!(status.success(), "pre 应优雅退出：{status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
