//! 进程级集成测试共用助手：spawn mozart-pre（ALSA null 设备，无真实声学）、
//! 控制面调用与子进程守卫。各用例自带配置文本与独立运行目录。

use mozart_core::ctl::{self, Seqpacket};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// 子进程守卫：断言失败也不留孤儿进程。
pub struct ChildGuard(pub Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 用例独立运行目录（pid + 序号防并行碰撞）。
pub fn run_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mozart-pre-it-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 写配置并拉起 pre；采集/播放用 ALSA null 设备（无硬件依赖）。
pub fn spawn_pre(dir: &Path, config_text: &str) -> ChildGuard {
    let config = dir.join("mozart.toml");
    std::fs::write(&config, config_text).unwrap();
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

pub fn wait_until(what: &str, timeout: Duration, pred: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !pred() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 控制面与播报输出 socket 就绪（pre 绑定 tts-out.sock）。
pub fn wait_sockets(dir: &Path) {
    wait_until("socket 就绪", Duration::from_secs(5), || {
        dir.join("pre.sock").exists() && dir.join("tts-out.sock").exists()
    });
}

pub fn call_ok(conn: &Seqpacket, type_: u16, request_id: u32, body: &[u8]) -> Value {
    conn.send(type_, request_id, body).unwrap();
    let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
    let reply_type = reply.header.type_;
    let reply_request = reply.header.request_id;
    assert_eq!(reply_type, type_);
    assert_eq!(reply_request, request_id);
    let envelope: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(envelope["status"], "ok", "控制应答：{envelope}");
    envelope["data"].clone()
}

pub fn get_status(conn: &Seqpacket) -> Value {
    call_ok(conn, ctl::PRE_GET_STATUS, 1, b"")
}

pub fn sigterm(child: &Child) {
    // SAFETY: kill 只发信号；pid 来自本用例 spawn 的子进程。
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "SIGTERM: {}", std::io::Error::last_os_error());
}

pub fn wait_exit(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "SIGTERM 后未在时限内退出");
        std::thread::sleep(Duration::from_millis(20));
    }
}