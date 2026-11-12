//! 输出线程（SPEC-05）：独占 asr-events.sock SEQPACKET 监听（桥连接、识别服务监听），
//! 一条消息一个 JSON 事件（裸 JSON，不带控制信封）。解码/复核线程只交付事件。
//! 断连时 final/refined 存有界队列、重连按序补发，partial 直接丢；超限丢最旧并计数。
//! 事件面协议区别于控制面信封，故自带最小 SEQPACKET 收发（复用 core 的 sockaddr 约定）。

use crate::event::{Event, PARTIAL};
use crate::state::Shared;
use std::collections::VecDeque;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 事件循环节拍：收事件阻塞步长，兼作 accept 轮询间隔。
const POLL: Duration = Duration::from_millis(50);
/// 单条事件发送等待上限：桥读循环只入队，正常不阻塞；超时按断连处理。
const SEND_TIMEOUT: Duration = Duration::from_millis(200);

/// SEQPACKET 事件监听 socket。
pub struct EventListener {
    fd: OwnedFd,
}

impl EventListener {
    /// 绑定并监听；清理同名陈旧文件（进程重启无需外部清理）。
    pub fn bind(path: &Path) -> io::Result<Self> {
        let fd = new_seqpacket()?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let (addr, len) = sockaddr(path)?;
        // SAFETY: fd 有效；addr 指向已零初始化的 sockaddr_un。
        let rc =
            unsafe { libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd 已绑定；backlog 1 足够（桥单连接）。
        let rc = unsafe { libc::listen(fd.as_raw_fd(), 1) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// 非阻塞 accept：无连接返回 None。
    pub fn try_accept(&self) -> io::Result<Option<EventConn>> {
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pollfd 指向单个已初始化结构；超时 0 立即返回。
        let rc = unsafe { libc::poll(&mut pollfd, 1, 0) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if rc == 0 {
            return Ok(None);
        }
        // SAFETY: fd 处于监听状态；不关心对端地址。
        let raw = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw 是 accept4 新建且未复制的文件描述符。
        Ok(Some(EventConn {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        }))
    }
}

/// 一条已接受的桥连接。
pub struct EventConn {
    fd: OwnedFd,
}

impl EventConn {
    /// 发送一条完整 JSON 事件。缓冲满时等待至超时；对端关闭/超时返回 Err。
    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + SEND_TIMEOUT;
        loop {
            // SAFETY: bytes 有效；MSG_NOSIGNAL 防 SIGPIPE，MSG_DONTWAIT 缓冲满即返回。
            let sent = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if sent >= 0 {
                if sent as usize == bytes.len() {
                    return Ok(());
                }
                return Err(io::Error::new(io::ErrorKind::WriteZero, "短事件发送"));
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "事件发送超时"));
                    }
                    wait_writable(self.fd.as_raw_fd(), remaining)?;
                }
                _ => return Err(err),
            }
        }
    }
}

fn wait_writable(fd: libc::c_int, timeout: Duration) -> io::Result<()> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    // SAFETY: pollfd 指向单个已初始化结构。
    let rc = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if rc == 0 {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "事件发送等待超时"));
    }
    Ok(())
}

/// 断连重连缓冲：final/refined 入队，partial 丢；超限丢最旧并计数。
pub struct OutputBuffer {
    queue: VecDeque<Event>,
    capacity: usize,
    dropped: u64,
}

