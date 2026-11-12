//! llama-server HTTP 客户端（SPEC-05 翻译）：Unix socket + 原生 HTTP/1.1，
//! POST /v1/chat/completions，max_tokens=128、temperature=0、关闭思考模式。
//! 每请求一连（Connection: close），不维护长连接；连通状态由调用方
//! 按请求结果记入共享状态（GetStatus 的 llama_connected 与最近成功失败）。
//! 收发用 poll 分步等待：停止标志置位即放弃在途请求（daemon 2 s 宽限），
//! 单请求总时限 REQUEST_TIMEOUT。

use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// 翻译系统提示词（旧实现原文，subtitle_bridge.py SYSTEM_PROMPT）。
pub const SYSTEM_PROMPT: &str = "Translate the Chinese utterance into English. \
Return only the translation. Preserve names, years, numbers, negation and uncertainty exactly. \
Use Arabic numerals for years and large quantities. \
Do not invent context, people, relationships or events. \
Keep direct address as direct address, not a possession or a family relationship.";

/// 单请求总时限（含收发）。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 连接时限：监听积压满时 connect 不无限阻塞。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// connect 重试步长：AF_UNIX 积压满时轮询重试的粒度（与 core Seqpacket 一致）。
const CONNECT_RETRY_STEP: Duration = Duration::from_millis(10);
/// poll 步长：停止标志与总时限的检查粒度。
const POLL_STEP: Duration = Duration::from_millis(100);
/// 响应体上限：翻译输出远小于此，超限按坏响应处理。
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// 一次推理结果：译文正文与结束原因（质检需要 finish_reason=length 判截断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub content: String,
    pub finish_reason: Option<String>,
}

pub struct LlamaClient {
    socket: PathBuf,
    timeout: Duration,
}

impl LlamaClient {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// 测试用短时限；生产路径一律走 new。
    #[cfg(test)]
    pub fn with_timeout(socket: PathBuf, timeout: Duration) -> Self {
        Self { socket, timeout }
    }

    /// 一次翻译请求。失败返回原因（连接、超时、HTTP 状态、响应解析）；
    /// 停止标志置位时尽快返回 Err，由调用方决定是否计数。
    pub fn translate(&self, text: &str, stop: &AtomicBool) -> Result<Completion, String> {
        let stream = connect_timeout(&self.socket, CONNECT_TIMEOUT)
            .map_err(|err| format!("connect {}：{err}", self.socket.display()))?;
        let raw = exchange(&stream, &build_request(text), self.timeout, stop)?;
        parse_response(&raw)
    }
}

/// 带超时的 Unix STREAM 连接：非阻塞 connect 重试到时限。
/// AF_UNIX 积压满时 connect 返回 EAGAIN 且无异步完成通知，须轮询重试
/// （与 core Seqpacket::connect_timeout 同法），不能用 poll 等待可写。
fn connect_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    // SAFETY: socket() 不访问其他内存；CLOEXEC|NONBLOCK 标志有效。
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw 是新建且未复制的文件描述符。
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let (addr, len) = sockaddr_un(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: fd 有效；addr 指向已零初始化的 sockaddr_un。
        let rc = unsafe {
            libc::connect(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len)
        };
        if rc == 0 {
            return into_blocking_stream(fd);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) | Some(libc::EINPROGRESS) | Some(libc::EAGAIN) => {}
            Some(libc::EISCONN) => return into_blocking_stream(fd),
            _ => return Err(err),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timeout"));
        }
        std::thread::sleep(CONNECT_RETRY_STEP.min(deadline - now));
    }
}

/// 连接成功后转回阻塞模式并把所有权移交 UnixStream。
fn into_blocking_stream(fd: OwnedFd) -> io::Result<UnixStream> {
    set_nonblocking(&fd, false)?;
    // SAFETY: fd 是已连接的 STREAM 套接字，所有权移交 UnixStream。
    Ok(unsafe { UnixStream::from_raw_fd(fd.into_raw_fd()) })
}

fn set_nonblocking(fd: &OwnedFd, nonblocking: bool) -> io::Result<()> {
    // SAFETY: fcntl 只读写该 fd 的标志位。
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let target = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // SAFETY: target 由上一次 F_GETFL 结果派生。
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, target) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sockaddr_un(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: sockaddr_un 可由全零位模式表示。
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long"));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

/// 构造请求报文：系统提示词 + 用户原文，参数固定。
pub fn build_request(text: &str) -> Vec<u8> {
    let body = serde_json::to_vec(&json!({
        "model": "default",
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": text }
        ],
        "max_tokens": 128,
        "temperature": 0,
        "chat_template_kwargs": { "enable_thinking": false }
    }))
    .expect("request body is JSON-serializable");
    let mut request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    request
}

