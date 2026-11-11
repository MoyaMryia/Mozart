//! 控制面信封与 SEQPACKET 传输，对应 core/include/mozart/ctl.h。
//! 修改 C 头后必须同步本文件；布局由编译期断言保证。

use std::fmt;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

pub const MAGIC: u32 = 0x4C54_434D;
pub const VERSION: u16 = 1;
pub const HEADER_BYTES: usize = 16;
pub const MAX_BODY_BYTES: usize = 65_536;

pub const DAEMON_HELLO: u16 = 0x0101;
pub const DAEMON_GET_STATUS: u16 = 0x0102;
pub const DAEMON_ROUTE_SET: u16 = 0x0103;
pub const DAEMON_MUTE_SET: u16 = 0x0104;
pub const DAEMON_SHUTDOWN: u16 = 0x0105;

pub const PRE_HELLO: u16 = 0x0201;
pub const PRE_SET_ROUTE: u16 = 0x0202;
pub const PRE_SET_MUTE: u16 = 0x0203;
pub const PRE_GET_STATUS: u16 = 0x0204;

pub const RVC_HELLO: u16 = 0x0301;
pub const RVC_LOAD_MODEL: u16 = 0x0302;
pub const RVC_START_STREAM: u16 = 0x0303;
pub const RVC_STOP_STREAM: u16 = 0x0304;
pub const RVC_RESET_STREAM: u16 = 0x0305;
pub const RVC_GET_STATUS: u16 = 0x0306;

pub const CODE_OK: &str = "Ok";
pub const CODE_INVALID_ARGUMENT: &str = "InvalidArgument";
pub const CODE_NOT_LOADED: &str = "NotLoaded";
pub const CODE_NOT_READY: &str = "NotReady";
pub const CODE_BUSY: &str = "Busy";
pub const CODE_UNSUPPORTED: &str = "Unsupported";
pub const CODE_INFERENCE_FAILED: &str = "InferenceFailed";
pub const CODE_UNKNOWN_COMMAND: &str = "UnknownCommand";
pub const CODE_INTERNAL: &str = "Internal";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    ShortFrame,
    BadMagic,
    BadVersion,
    BodyTooLarge,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ProtocolError::ShortFrame => "control frame shorter than header",
            ProtocolError::BadMagic => "control frame magic mismatch",
            ProtocolError::BadVersion => "control protocol version mismatch",
            ProtocolError::BodyTooLarge => "control frame body exceeds limit",
        };
        f.write_str(text)
    }
}

impl std::error::Error for ProtocolError {}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub magic: u32,
    pub version: u16,
    pub type_: u16,
    pub request_id: u32,
    pub body_len: u32,
}

const _: () = assert!(mem::size_of::<Header>() == HEADER_BYTES);
const _: () = assert!(mem::offset_of!(Header, magic) == 0);
const _: () = assert!(mem::offset_of!(Header, version) == 4);
const _: () = assert!(mem::offset_of!(Header, type_) == 6);
const _: () = assert!(mem::offset_of!(Header, request_id) == 8);
const _: () = assert!(mem::offset_of!(Header, body_len) == 12);

impl Header {
    pub fn new(type_: u16, request_id: u32, body_len: u32) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            type_,
            request_id,
            body_len,
        }
    }

    pub fn to_bytes(self) -> [u8; HEADER_BYTES] {
        let mut out = [0u8; HEADER_BYTES];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..6].copy_from_slice(&self.version.to_le_bytes());
        out[6..8].copy_from_slice(&self.type_.to_le_bytes());
        out[8..12].copy_from_slice(&self.request_id.to_le_bytes());
        out[12..16].copy_from_slice(&self.body_len.to_le_bytes());
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, ProtocolError> {
        let bytes: &[u8; HEADER_BYTES] = buf
            .get(..HEADER_BYTES)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(ProtocolError::ShortFrame)?;
        let header = Self {
            magic: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            version: u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
            type_: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            request_id: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            body_len: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        };
        if header.magic != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        if header.body_len as usize > MAX_BODY_BYTES {
            return Err(ProtocolError::BodyTooLarge);
        }
        Ok(header)
    }

    pub fn ensure_version(self) -> Result<Self, ProtocolError> {
        if self.version != VERSION {
            return Err(ProtocolError::BadVersion);
        }
        Ok(self)
    }
}