impl OutputBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            capacity,
            dropped: 0,
        }
    }

    /// 入队一条事件。partial 直接丢（不计数）；final/refined 超限丢最旧。
    pub fn push(&mut self, event: Event) {
        if event.kind == PARTIAL {
            return;
        }
        if self.queue.len() >= self.capacity {
            self.queue.pop_front();
            self.dropped += 1;
        }
        self.queue.push_back(event);
    }

    pub fn front(&self) -> Option<&Event> {
        self.queue.front()
    }
    pub fn pop_front(&mut self) -> Option<Event> {
        self.queue.pop_front()
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// 输出循环。事件通道所有发送方（解码/复核）退出后收到 Disconnected，
/// 尽力补发缓冲后退出。
pub fn run(listener: EventListener, rx: Receiver<Event>, shared: Arc<Shared>, capacity: usize) {
    let mut conn: Option<EventConn> = None;
    let mut buffer = OutputBuffer::new(capacity);
    let mut disconnected = false;
    loop {
        if !disconnected {
            match rx.recv_timeout(POLL) {
                Ok(event) => deliver(event, &mut conn, &mut buffer, &shared),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => disconnected = true,
            }
            while let Ok(event) = rx.try_recv() {
                deliver(event, &mut conn, &mut buffer, &shared);
            }
        }
        if conn.is_none() {
            if let Ok(Some(c)) = listener.try_accept() {
                conn = Some(c);
                shared.set_bridge_connected(true);
            }
        }
        // 按序补发缓冲；发送失败转断连。
        if let Some(c) = conn.as_ref() {
            while let Some(event) = buffer.front() {
                match send_event(c, event) {
                    Ok(()) => {
                        buffer.pop_front();
                    }
                    Err(_) => {
                        conn = None;
                        shared.set_bridge_connected(false);
                        break;
                    }
                }
            }
        }
        shared.set_event_buffer_dropped(buffer.dropped());
        if disconnected && (buffer.is_empty() || conn.is_none()) {
            break;
        }
    }
}

/// 交付一条事件：连接可用且缓冲为空时直发；否则入缓冲（保序，partial 丢）。
fn deliver(
    event: Event,
    conn: &mut Option<EventConn>,
    buffer: &mut OutputBuffer,
    shared: &Shared,
) {
    if let Some(c) = conn.as_ref() {
        if buffer.is_empty() {
            match send_event(c, &event) {
                Ok(()) => return,
                Err(_) => {
                    *conn = None;
                    shared.set_bridge_connected(false);
                }
            }
        }
    }
    buffer.push(event);
}

fn send_event(conn: &EventConn, event: &Event) -> io::Result<()> {
    let bytes = serde_json::to_vec(event)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    conn.send(&bytes)
}

fn new_seqpacket() -> io::Result<OwnedFd> {
    // SAFETY: socket() 不访问其他内存；CLOEXEC 防泄漏给子进程。
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw 是刚创建且未复制的文件描述符。
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn sockaddr(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: sockaddr_un 可由全零位模式表示。
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "事件 socket 路径过长",
        ));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, FINAL, PARTIAL, REFINED};

    fn event(kind: &'static str, seq: u64) -> Event {
        let mut e = Event::partial("s", seq, "x".to_string(), None, None);
        e.kind = kind;
        e
    }

    #[test]
    fn buffer_drops_partial_silently() {
        let mut buffer = OutputBuffer::new(2);
        buffer.push(event(PARTIAL, 1));
        assert!(buffer.is_empty());
        assert_eq!(buffer.dropped(), 0);
    }

    #[test]
    fn buffer_drop_oldest_over_capacity() {
        let mut buffer = OutputBuffer::new(2);
        buffer.push(event(FINAL, 1));
        buffer.push(event(REFINED, 2));
        buffer.push(event(FINAL, 3));
        assert_eq!(buffer.dropped(), 1);
        // 丢最旧：保留 2、3。
        assert_eq!(buffer.pop_front().unwrap().seq, 2);
        assert_eq!(buffer.pop_front().unwrap().seq, 3);
        assert!(buffer.is_empty());
    }

    #[test]
    fn buffer_keeps_order_within_capacity() {
        let mut buffer = OutputBuffer::new(4);
        for seq in 1..=4 {
            buffer.push(event(FINAL, seq));
        }
        assert_eq!(buffer.dropped(), 0);
        for seq in 1..=4 {
            assert_eq!(buffer.pop_front().unwrap().seq, seq);
        }
    }
}