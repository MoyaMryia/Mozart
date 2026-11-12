//! 进程级集成测试（stub 模式，无需 sherpa 库）：spawn mozart-asr 二进制，
//! DGRAM 喂 InputFrame，控制面 Hello/GetStatus，事件面收 JSON，SIGTERM 收尾。
//! 默认 stub 返回空文本（无事件）；MOZART_ASR_STUB_TEXT 注入文本验证事件链路。

use mozart_core::ctl::{self, Seqpacket};
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, InputFrame, INPUT_SAMPLES};
use serde_json::Value;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
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
        "mozart-asr-proc-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写配置并拉起进程；stub_text 控制在线 stub 的注入文本。
fn spawn_asr(dir: &Path, review_model_dir: &str, stub_text: Option<&str>) -> ChildGuard {
    let config = dir.join("mozart.toml");
    std::fs::write(
        &config,
        format!(
            "[daemon]\nrun_dir = \"{}\"\n[asr]\nmodel_dir = \"/nonexistent-zipformer\"\n\
             review_model_dir = \"{review_model_dir}\"\nthreads = 1\n",
            dir.display()
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mozart-asr"));
    command
        .arg("--socket")
        .arg(dir.join("asr.sock"))
        .arg("--config")
        .arg(&config)
        .env_remove("MOZART_CONFIG")
        .env_remove("MOZART_RUN_DIR")
        .env_remove("MOZART_ASR_STUB_TEXT")
        .env("MOZART_ASR_STUB", "1");
    if let Some(text) = stub_text {
        command.env("MOZART_ASR_STUB_TEXT", text);
    }
    ChildGuard(command.spawn().expect("启动 mozart-asr"))
}

fn wait_until(what: &str, timeout: Duration, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !pred() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 三个 socket 文件就绪（控制面先绑，事件面最后）。
fn wait_sockets(dir: &Path) {
    wait_until("socket 就绪", Duration::from_secs(5), || {
        dir.join("asr.sock").exists()
            && dir.join("asr-in.sock").exists()
            && dir.join("asr-events.sock").exists()
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
    call_ok(conn, ctl::ASR_GET_STATUS, 1)
}

fn frame(idx: u32) -> InputFrame {
    InputFrame {
        meta: FrameMeta {
            pts_ns: idx as u64 * mozart_core::FRAME_MS as u64 * 1_000_000,
            frame_idx: idx,
            vad_flag: 1,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        },
        pcm: [0.25; INPUT_SAMPLES],
    }
}

/// 最小 SEQPACKET 事件客户端：连接 + 带超时收一条完整消息。
struct EventClient {
    fd: OwnedFd,
}

impl EventClient {
    fn connect(path: &Path) -> Self {
        // SAFETY: socket() 不访问其他内存。
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0, "socket: {}", io::Error::last_os_error());
        // SAFETY: sockaddr_un 可由全零位模式表示。
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_os_str().as_bytes();
        assert!(bytes.len() < addr.sun_path.len());
        for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
        // SAFETY: fd 有效；addr 指向已初始化的 sockaddr_un。
        let rc = unsafe { libc::connect(raw, (&addr as *const libc::sockaddr_un).cast(), len) };
        assert_eq!(rc, 0, "connect: {}", io::Error::last_os_error());
        // SAFETY: raw 是刚创建且未复制的文件描述符。
        Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        }
    }

    fn recv(&self, timeout: Duration) -> io::Result<Vec<u8>> {
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: pollfd 指向单个已初始化结构。
        let rc = unsafe { libc::poll(&mut pollfd, 1, ms) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if rc == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "事件等待超时"));
        }
        let mut buf = vec![0u8; 65536];
        // SAFETY: buf 有效且长度与传入一致。
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }
}

fn read_event(client: &EventClient) -> Value {
    let bytes = client.recv(Duration::from_secs(3)).expect("读事件");
    serde_json::from_slice(&bytes).expect("事件是 JSON")
}

fn sigterm(child: &Child) {
    // SAFETY: kill 只发信号；pid 来自本用例 spawn 的子进程。
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "SIGTERM: {}", io::Error::last_os_error());
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

#[test]
fn stub_process_control_counters_and_clean_exit() {
    let dir = run_dir("plain");
    let mut child = spawn_asr(&dir, "", None);
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("asr.sock")).unwrap();
    let hello = call_ok(&control, ctl::ASR_HELLO, 7);
    let startup_id = hello["startup_id"].as_str().unwrap().to_string();
    assert_eq!(startup_id.len(), 8);
    assert!(startup_id.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(hello["version"].as_str().is_some());

    let status = get_status(&control);
    assert_eq!(status["state"], "idle");
    assert_eq!(status["startup_id"], startup_id);
    assert_eq!(status["frames_received"], 0);
    assert_eq!(status["bridge_connected"], false);

    // 桥连接事件面：状态翻转为已连接。
    let events = EventClient::connect(&dir.join("asr-events.sock"));
    wait_until("桥连接", Duration::from_secs(2), || {
        get_status(&control)["bridge_connected"] == true
    });

    // 喂 5 帧：收包计数增长；默认 stub 空文本，无事件。
    let sender = FrameSocket::<InputFrame>::connect(&dir.join("asr-in.sock"), 8).unwrap();
    for idx in 0..5 {
        sender.send(&frame(idx)).unwrap();
    }
    wait_until("收包计数", Duration::from_secs(2), || {
        get_status(&control)["frames_received"] == 5
    });
    assert!(
        events.recv(Duration::from_millis(300)).is_err(),
        "空 stub 不应发事件"
    );

    // SIGTERM：无句可切，干净退出。
    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stub_text_emits_partial_and_sigterm_final() {
    let dir = run_dir("text");
    let mut child = spawn_asr(&dir, "", Some("你好世界"));
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("asr.sock")).unwrap();
    let hello = call_ok(&control, ctl::ASR_HELLO, 1);
    let startup_id = hello["startup_id"].as_str().unwrap().to_string();
    let events = EventClient::connect(&dir.join("asr-events.sock"));
    wait_until("桥连接", Duration::from_secs(2), || {
        get_status(&control)["bridge_connected"] == true
    });

    let sender = FrameSocket::<InputFrame>::connect(&dir.join("asr-in.sock"), 8).unwrap();
    // partial 节流窗口 100 ms 自解码器构造起算：等窗口过后再喂首帧。
    std::thread::sleep(Duration::from_millis(150));
    for idx in 0..3 {
        sender.send(&frame(idx)).unwrap();
    }
    // partial：携带启动号与句号。
    let partial = read_event(&events);
    assert_eq!(partial["type"], "partial");
    assert_eq!(partial["startup_id"], startup_id);
    assert_eq!(partial["utterance_id"], format!("{startup_id}:1"));
    assert_eq!(partial["text"], "你好世界");
    assert_eq!(get_status(&control)["finals"], 0);

    // SIGTERM：喂静音切出最后 final（复核关闭 → disabled）。
    sigterm(&child.0);
    let final_event = read_event(&events);
    assert_eq!(final_event["type"], "final");
    assert_eq!(final_event["startup_id"], startup_id);
    assert_eq!(final_event["utterance_id"], format!("{startup_id}:1"));
    assert_eq!(final_event["text"], "你好世界");
    assert_eq!(final_event["refinement_status"], "disabled");
    let status = wait_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stub_review_enabled_emits_refined_failed() {
    let dir = run_dir("review");
    // stub 离线识别器返回空文本 → 每句 failed（在线识别不受影响）。
    let mut child = spawn_asr(&dir, "/nonexistent-sensevoice", Some("复核句"));
    wait_sockets(&dir);

    let control = Seqpacket::connect(&dir.join("asr.sock")).unwrap();
    let events = EventClient::connect(&dir.join("asr-events.sock"));
    wait_until("桥连接", Duration::from_secs(2), || {
        get_status(&control)["bridge_connected"] == true
    });

    let sender = FrameSocket::<InputFrame>::connect(&dir.join("asr-in.sock"), 8).unwrap();
    // partial 节流窗口 100 ms 自解码器构造起算：等窗口过后再喂首帧。
    std::thread::sleep(Duration::from_millis(150));
    sender.send(&frame(0)).unwrap();
    let partial = read_event(&events);
    assert_eq!(partial["type"], "partial");

    sigterm(&child.0);
    let final_event = read_event(&events);
    assert_eq!(final_event["type"], "final");
    assert_eq!(final_event["refinement_status"], "pending");
    let refined = read_event(&events);
    assert_eq!(refined["type"], "refined");
    assert_eq!(refined["utterance_id"], final_event["utterance_id"]);
    assert_eq!(refined["refinement_status"], "failed");
    assert!(refined["refinement_error"].as_str().is_some());
    // 在线文本保留。
    assert_eq!(refined["text"], "复核句");

    let status = wait_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "退出状态：{status}");
    let _ = std::fs::remove_dir_all(&dir);
}