#[derive(Debug)]
pub struct Message {
    pub header: Header,
    pub body: Vec<u8>,
}

pub struct Seqpacket {
    fd: OwnedFd,
}

impl Seqpacket {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let fd = new_seqpacket()?;
        let (addr, len) = sockaddr(path)?;
        // SAFETY: fd 有效；addr 指向已零初始化的 sockaddr_un。
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_un).cast(),
                len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    pub fn connect(path: &Path) -> io::Result<Self> {
        let fd = new_seqpacket()?;
        let (addr, len) = sockaddr(path)?;
        // SAFETY: fd 有效；addr 指向已零初始化的 sockaddr_un。
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_un).cast(),
                len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    pub fn listen(&self, backlog: libc::c_int) -> io::Result<()> {
        // SAFETY: fd 是已 bind 的 SEQPACKET 套接字。
        let rc = unsafe { libc::listen(self.fd.as_raw_fd(), backlog) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn accept(&self) -> io::Result<Self> {
        loop {
            // SAFETY: fd 处于监听状态；地址参数为空表示不关心对端地址。
            let raw = unsafe {
                libc::accept4(
                    self.fd.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if raw < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            // SAFETY: raw 是 accept4 新建且未复制的文件描述符。
            return Ok(Self {
                fd: unsafe { OwnedFd::from_raw_fd(raw) },
            });
        }
    }

    pub fn send(&self, type_: u16, request_id: u32, body: &[u8]) -> io::Result<()> {
        if body.len() > MAX_BODY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control body exceeds MOZART_CTL_MAX_BODY_BYTES",
            ));
        }
        let mut packet = Vec::with_capacity(HEADER_BYTES + body.len());
        packet.extend_from_slice(&Header::new(type_, request_id, body.len() as u32).to_bytes());
        packet.extend_from_slice(body);
        // SAFETY: packet 缓冲区有效；MSG_NOSIGNAL 防止对端关闭时触发 SIGPIPE。
        let sent = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if sent as usize != packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short SEQPACKET send",
            ));
        }
        Ok(())
    }

    pub fn recv(&self, timeout: Option<Duration>) -> io::Result<Message> {
        self.wait_readable(timeout)?;
        let mut buf = vec![0u8; HEADER_BYTES + MAX_BODY_BYTES + 1];
        loop {
            // SAFETY: buf 有效且长度与传入一致。
            let n = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    0,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            let n = n as usize;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "control peer closed",
                ));
            }
            if n > HEADER_BYTES + MAX_BODY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control frame exceeds maximum size",
                ));
            }
            let header = Header::from_bytes(&buf[..n])
                .and_then(Header::ensure_version)
                .map_err(protocol_io)?;
            return Ok(Message {
                header,
                body: buf[HEADER_BYTES..n].to_vec(),
            });
        }
    }

    fn wait_readable(&self, timeout: Option<Duration>) -> io::Result<()> {
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = match timeout {
            None => -1,
            Some(d) => d.as_millis().min(libc::c_int::MAX as u128) as libc::c_int,
        };
        loop {
            // SAFETY: pollfd 指向单个已初始化的 pollfd。
            let rc = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if rc == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "control socket recv timeout",
                ));
            }
            return Ok(());
        }
    }
}

fn new_seqpacket() -> io::Result<OwnedFd> {
    // SAFETY: socket() 不访问其他内存。CLOEXEC 防止套接字泄漏给子进程。
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
            "control socket path too long",
        ));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