/// 非阻塞收发，poll 分步等待；停止或超时即放弃。
fn exchange(
    mut stream: &UnixStream,
    request: &[u8],
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<Vec<u8>, String> {
    stream
        .set_nonblocking(true)
        .map_err(|err| format!("set_nonblocking：{err}"))?;
    let deadline = Instant::now() + timeout;

    let mut written = 0;
    while written < request.len() {
        check_wait(stop, deadline)?;
        match stream.write(&request[written..]) {
            Ok(0) => return Err("connection closed during write".to_string()),
            Ok(n) => written += n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                poll_wait(stream, libc::POLLOUT)?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("write：{err}")),
        }
    }

    let mut response = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        check_wait(stop, deadline)?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&chunk[..n]);
                if response.len() > MAX_RESPONSE_BYTES {
                    return Err("response exceeds 1 MiB limit".to_string());
                }
                if response_complete(&response)? {
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                poll_wait(stream, libc::POLLIN)?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("read：{err}")),
        }
    }
    Ok(response)
}

fn check_wait(stop: &AtomicBool, deadline: Instant) -> Result<(), String> {
    if stop.load(Relaxed) {
        return Err("aborted: stopping".to_string());
    }
    if Instant::now() >= deadline {
        return Err("request timeout".to_string());
    }
    Ok(())
}

/// 等待可读/可写一个步长；超时返回 Ok 由外层复查时限，POLLHUP 交给 read 收尾。
fn poll_wait(stream: &UnixStream, events: libc::c_short) -> Result<(), String> {
    let mut pollfd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events,
        revents: 0,
    };
    // SAFETY: pollfd 指向单个已初始化的 pollfd。
    let rc = unsafe { libc::poll(&mut pollfd, 1, POLL_STEP.as_millis() as libc::c_int) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(format!("poll：{err}"));
        }
    }
    Ok(())
}

/// 头部未收齐返回 false；收齐后按 Content-Length 判满，
/// 无长度信息时读到对端关闭（请求带 Connection: close）。
fn response_complete(response: &[u8]) -> Result<bool, String> {
    let Some(head_end) = find_subslice(response, b"\r\n\r\n") else {
        return Ok(false);
    };
    let head = std::str::from_utf8(&response[..head_end])
        .map_err(|_| "response head is not UTF-8".to_string())?;
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let len: usize = value
                .trim()
                .parse()
                .map_err(|_| format!("invalid Content-Length: {}", value.trim()))?;
            return Ok(response.len() >= head_end + 4 + len);
        }
    }
    Ok(false)
}

/// 解析响应：状态码、分帧（Content-Length/chunked/关闭）、choices[0] 的正文与结束原因。
pub fn parse_response(raw: &[u8]) -> Result<Completion, String> {
    let head_end = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response: no header terminator".to_string())?;
    let head = std::str::from_utf8(&raw[..head_end])
        .map_err(|_| "response head is not UTF-8".to_string())?;
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse().ok())
        .ok_or_else(|| format!("malformed status line: {status_line}"))?;
    let body = &raw[head_end + 4..];
    if code != 200 {
        let snippet = String::from_utf8_lossy(&body[..body.len().min(120)]);
        return Err(format!("HTTP {code}: {snippet}"));
    }
    let chunked = lines.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    });
    let body = if chunked {
        decode_chunks(body)?
    } else {
        body.to_vec()
    };
    let value: Value = serde_json::from_slice(&body)
        .map_err(|err| format!("invalid response JSON: {err}"))?;
    let choice = &value["choices"][0];
    let content = choice["message"]["content"]
        .as_str()
        .map(|content| content.to_string())
        .ok_or_else(|| "missing choices[0].message.content".to_string())?;
    let finish_reason = choice["finish_reason"].as_str().map(|reason| reason.to_string());
    Ok(Completion {
        content,
        finish_reason,
    })
}

