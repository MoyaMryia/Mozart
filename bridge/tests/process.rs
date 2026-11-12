//! 进程级集成测试：spawn mozart-bridge 二进制，mock ASR 事件面（裸 JSON SEQPACKET），
//! 验证字幕落盘、启动号校验与重连、开关命令与状态文件恢复、SIGTERM 收尾。
//! 翻译/播报投递在 Task 5 接入，此处只验证待翻译队列钩子。

use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::io;
use std::mem;
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
        "mozart-bridge-proc-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写配置并拉起进程；bridge_extra 追加 [bridge] 段键值。
fn spawn_bridge(dir: &Path, bridge_extra: &str) -> ChildGuard {
    let config = dir.join("mozart.toml");
    std::fs::write(
        &config,
        format!(
            "[daemon]\nrun_dir = \"{}\"\n[bridge]\nsubtitles_path = \"{}\"\n{bridge_extra}",
            dir.display(),
            dir.join("subtitles.jsonl").display()
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mozart-bridge"));
    command
        .arg("--socket")
        .arg(dir.join("bridge.sock"))
        .arg("--config")
        .arg(&config)
        .env_remove("MOZART_CONFIG")
        .env_remove("MOZART_RUN_DIR");
    ChildGuard(command.spawn().expect("启动 mozart-bridge"))
}

fn wait_until(what: &str, timeout: Duration, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !pred() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 连接控制面：重启场景下陈旧 socket 文件先于重绑定存在，重试到真正可连接。
fn connect_control(dir: &Path) -> Seqpacket {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Seqpacket::connect(&dir.join("bridge.sock")) {
            Ok(conn) => return conn,
            Err(err) => {
                assert!(Instant::now() < deadline, "连接 bridge.sock 超时：{err}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// 控制面请求：回 ok 信封的 data。
fn call_ok(conn: &Seqpacket, type_: u16, request_id: u32, body: &[u8]) -> Value {
    let envelope = call(conn, type_, request_id, body);
    assert_eq!(envelope["status"], "ok", "控制应答：{envelope}");
    envelope["data"].clone()
}

/// 控制面请求：回完整信封。
fn call(conn: &Seqpacket, type_: u16, request_id: u32, body: &[u8]) -> Value {
    conn.send(type_, request_id, body).unwrap();
    let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
    let reply_type = reply.header.type_;
    let reply_request = reply.header.request_id;
    assert_eq!(reply_type, type_);
    assert_eq!(reply_request, request_id);
    serde_json::from_slice(&reply.body).unwrap()
}

fn get_status(conn: &Seqpacket) -> Value {
    call_ok(conn, ctl::BRIDGE_GET_STATUS, 1, b"")
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

fn read_lines(dir: &Path) -> Vec<Value> {
    let content = std::fs::read_to_string(dir.join("subtitles.jsonl")).unwrap_or_default();
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("字幕行是合法 JSON"))
        .collect()
}

// ---- mock ASR 事件面：裸 JSON SEQPACKET 监听（识别服务监听、桥连接） ----

struct MockAsr {
    fd: OwnedFd,
}

impl MockAsr {
    fn bind(path: &Path) -> Self {
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
            )
        };
        assert!(raw >= 0, "socket: {}", io::Error::last_os_error());
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let _ = std::fs::remove_file(path);
        let (addr, len) = sockaddr(path);
        let rc = unsafe { libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) };
        assert_eq!(rc, 0, "bind: {}", io::Error::last_os_error());
        let rc = unsafe { libc::listen(fd.as_raw_fd(), 1) };
        assert_eq!(rc, 0, "listen: {}", io::Error::last_os_error());
        Self { fd }
    }

    /// 限时等待桥连接。
    fn accept(&self) -> MockConn {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut pollfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pollfd, 1, 100) };
            assert!(rc >= 0, "poll: {}", io::Error::last_os_error());
            if rc > 0 {
                let raw = unsafe {
                    libc::accept4(
                        self.fd.as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        libc::SOCK_CLOEXEC,
                    )
                };
                assert!(raw >= 0, "accept: {}", io::Error::last_os_error());
                return MockConn {
                    fd: unsafe { OwnedFd::from_raw_fd(raw) },
                };
            }
            assert!(Instant::now() < deadline, "等待桥连接超时");
        }
    }
}

struct MockConn {
    fd: OwnedFd,
}

impl MockConn {
    fn send_event(&self, event: &Value) {
        let bytes = serde_json::to_vec(event).unwrap();
        let sent = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        assert_eq!(sent as usize, bytes.len(), "事件发送：{}", io::Error::last_os_error());
    }
}

fn sockaddr(path: &Path) -> (libc::sockaddr_un, libc::socklen_t) {
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    assert!(bytes.len() < addr.sun_path.len());
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    (addr, len as libc::socklen_t)
}

fn event(kind: &str, startup: &str, seq: u64, text: &str) -> Value {
    json!({
        "type": kind,
        "startup_id": startup,
        "utterance_id": format!("{startup}:{seq}"),
        "seq": seq,
        "text": text,
        "emitted_at": 1731234567.89,
    })
}

fn refined_event(startup: &str, seq: u64, text: &str, refined_text: &str) -> Value {
    let mut value = event("refined", startup, seq, text);
    value["refinement_status"] = json!("completed");
    value["refined_text"] = json!(refined_text);
    value
}

// ---- 用例 ----

#[test]
fn subtitles_flow_status_and_sigterm() {
    let dir = run_dir("flow");
    let mock = MockAsr::bind(&dir.join("asr-events.sock"));
    let mut child = spawn_bridge(&dir, "");
    let control = connect_control(&dir);
    let conn = mock.accept();

    conn.send_event(&event("partial", "a1b2c3d4", 1, "你"));
    conn.send_event(&event("partial", "a1b2c3d4", 1, "你好"));
    conn.send_event(&event("final", "a1b2c3d4", 1, "你好"));
    conn.send_event(&refined_event("a1b2c3d4", 1, "你好", "你好。"));
    wait_until("字幕落盘 4 行", Duration::from_secs(5), || {
        read_lines(&dir).len() == 4
    });

    let lines = read_lines(&dir);
    let expected = [
        ("partial", 0u64, "你"),
        ("partial", 1, "你好"),
        ("final", 2, "你好"),
        ("refined", 3, "你好。"),
    ];
    for (line, (kind, revision, text)) in lines.iter().zip(expected) {
        assert_eq!(line["type"], kind);
        assert_eq!(line["revision"], revision);
        assert_eq!(line["text"], text);
        assert_eq!(line["utterance_id"], "a1b2c3d4:1");
        assert_eq!(line["startup_id"], "a1b2c3d4");
        assert!(line["timestamp_ms"].as_u64().unwrap() > 0);
        assert_eq!(line.as_object().unwrap().len(), 6);
    }

    let hello = call_ok(&control, ctl::BRIDGE_HELLO, 2, b"");
    assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));

    let status = get_status(&control);
    assert_eq!(status["subtitles_published"], 1);
    assert_eq!(status["records_in_memory"], 1);
    assert_eq!(status["translate_queue_depth"], 1);
    assert_eq!(status["speech_queue_depth"], 0);
    assert_eq!(status["translate_enabled"], true);
    assert_eq!(status["speech_enabled"], true);
    assert_eq!(
        status["subtitles_path"],
        dir.join("subtitles.jsonl").display().to_string()
    );
    assert!(status["last_write_unix_ms"].as_u64().unwrap() > 0);
    assert_eq!(status["degraded"], json!([]));

    // SIGTERM：2 s 宽限内退出，存档不受影响。
    sigterm(&child.0);
    let status = wait_exit(&mut child.0, Duration::from_secs(2));
    assert!(status.success(), "退出状态：{status}");
    assert_eq!(read_lines(&dir).len(), 4);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn startup_id_filters_stale_and_reconnect_adopts_new() {
    let dir = run_dir("startup");
    let mock = MockAsr::bind(&dir.join("asr-events.sock"));
    let mut child = spawn_bridge(&dir, "");
    let control = connect_control(&dir);
    let conn = mock.accept();

    // 首条事件确立会话 aaaa；bbbb 的事件被丢弃。
    conn.send_event(&event("partial", "aaaa", 1, "甲"));
    conn.send_event(&event("partial", "bbbb", 1, "乙"));
    conn.send_event(&event("final", "aaaa", 1, "甲"));
    wait_until("字幕落盘 2 行", Duration::from_secs(5), || {
        read_lines(&dir).len() == 2
    });
    let status = get_status(&control);
    assert_eq!(status["records_in_memory"], 1);
    assert_eq!(status["subtitles_published"], 1);

    // 断连且识别服务下线：降级持续可查；重新上线后新启动号被接纳。
    drop(conn);
    drop(mock);
    wait_until("断连降级可查", Duration::from_secs(5), || {
        get_status(&control)["degraded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().starts_with("asr events disconnected"))
    });
    let mock = MockAsr::bind(&dir.join("asr-events.sock"));
    let conn = mock.accept();
    conn.send_event(&event("partial", "bbbb", 2, "乙"));
    wait_until("重连后落盘 3 行", Duration::from_secs(5), || {
        read_lines(&dir).len() == 3
    });
    let status = get_status(&control);
    assert_eq!(status["records_in_memory"], 2);
    assert_eq!(status["subtitles_published"], 2);
    assert_eq!(status["degraded"], json!([]));
    let lines = read_lines(&dir);
    assert_eq!(lines[2]["startup_id"], "bbbb");
    assert_eq!(lines[2]["utterance_id"], "bbbb:2");

    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn switches_persist_across_restart() {
    let dir = run_dir("persist");
    let mock = MockAsr::bind(&dir.join("asr-events.sock"));
    let mut child = spawn_bridge(&dir, "");
    let control = connect_control(&dir);
    let _conn = mock.accept();

    // 运行时覆盖：translate off、speech off。
    call_ok(
        &control,
        ctl::BRIDGE_TRANSLATE_SET,
        2,
        br#"{"enabled":false}"#,
    );
    call_ok(&control, ctl::BRIDGE_SPEECH_SET, 3, br#"{"enabled":false}"#);
    let status = get_status(&control);
    assert_eq!(status["translate_enabled"], false);
    assert_eq!(status["speech_enabled"], false);
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("bridge-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["translate_enabled"], false);
    assert_eq!(state["speech_enabled"], false);

    // speech stop：关播报并清队列（Task 4 无投递方，清空数为 0）。
    let stop = call_ok(&control, ctl::BRIDGE_SPEECH_STOP, 4, b"{}");
    assert_eq!(stop["speech_enabled"], false);
    assert_eq!(stop["queue_cleared"], 0);

    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    drop(control);

    // 重启：TOML 基线仍为 true/true，状态文件覆盖优先。
    let mut child = spawn_bridge(&dir, "");
    let control = connect_control(&dir);
    let _conn = mock.accept();
    let status = get_status(&control);
    assert_eq!(status["translate_enabled"], false);
    assert_eq!(status["speech_enabled"], false);

    // 恢复后再开 translate：覆盖写回状态文件。
    call_ok(
        &control,
        ctl::BRIDGE_TRANSLATE_SET,
        5,
        br#"{"enabled":true}"#,
    );
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("bridge-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["translate_enabled"], true);
    assert_eq!(state["speech_enabled"], false);

    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn toml_baseline_applies_without_state_file() {
    let dir = run_dir("baseline");
    let mock = MockAsr::bind(&dir.join("asr-events.sock"));
    let mut child = spawn_bridge(
        &dir,
        "translate_enabled = false\nspeech_enabled = false\n",
    );
    let control = connect_control(&dir);
    let conn = mock.accept();

    let status = get_status(&control);
    assert_eq!(status["translate_enabled"], false);
    assert_eq!(status["speech_enabled"], false);
    assert!(!dir.join("bridge-state.json").exists());

    // translate off：final 只发布原文，不入待翻译队列。
    conn.send_event(&event("final", "s", 1, "你好"));
    wait_until("字幕落盘", Duration::from_secs(5), || {
        read_lines(&dir).len() == 1
    });
    assert_eq!(get_status(&control)["translate_queue_depth"], 0);

    // 运行时打开后，新 final 入队。
    call_ok(
        &control,
        ctl::BRIDGE_TRANSLATE_SET,
        2,
        br#"{"enabled":true}"#,
    );
    conn.send_event(&event("final", "s", 2, "世界"));
    wait_until("第二句落盘", Duration::from_secs(5), || {
        read_lines(&dir).len() == 2
    });
    let status = get_status(&control);
    assert_eq!(status["translate_queue_depth"], 1);
    assert_eq!(status["subtitles_published"], 2);

    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_requests_are_rejected_and_service_continues() {
    let dir = run_dir("invalid");
    let mock = MockAsr::bind(&dir.join("asr-events.sock"));
    let mut child = spawn_bridge(&dir, "");
    let control = connect_control(&dir);
    let _conn = mock.accept();

    let envelope = call(&control, ctl::ASR_HELLO, 2, b"");
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["code"], ctl::CODE_UNKNOWN_COMMAND);

    let envelope = call(&control, ctl::BRIDGE_TRANSLATE_SET, 3, b"not json");
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["code"], ctl::CODE_INVALID_ARGUMENT);

    let envelope = call(&control, ctl::BRIDGE_SPEECH_STOP, 4, br#"{"x":1}"#);
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["code"], ctl::CODE_INVALID_ARGUMENT);

    // 拒绝后仍正常服务，开关未被改动。
    let status = get_status(&control);
    assert_eq!(status["translate_enabled"], true);
    assert_eq!(status["speech_enabled"], true);

    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn asr_absent_degrades_then_recovers() {
    let dir = run_dir("absent");
    // 先不绑定事件面：桥应降级运行并可查询原因。
    let mut child = spawn_bridge(&dir, "");
    let control = connect_control(&dir);
    wait_until("断连降级可查", Duration::from_secs(5), || {
        get_status(&control)["degraded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().starts_with("asr events disconnected"))
    });

    // 识别服务随后上线：桥自动重连，降级解除。
    let mock = MockAsr::bind(&dir.join("asr-events.sock"));
    let conn = mock.accept();
    wait_until("重连降级解除", Duration::from_secs(5), || {
        get_status(&control)["degraded"] == json!([])
    });
    conn.send_event(&event("final", "late", 1, "迟到"));
    wait_until("重连后落盘", Duration::from_secs(5), || {
        read_lines(&dir).len() == 1
    });

    sigterm(&child.0);
    assert!(wait_exit(&mut child.0, Duration::from_secs(2)).success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_state_file_fails_startup() {
    let dir = run_dir("corrupt");
    std::fs::write(dir.join("bridge-state.json"), "{not json").unwrap();
    let mut child = spawn_bridge(&dir, "");
    let status = wait_exit(&mut child.0, Duration::from_secs(5));
    assert!(!status.success(), "损坏状态文件应启动失败");
    let _ = std::fs::remove_dir_all(&dir);
}