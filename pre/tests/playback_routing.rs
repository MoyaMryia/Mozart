//! 进程级集成测试：spawn mozart-pre（ALSA null 设备，无真实声学），TTS 桩经
//! tts-out.sock 喂 SpeechFrame，验证扬声器分时接线：播报期 GetStatus 为
//! broadcast 且播报帧计数增长，停喂后 holdoff 200 ms 恢复 realtime。
//! 路由选源与 holdoff 的逐分支时序由 playback 单元测试钉住；本用例验证
//! socket → 接收环 → 播放线程 → 状态上报的端到端接线。
//! 另含锁文件防重复实例用例（SPEC-02）：同 run_dir 第二实例必须失败退出。

mod common;
use common::*;

use mozart_core::ctl::Seqpacket;
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, OutputFrame, SpeechFrame, OUTPUT_SAMPLES};
use std::io::Read;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// 采集/播放走 ALSA null 设备的基线配置。
fn null_device_config(dir: &std::path::Path) -> String {
    format!(
        "[daemon]\nrun_dir = \"{}\"\n[pre]\ncapture_device = \"null\"\nplayback_device = \"null\"\n",
        dir.display()
    )
}

/// TTS 桩帧：48 kHz 单声道，meta 按 20 ms 步进。
fn tts_frame(idx: u32) -> OutputFrame {
    OutputFrame::new_48k(
        FrameMeta {
            pts_ns: idx as u64 * 20_000_000,
            frame_idx: idx,
            vad_flag: 1,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        },
        [0.25; OUTPUT_SAMPLES],
    )
}

#[test]
fn broadcast_priority_and_holdoff_recovery() {
    let dir = run_dir("sharing");
    let mut child = spawn_pre(&dir, &null_device_config(&dir));
    wait_sockets(&dir);
    let control = Seqpacket::connect(&dir.join("pre.sock")).unwrap();

    // 初始实时：无播报帧。
    let status = get_status(&control);
    assert_eq!(status["speaker_state"], "realtime");
    assert_eq!(status["tts_frames_played"], 0);

    // TTS 桩：连接 pre 绑定的 tts-out.sock，paced 喂帧（每 5 ms 一帧，
    // 远小于 200 ms holdoff，播报期状态持续 broadcast）。
    let tx = FrameSocket::<SpeechFrame>::connect(&dir.join("tts-out.sock"), 8).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let sender = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut idx = 0u32;
            let issued_ns = mozart_core::speech::monotonic_ns();
            while !stop.load(Relaxed) {
                let _ = tx.send(&SpeechFrame { issued_ns, frame: tts_frame(idx) });
                idx = idx.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };

    // 播报期：状态进入 broadcast，播报帧计数增长。
    wait_until("进入播报", Duration::from_secs(3), || {
        let status = get_status(&control);
        status["speaker_state"] == "broadcast"
            && status["tts_frames_played"].as_u64().unwrap() > 0
    });

    // 停喂：holdoff 200 ms 后恢复实时。
    stop.store(true, Relaxed);
    sender.join().unwrap();
    wait_until("恢复实时", Duration::from_secs(3), || {
        get_status(&control)["speaker_state"] == "realtime"
    });
    let status = get_status(&control);
    assert!(status["tts_frames_played"].as_u64().unwrap() > 0);
    // 实时播放路径也在走（null 设备持续写帧）。
    assert!(status["frames_played"].as_u64().unwrap() > 0);

    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(2));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn confirmed_speech_stop_rejects_delayed_old_frames_and_accepts_new() {
    let dir = run_dir("speech-fence");
    let mut child = spawn_pre(&dir, &null_device_config(&dir));
    wait_sockets(&dir);
    let control = Seqpacket::connect(&dir.join("pre.sock")).unwrap();
    let tx = FrameSocket::<SpeechFrame>::connect(&dir.join("tts-out.sock"), 8).unwrap();
    let old = SpeechFrame { issued_ns: mozart_core::speech::monotonic_ns(), frame: tts_frame(0) };
    tx.send(&old).unwrap();
    wait_until("首个旧代帧已播", Duration::from_secs(3), || get_status(&control)["tts_frames_played"] == 1);
    let cutoff_ns = mozart_core::speech::monotonic_ns();
    let body = serde_json::to_vec(&mozart_core::speech::StopFence { cutoff_ns }).unwrap();
    control.send(mozart_core::ctl::PRE_SPEECH_STOP, 80, &body).unwrap();
    let msg = control.recv(Some(Duration::from_secs(2))).unwrap();
    let reply: mozart_core::ctl::schema::Response = serde_json::from_slice(&msg.body).unwrap();
    assert_eq!(reply.status, "ok", "{}", reply.reason);
    assert_eq!(reply.data["cutoff_ns"], cutoff_ns);
    assert_eq!(reply.data["stopped"], true);

    for _ in 0..4 {
        tx.send(&old).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(get_status(&control)["tts_frames_played"], 1, "旧帧不得经过播放边界");
    let fresh = SpeechFrame { issued_ns: mozart_core::speech::monotonic_ns(), frame: tts_frame(1) };
    tx.send(&fresh).unwrap();
    wait_until("只接纳新的播报帧", Duration::from_secs(3), || get_status(&control)["tts_frames_played"] == 2);
    assert_eq!(get_status(&control)["tts_frames_played"], 2);
    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 锁文件防重复实例（SPEC-02）：同 run_dir 的第二实例必须立即失败退出，
/// 错误信息指明已有实例；首实例不受影响。
#[test]
fn second_instance_rejected_by_lock_file() {
    let dir = run_dir("lock");
    let mut first = spawn_pre(&dir, &null_device_config(&dir));
    wait_sockets(&dir);

    let mut second = Command::new(env!("CARGO_BIN_EXE_mozart-pre"))
        .arg("--socket")
        .arg(dir.join("pre.sock"))
        .arg("--config")
        .arg(dir.join("mozart.toml"))
        .env_remove("MOZART_CONFIG")
        .env_remove("MOZART_RUN_DIR")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("启动第二实例");
    let status = wait_exit(&mut second, Duration::from_secs(2));
    assert!(!status.success(), "第二实例必须失败退出：{status}");
    let mut stderr = String::new();
    second
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("pre already running"),
        "错误信息必须指明已有实例：{stderr}"
    );

    // 首实例不受影响：控制面仍可用，SIGTERM 正常退出。
    let control = Seqpacket::connect(&dir.join("pre.sock")).unwrap();
    assert_eq!(get_status(&control)["speaker_state"], "realtime");
    sigterm(&first.0);
    assert!(wait_exit(&mut first.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}