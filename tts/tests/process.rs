//! 进程级集成测试（stub 模式，无需 sherpa 库）：spawn mozart-tts 二进制，
//! 控制面 Hello/GetStatus，任务面提交/受理应答，输出帧计数，SIGTERM 收尾。
//! v1 输出侧 TTS 绑定 tts-out.sock、无接收方：帧发送全部丢弃计数，
//! 音频内容正确性由 resample/output 单元测试钉住。

use mozart_core::ctl::{self, Seqpacket};
use mozart_core::dgram::FrameSocket;
use mozart_core::InputFrame;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
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
        "mozart-tts-proc-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写配置并拉起进程；tts_extra 追加 [tts] 段键值。
fn spawn_tts(dir: &Path, tts_extra: &str) -> ChildGuard {
    let config = dir.join("mozart.toml");
    std::fs::write(
        &config,
        format!(
            "[daemon]\nrun_dir = \"{}\"\n[tts]\nthreads = 1\n{tts_extra}",
            dir.display()
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mozart-tts"));
    command
        .arg("--socket")
        .arg(dir.join("tts.sock"))
        .arg("--config")
        .arg(&config)
        .env_remove("MOZART_CONFIG")
        .env_remove("MOZART_RUN_DIR");
    ChildGuard(command.spawn().expect("启动 mozart-tts"))
}

fn wait_until(what: &str, timeout: Duration, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !pred() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 三个 socket 文件就绪（控制面先绑，输出面最后）。
fn wait_sockets(dir: &Path) {
    wait_until("socket 就绪", Duration::from_secs(5), || {
        dir.join("tts.sock").exists()
            && dir.join("tts-jobs.sock").exists()
            && dir.join("tts-out.sock").exists()
    });
}

/// 控制面请求：回 ok 信封的 data。
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
    call_ok(conn, ctl::TTS_GET_STATUS, 1)
}

/// 任务面提交：回 TTS_JOB_REPLY 的 JSON 体，校验命令号与 request_id 回显。
fn submit_job(conn: &Seqpacket, request_id: u32, body: &Value) -> Value {
    let bytes = serde_json::to_vec(body).unwrap();
    conn.send(ctl::TTS_JOB_SUBMIT, request_id, &bytes).unwrap();
    let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
    let reply_type = reply.header.type_;
    let reply_request = reply.header.request_id;
    assert_eq!(reply_type, ctl::TTS_JOB_REPLY);
    assert_eq!(reply_request, request_id);
    serde_json::from_slice(&reply.body).expect("受理应答是 JSON")
}

fn sigterm(child: &Child) {
    // SAFETY: kill 只发信号；pid 来自本用例 spawn 的子进程。
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "SIGTERM: {}", std::io::Error::last_os_error());
}

/// 限时等待退出，返回退出状态。
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

/// 生成 16-bit PCM 单声道 WAV（参考音色夹具）。
fn write_wav(path: &Path, rate: u32, samples: &[i16]) {
    let data: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    std::fs::write(path, out).unwrap();
}

#[test]
fn stub_process_control_and_clean_exit() {
    let dir = run_dir("plain");
    let mut child = spawn_tts(&dir, "");
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("tts.sock")).unwrap();
    let hello = call_ok(&control, ctl::TTS_HELLO, 7);
    assert!(hello["version"].as_str().is_some());

    wait_until("模型加载", Duration::from_secs(5), || {
        get_status(&control)["model_loaded"] == true
    });
    let status = get_status(&control);
    assert_eq!(status["state"], "idle");
    assert_eq!(status["synthesized"], 0);
    assert_eq!(status["failed"], 0);
    assert_eq!(status["skipped"], 0);
    assert_eq!(status["queue_depth"], 0);
    assert_eq!(status["frames_out"], 0);
    assert_eq!(status["output_dropped"], 0);
    assert_eq!(status["last_error"], Value::Null);

    // SIGTERM：无在途任务，daemon 2 s 宽限内干净退出。
    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(2));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn job_submit_reply_and_output_frames() {
    let dir = run_dir("job");
    let mut child = spawn_tts(&dir, "");
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("tts.sock")).unwrap();
    wait_until("模型加载", Duration::from_secs(5), || {
        get_status(&control)["model_loaded"] == true
    });
    let jobs = Seqpacket::connect(&dir.join("tts-jobs.sock")).unwrap();

    let reply = submit_job(
        &jobs,
        42,
        &json!({"text": "Hello world", "utterance_id": "ab12cd34:1"}),
    );
    assert_eq!(reply["status"], "submitted");
    assert_eq!(reply["utterance_id"], "ab12cd34:1");
    assert!(reply.get("reason").is_none());

    // stub 合成 0.1 s @ 24 kHz → 1600 samples @ 16 kHz → 恰 5 契约帧。
    wait_until("合成完成", Duration::from_secs(5), || {
        get_status(&control)["synthesized"] == 1
    });
    let status = get_status(&control);
    assert_eq!(status["failed"], 0);
    assert_eq!(status["queue_depth"], 0);
    assert_eq!(status["state"], "idle");
    assert_eq!(status["frames_out"], 5);
    // v1 无接收方：绑定侧发送全部丢弃计数。
    assert_eq!(status["output_dropped"], 5);

    // 输出 socket 已绑定可连接（接收方接 RVC 留 FUTURE）。
    let probe = FrameSocket::<InputFrame>::connect(&dir.join("tts-out.sock"), 8).unwrap();
    drop(probe);

    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(2));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn queue_full_burst_conserves_jobs() {
    let dir = run_dir("burst");
    let mut child = spawn_tts(&dir, "queue_capacity = 1\n");
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("tts.sock")).unwrap();
    wait_until("模型加载", Duration::from_secs(5), || {
        get_status(&control)["model_loaded"] == true
    });
    let jobs = Seqpacket::connect(&dir.join("tts-jobs.sock")).unwrap();

    // 容量 1：突发 32 句，受理只有 submitted/skipped 两种；
    // 每句要么入队合成、要么队满拒收，总数守恒。
    const BURST: u32 = 32;
    let mut submitted = 0u32;
    let mut skipped = 0u32;
    for idx in 0..BURST {
        let reply = submit_job(
            &jobs,
            idx,
            &json!({"text": "Burst sentence", "utterance_id": format!("ab12cd34:{}", idx + 1)}),
        );
        match reply["status"].as_str().unwrap() {
            "submitted" => submitted += 1,
            "skipped" => {
                skipped += 1;
                assert_eq!(reply["reason"], "queue full");
            }
            other => panic!("意外受理状态：{other}"),
        }
    }
    assert!(skipped > 0, "容量 1 突发 {BURST} 句应有队满拒收");
    wait_until("突发处理完毕", Duration::from_secs(10), || {
        let status = get_status(&control);
        status["synthesized"].as_u64().unwrap() + status["skipped"].as_u64().unwrap()
            == BURST as u64
    });
    let status = get_status(&control);
    assert_eq!(status["synthesized"].as_u64().unwrap(), submitted as u64);
    assert_eq!(status["skipped"].as_u64().unwrap(), skipped as u64);
    assert_eq!(status["failed"], 0);
    assert_eq!(status["queue_depth"], 0);

    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(2));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_jobs_reply_failed_and_process_continues() {
    let dir = run_dir("invalid");
    let mut child = spawn_tts(&dir, "");
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("tts.sock")).unwrap();
    wait_until("模型加载", Duration::from_secs(5), || {
        get_status(&control)["model_loaded"] == true
    });
    let jobs = Seqpacket::connect(&dir.join("tts-jobs.sock")).unwrap();

    let bad_bodies = [
        json!({"text": "", "utterance_id": "x:1"}),
        json!({"text": "hi"}),
        json!({"text": "hi", "utterance_id": "x:1", "voice_params": 42}),
    ];
    for (idx, body) in bad_bodies.iter().enumerate() {
        let reply = submit_job(&jobs, idx as u32, body);
        assert_eq!(reply["status"], "failed", "body: {body}");
        assert!(reply["reason"].as_str().is_some());
    }
    // 未知命令号同样回 failed。
    jobs.send(ctl::TTS_HELLO, 99, b"").unwrap();
    let reply = jobs.recv(Some(Duration::from_secs(2))).unwrap();
    let reply_type = reply.header.type_;
    assert_eq!(reply_type, ctl::TTS_JOB_REPLY);
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["status"], "failed");

    // 无效请求不入队：后续合法任务正常受理。
    let reply = submit_job(
        &jobs,
        100,
        &json!({"text": "Still alive", "utterance_id": "x:2"}),
    );
    assert_eq!(reply["status"], "submitted");
    wait_until("合法任务合成", Duration::from_secs(5), || {
        get_status(&control)["synthesized"] == 1
    });
    assert_eq!(get_status(&control)["queue_depth"], 0);

    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(2));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_reference_wav_fails_startup() {
    let dir = run_dir("badwav");
    std::fs::write(dir.join("ref.wav"), b"not a wav file").unwrap();
    let mut child = spawn_tts(
        &dir,
        &format!("reference_wav = \"{}\"\n", dir.join("ref.wav").display()),
    );
    let status = wait_exit(&mut child.0, Duration::from_secs(5));
    assert!(!status.success(), "坏参考 WAV 应启动失败：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reference_wav_loads_at_startup() {
    let dir = run_dir("wav");
    let samples: Vec<i16> = (0..2400).map(|i| ((i % 100) as i16 - 50) * 100).collect();
    write_wav(&dir.join("ref.wav"), 24000, &samples);
    let mut child = spawn_tts(
        &dir,
        &format!("reference_wav = \"{}\"\n", dir.join("ref.wav").display()),
    );
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("tts.sock")).unwrap();
    wait_until("模型加载", Duration::from_secs(5), || {
        get_status(&control)["model_loaded"] == true
    });
    let jobs = Seqpacket::connect(&dir.join("tts-jobs.sock")).unwrap();
    let reply = submit_job(
        &jobs,
        1,
        &json!({"text": "With reference", "utterance_id": "wav:1"}),
    );
    assert_eq!(reply["status"], "submitted");
    wait_until("合成完成", Duration::from_secs(5), || {
        get_status(&control)["synthesized"] == 1
    });

    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(2));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}