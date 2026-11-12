//! 进程级集成测试：spawn mozart-pre（ALSA null 设备，无真实声学），TTS 桩经
//! tts-out.sock 喂 OutputFrame，验证扬声器分时接线：播报期 GetStatus 为
//! broadcast 且播报帧计数增长，停喂后 holdoff 200 ms 恢复 realtime。
//! 路由选源与 holdoff 的逐分支时序由 playback 单元测试钉住；本用例验证
//! socket → 接收环 → 播放线程 → 状态上报的端到端接线。

use mozart_core::ctl::{self, Seqpacket};
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, OutputFrame, OUTPUT_SAMPLES};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 子进程守卫：断言失败也不留孤儿进程。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 用例独立运行目录（pid + 序号防并行碰撞）。
fn run_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mozart-pre-route-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写配置并拉起 pre；采集/播放用 ALSA null 设备（无硬件依赖）。
fn spawn_pre(dir: &Path) -> ChildGuard {
    let config = dir.join("mozart.toml");
    std::fs::write(
        &config,
        format!(
            "[daemon]\nrun_dir = \"{}\"\n[pre]\ncapture_device = \"null\"\nplayback_device = \"null\"\n",
            dir.display()
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mozart-pre"));
    command
        .arg("--socket")
        .arg(dir.join("pre.sock"))
        .arg("--config")
        .arg(&config)
        .env_remove("MOZART_CONFIG")
        .env_remove("MOZART_RUN_DIR");
    ChildGuard(command.spawn().expect("启动 mozart-pre"))
}

fn wait_until(what: &str, timeout: Duration, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !pred() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 控制面与播报输出 socket 就绪（pre 绑定 tts-out.sock）。
fn wait_sockets(dir: &Path) {
    wait_until("socket 就绪", Duration::from_secs(5), || {
        dir.join("pre.sock").exists() && dir.join("tts-out.sock").exists()
    });
}

fn call_ok(conn: &Seqpacket, type_: u16, request_id: u32) -> Value {
    conn.send(type_, request_id, b"").unwrap();
    let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
    let reply_type = reply.header.type_;
    let reply_request = reply.header.request_id;
    assert_eq!(reply_type, type_);
    assert_eq!(reply_request, request_id);
    let envelope: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(envelope["status"], "ok", "控制应答：{envelope}");
    envelope["data"].clone()
}

fn get_status(conn: &Seqpacket) -> Value {
    call_ok(conn, ctl::PRE_GET_STATUS, 1)
}

fn sigterm(child: &Child) {
    // SAFETY: kill 只发信号；pid 来自本用例 spawn 的子进程。
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "SIGTERM: {}", std::io::Error::last_os_error());
}

fn wait_exit(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "SIGTERM 后未在时限内退出");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// TTS 桩帧：48 kHz 单声道，meta 按 20 ms 步进。
fn tts_frame(idx: u32) -> OutputFrame {
    OutputFrame {
        meta: FrameMeta {
            pts_ns: idx as u64 * 20_000_000,
            frame_idx: idx,
            vad_flag: 1,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        },
        pcm: [0.25; OUTPUT_SAMPLES],
    }
}

#[test]
fn broadcast_priority_and_holdoff_recovery() {
    let dir = run_dir("sharing");
    let mut child = spawn_pre(&dir);
    wait_sockets(&dir);
    let control = Seqpacket::connect(&dir.join("pre.sock")).unwrap();

    // 初始实时：无播报帧。
    let status = get_status(&control);
    assert_eq!(status["speaker_state"], "realtime");
    assert_eq!(status["tts_frames_played"], 0);

    // TTS 桩：连接 pre 绑定的 tts-out.sock，paced 喂帧（每 5 ms 一帧，
    // 远小于 200 ms holdoff，播报期状态持续 broadcast）。
    let tx = FrameSocket::<OutputFrame>::connect(&dir.join("tts-out.sock"), 8).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let sender = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut idx = 0u32;
            while !stop.load(Relaxed) {
                let _ = tx.send(&tts_frame(idx));
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