fn protocol_io(err: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socketpair() -> (Seqpacket, Seqpacket) {
        let mut fds = [0; 2];
        // SAFETY: fds 指向可写数组；AF_UNIX + SOCK_SEQPACKET 由内核支持。
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair: {}", io::Error::last_os_error());
        // SAFETY: fds 内是新建且未复制的文件描述符。
        unsafe {
            (
                Seqpacket {
                    fd: OwnedFd::from_raw_fd(fds[0]),
                },
                Seqpacket {
                    fd: OwnedFd::from_raw_fd(fds[1]),
                },
            )
        }
    }

    #[test]
    fn header_bytes_match_c_layout() {
        let bytes = Header::new(DAEMON_HELLO, 0x0102_0304, 5).to_bytes();
        assert_eq!(&bytes[0..4], b"MCTL");
        assert_eq!(&bytes[4..6], &VERSION.to_le_bytes());
        assert_eq!(&bytes[6..8], &DAEMON_HELLO.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x0102_0304u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &5u32.to_le_bytes());
    }

    #[test]
    fn header_decode_rejects_bad_frames() {
        assert_eq!(Header::from_bytes(&[0u8; 15]), Err(ProtocolError::ShortFrame));

        let mut bytes = Header::new(PRE_HELLO, 1, 0).to_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(Header::from_bytes(&bytes), Err(ProtocolError::BadMagic));

        let mut bytes = Header::new(PRE_HELLO, 1, 0).to_bytes();
        bytes[12..16].copy_from_slice(&(MAX_BODY_BYTES as u32 + 1).to_le_bytes());
        assert_eq!(Header::from_bytes(&bytes), Err(ProtocolError::BodyTooLarge));

        let mut bytes = Header::new(PRE_HELLO, 1, 0).to_bytes();
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        let header = Header::from_bytes(&bytes).unwrap();
        assert_eq!(header.ensure_version(), Err(ProtocolError::BadVersion));
    }

    #[test]
    fn seqpacket_roundtrip_preserves_boundaries() {
        let (a, b) = socketpair();
        a.send(DAEMON_GET_STATUS, 42, br#"{"x":1}"#).unwrap();
        a.send(DAEMON_MUTE_SET, 43, b"").unwrap();

        let first = b.recv(Some(Duration::from_secs(1))).unwrap();
        let first_type = first.header.type_;
        let first_request = first.header.request_id;
        assert_eq!(first_type, DAEMON_GET_STATUS);
        assert_eq!(first_request, 42);
        assert_eq!(first.body, br#"{"x":1}"#);

        let second = b.recv(Some(Duration::from_secs(1))).unwrap();
        let second_type = second.header.type_;
        assert_eq!(second_type, DAEMON_MUTE_SET);
        assert!(second.body.is_empty());
    }

    #[test]
    fn seqpacket_recv_timeout() {
        let (_a, b) = socketpair();
        let err = b.recv(Some(Duration::from_millis(10))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn send_rejects_oversized_body() {
        let (a, _b) = socketpair();
        let body = vec![0u8; MAX_BODY_BYTES + 1];
        let err = a.send(DAEMON_HELLO, 1, &body).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn listen_accept_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mozart-ctl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("listen.sock");
        let _ = std::fs::remove_file(&path);

        let listener = Seqpacket::bind(&path).unwrap();
        listener.listen(1).unwrap();

        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let conn = Seqpacket::connect(&client_path).unwrap();
            conn.send(PRE_GET_STATUS, 9, br#"{"probe":true}"#).unwrap();
        });

        let conn = listener.accept().unwrap();
        let msg = conn.recv(Some(Duration::from_secs(1))).unwrap();
        let msg_type = msg.header.type_;
        let request_id = msg.header.request_id;
        assert_eq!(msg_type, PRE_GET_STATUS);
        assert_eq!(request_id, 9);
        client.join().unwrap();

        drop(conn);
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }
}