/// chunked 解码：尺寸行（十六进制，忽略扩展）+ 数据块，0 块结束（忽略尾部）。
fn decode_chunks(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end =
            find_subslice(body, b"\r\n").ok_or_else(|| "bad chunk size line".to_string())?;
        let size_line = std::str::from_utf8(&body[..line_end])
            .map_err(|_| "chunk size line is not UTF-8".to_string())?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("invalid chunk size: {size_line}"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err("truncated chunk body".to_string());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{http_response, temp_dir, MockLlama};
    use std::sync::{mpsc, Arc};

    #[test]
    fn request_format_matches_contract() {
        let request = build_request("你好");
        let text = String::from_utf8(request.clone()).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(head.contains("Content-Type: application/json\r\n"));
        assert!(head.ends_with("Connection: close"));
        assert!(head.contains(&format!("Content-Length: {}\r\n", body.len())));
        let value: Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["model"], "default");
        assert_eq!(value["max_tokens"], 128);
        assert_eq!(value["temperature"], 0);
        assert_eq!(value["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(value["messages"][0]["role"], "system");
        // 旧实现原文逐字对拍，防止提示词被改写。
        assert_eq!(
            value["messages"][0]["content"],
            "Translate the Chinese utterance into English. Return only the translation. \
             Preserve names, years, numbers, negation and uncertainty exactly. \
             Use Arabic numerals for years and large quantities. \
             Do not invent context, people, relationships or events. \
             Keep direct address as direct address, not a possession or a family relationship."
        );
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "你好");
    }

    #[test]
    fn translate_success_over_unix_socket() {
        let dir = temp_dir("http-ok");
        let path = dir.join("llama.sock");
        let mock = MockLlama::bind(&path);
        let body = r#"{"choices":[{"message":{"content":"Hello."}}]}"#;
        let server = mock.spawn(vec![http_response(200, body)]);

        let client = LlamaClient::new(path);
        let stop = AtomicBool::new(false);
        assert_eq!(client.translate("你好", &stop).unwrap().content, "Hello.");

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        let text = String::from_utf8(requests[0].clone()).unwrap();
        assert!(text.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        let (_, body) = text.split_once("\r\n\r\n").unwrap();
        let value: Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["messages"][1]["content"], "你好");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunked_and_eof_framing_are_parsed() {
        let dir = temp_dir("http-framing");
        let path = dir.join("llama.sock");
        let mock = MockLlama::bind(&path);
        // chunked 尺寸按实际字节数生成，避免手算十六进制。
        let body = r#"{"choices":[{"message":{"content":"Hi there."}}]}"#;
        let split = 27;
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            split,
            &body[..split],
            body.len() - split,
            &body[split..]
        );
        // 第二路无 Content-Length：读到对端关闭为止。
        let eof = "HTTP/1.1 200 OK\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"Bye.\"}}]}";
        let server = mock.spawn(vec![chunked, eof.to_string()]);

        let client = LlamaClient::new(path);
        let stop = AtomicBool::new(false);
        assert_eq!(client.translate("甲", &stop).unwrap().content, "Hi there.");
        assert_eq!(client.translate("乙", &stop).unwrap().content, "Bye.");
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_error_status_is_reported() {
        let dir = temp_dir("http-500");
        let path = dir.join("llama.sock");
        let mock = MockLlama::bind(&path);
        let server = mock.spawn(vec![http_response(500, "{\"error\":\"busy\"}")]);
        let client = LlamaClient::new(path);
        let err = client.translate("你好", &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("HTTP 500"), "{err}");
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeout_and_stop_abort_are_prompt() {
        let dir = temp_dir("http-timeout");
        let path = dir.join("llama.sock");
        let mock = MockLlama::bind(&path);
        // 收下请求后不应答：验证总时限与停止放弃。
        let (stall_tx, stall_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let server = mock.spawn_stall(2, stall_tx, release_rx);

        let client = LlamaClient::with_timeout(path.clone(), Duration::from_millis(200));
        let started = Instant::now();
        let err = client.translate("你好", &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("timeout"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        stall_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // 停止标志：在途请求立即放弃，不等总时限。
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = stop.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            stopper.store(true, Relaxed);
        });
        let started = Instant::now();
        let err = client.translate("你好", &stop).unwrap_err();
        assert!(err.contains("stopping"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        timer.join().unwrap();
        drop(release_tx);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_failure_reports_reason() {
        let dir = temp_dir("http-absent");
        let client = LlamaClient::new(dir.join("missing.sock"));
        let err = client.translate("你好", &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("connect"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 监听积压满时 connect 不无限阻塞，按 CONNECT_TIMEOUT 返回 TimedOut（C4）。
    #[test]
    fn connect_timeout_fires_when_backlog_full() {
        let dir = temp_dir("http-connect-full");
        let path = dir.join("llama.sock");
        // SAFETY: socket() 不访问其他内存；CLOEXEC 标志有效。
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0, "socket: {}", io::Error::last_os_error());
        // SAFETY: raw 是新建且未复制的文件描述符。
        let listener = unsafe { OwnedFd::from_raw_fd(raw) };
        let (addr, len) = sockaddr_un(&path).unwrap();
        // SAFETY: listener 有效；addr 指向已初始化的 sockaddr_un。
        let rc =
            unsafe { libc::bind(listener.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) };
        assert_eq!(rc, 0, "bind: {}", io::Error::last_os_error());
        // SAFETY: listener 已 bind。
        let rc = unsafe { libc::listen(listener.as_raw_fd(), 1) };
        assert_eq!(rc, 0, "listen: {}", io::Error::last_os_error());

        // 非阻塞 connect 占满积压直到 EAGAIN（内核可能允许 backlog+1，逐条探测）。
        let mut fillers = Vec::new();
        loop {
            // SAFETY: socket() 不访问其他内存；NONBLOCK|CLOEXEC 标志有效。
            let raw = unsafe {
                libc::socket(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                )
            };
            assert!(raw >= 0);
            // SAFETY: raw 是新建且未复制的文件描述符。
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            // SAFETY: fd 有效；addr 指向已初始化的 sockaddr_un。
            let rc =
                unsafe { libc::connect(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) };
            if rc == 0 {
                fillers.push(fd);
                continue;
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EINPROGRESS) => break,
                _ => panic!("filler connect：{err}"),
            }
        }
        assert!(!fillers.is_empty(), "积压应已被占满");

        let started = Instant::now();
        let err = connect_timeout(&path, Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(150), "过早返回：{elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "超时未生效：{elapsed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sockaddr_un_rejects_overlong_path() {
        let long = PathBuf::from("/").join("x".repeat(200));
        let err = sockaddr_un(&long).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn parse_response_rejects_bad_bodies() {
        let cases: Vec<Vec<u8>> = vec![
            b"not http".to_vec(),
            b"HTTP/1.1\r\n\r\n{}".to_vec(),
            b"HTTP/1.1 200 OK\r\n\r\n{not json".to_vec(),
            b"HTTP/1.1 200 OK\r\n\r\n{\"choices\":[]}".to_vec(),
            b"HTTP/1.1 200 OK\r\n\r\n{\"choices\":[{\"message\":{}}]}".to_vec(),
        ];
        for raw in cases {
            assert!(parse_response(&raw).is_err(), "应拒绝：{raw:?}");
        }
        // Content-Length 非法。
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n").is_err());
        // chunked 坏帧。
        assert!(decode_chunks(b"zz\r\n").is_err());
        assert!(decode_chunks(b"5\r\nabc").is_err());
    }

    #[test]
    fn parse_response_reads_finish_reason() {
        let raw = b"HTTP/1.1 200 OK\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"Hello.\"},\"finish_reason\":\"length\"}]}";
        let completion = parse_response(raw).unwrap();
        assert_eq!(completion.content, "Hello.");
        assert_eq!(completion.finish_reason.as_deref(), Some("length"));
        // 缺 finish_reason 时为 None（不触发截断判定）。
        let raw = b"HTTP/1.1 200 OK\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"Hi.\"}}]}";
        assert_eq!(parse_response(raw).unwrap().finish_reason, None);
    }

    #[test]
    fn response_complete_waits_for_content_length() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        let mut raw = head.to_vec();
        assert!(!response_complete(&raw).unwrap());
        raw.extend_from_slice(b"1234");
        assert!(!response_complete(&raw).unwrap());
        raw.extend_from_slice(b"5");
        assert!(response_complete(&raw).unwrap());
        // 无长度信息：等待对端关闭。
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\n\r\n{}").unwrap());
    }

    #[test]
    fn oversized_response_is_rejected() {
        let dir = temp_dir("http-big");
        let path = dir.join("llama.sock");
        let mock = MockLlama::bind(&path);
        let big = "x".repeat(MAX_RESPONSE_BYTES + 1);
        let server = mock.spawn(vec![http_response(200, &big)]);
        let client = LlamaClient::new(path);
        let err = client.translate("你好", &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("1 MiB"), "{err}");
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}