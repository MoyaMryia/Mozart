//! 单元测试共用夹具：临时目录、mock llama-server（Unix socket HTTP）、
//! mock TTS 任务面（SEQPACKET 信封）。进程级集成测试另见 tests/process.rs。

use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// 用例独立临时目录（pid + 线程 + 序号防并行碰撞）。
pub fn temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mozart-bridge-unit-{tag}-{}-{:?}-{}",
        std::process::id(),
        std::thread::current().id(),
        SEQ.fetch_add(1, Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 带 Content-Length 的 HTTP 响应报文。
pub fn http_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        500 => "Internal Server Error",
        _ => "Status",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// 读完整 HTTP 请求：头部 + Content-Length 指定的体。
pub fn read_http_request(stream: &mut UnixStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).expect("读请求");
        assert!(n > 0, "对端提前关闭");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let len: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().expect("Content-Length 是数字"))
        })
        .expect("请求带 Content-Length");
    while buf.len() < head_end + 4 + len {
        let n = stream.read(&mut chunk).expect("读请求体");
        assert!(n > 0, "对端提前关闭");
        buf.extend_from_slice(&chunk[..n]);
    }
    buf
}

/// mock llama-server：按连接顺序应答脚本化响应。
pub struct MockLlama {
    listener: UnixListener,
}

impl MockLlama {
    pub fn bind(path: &Path) -> Self {
        let _ = std::fs::remove_file(path);
        Self {
            listener: UnixListener::bind(path).expect("绑定 mock llama"),
        }
    }

    /// 每个连接读一条请求、回一条响应；返回收到的请求列表。
    pub fn spawn(self, responses: Vec<String>) -> JoinHandle<Vec<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = self.listener.accept().expect("accept");
                requests.push(read_http_request(&mut stream));
                stream.write_all(response.as_bytes()).expect("写响应");
                stream.flush().expect("flush");
            }
            requests
        })
    }

    /// 收下 count 条请求但不应答（超时/停止测试）；每收一条发一个信号。
    /// 连接保持在手里不关闭，直到 release 通道关闭/收到消息，避免客户端提前读到 EOF。
    pub fn spawn_stall(
        self,
        count: usize,
        received: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for _ in 0..count {
                let (mut stream, _) = self.listener.accept().expect("accept");
                let _ = read_http_request(&mut stream);
                let _ = received.send(());
                held.push(stream);
            }
            let _ = release.recv();
        })
    }
}

/// mock TTS 任务面：绑定 tts-jobs.sock，按脚本回受理状态。
pub struct MockTts {
    listener: Seqpacket,
}

impl MockTts {
    pub fn bind(path: &Path) -> Self {
        let _ = std::fs::remove_file(path);
        let listener = Seqpacket::bind(path).expect("绑定 mock tts");
        listener.listen(4).expect("listen");
        Self { listener }
    }

    /// 限时等待桥连接；无连接返回 None（验证未发起投递）。
    pub fn wait_connection(&self, timeout: Duration) -> Option<Seqpacket> {
        match self.listener.wait_readable(Some(timeout)) {
            Ok(()) => Some(self.listener.accept().expect("accept")),
            Err(_) => None,
        }
    }

    /// 服务一条连接：按序应答 statuses，记录请求体；应答完后保持连接，
    /// 直到对端关闭（进程退出或工作线程停止）。
    pub fn spawn(self, statuses: Vec<&'static str>) -> (Arc<Mutex<Vec<Value>>>, JoinHandle<()>) {
        self.spawn_inner(statuses, true)
    }

    /// 同 spawn，但应答完即关闭连接（模拟 TTS 重启）。
    pub fn spawn_closing(
        self,
        statuses: Vec<&'static str>,
    ) -> (Arc<Mutex<Vec<Value>>>, JoinHandle<()>) {
        self.spawn_inner(statuses, false)
    }

    fn spawn_inner(
        self,
        statuses: Vec<&'static str>,
        keep_alive: bool,
    ) -> (Arc<Mutex<Vec<Value>>>, JoinHandle<()>) {
        let jobs = Arc::new(Mutex::new(Vec::new()));
        let recorded = jobs.clone();
        let handle = std::thread::spawn(move || {
            let conn = self.listener.accept().expect("accept");
            for status in statuses {
                let msg = conn.recv(Some(Duration::from_secs(5))).expect("收任务");
                let msg_type = msg.header.type_;
                let request_id = msg.header.request_id;
                assert_eq!(msg_type, ctl::TTS_JOB_SUBMIT);
                let body: Value = serde_json::from_slice(&msg.body).expect("任务体是 JSON");
                let utterance_id = body["utterance_id"].as_str().expect("utterance_id");
                let reply = json!({ "status": status, "utterance_id": utterance_id });
                recorded.lock().unwrap().push(body);
                conn.send(ctl::TTS_JOB_REPLY, request_id, &serde_json::to_vec(&reply).unwrap())
                    .expect("回受理");
            }
            if keep_alive {
                // 保持连接：空闲不断开，对端关闭才退出。
                loop {
                    match conn.recv(Some(Duration::from_millis(200))) {
                        Ok(_) => {}
                        Err(err) if err.kind() == io::ErrorKind::TimedOut => continue,
                        Err(_) => break,
                    }
                }
            }
        });
        (jobs, handle)
    }
}