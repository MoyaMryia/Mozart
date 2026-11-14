//! 控制面信封与 SEQPACKET 传输，对应 core/include/mozart/ctl.h。
//! 修改 C 头后必须同步本文件；布局由编译期断言保证。

pub mod schema;

use std::fmt;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// connect 非阻塞重试的间隔。
const RETRY_STEP: Duration = Duration::from_millis(10);

pub const MAGIC: u32 = 0x4C54_434D;
pub const VERSION: u16 = 1;
pub const HEADER_BYTES: usize = 16;
pub const MAX_BODY_BYTES: usize = 65_536;

pub const DAEMON_HELLO: u16 = 0x0101;
pub const DAEMON_GET_STATUS: u16 = 0x0102;
pub const DAEMON_ROUTE_SET: u16 = 0x0103;
pub const DAEMON_MUTE_SET: u16 = 0x0104;
pub const DAEMON_SHUTDOWN: u16 = 0x0105;
/// 体：{"enabled": bool}；daemon 转发到桥 BRIDGE_TRANSLATE_SET。
pub const DAEMON_TRANSLATE_SET: u16 = 0x0106;
/// 体：{"enabled": bool}；daemon 转发到桥 BRIDGE_SPEECH_SET。
pub const DAEMON_SPEECH_SET: u16 = 0x0107;
/// 体：{}（空）；daemon 转发到桥 BRIDGE_SPEECH_STOP。
pub const DAEMON_SPEECH_STOP: u16 = 0x0108;
/// 文件链路（SPEC-04）：体 schema::FileSubmitRequest {"path","model_id"?}；
/// data {"job_id","state","queue_position"}。
pub const DAEMON_FILE_SUBMIT: u16 = 0x0110;
/// 体：{}（空）；data {"paused":bool}。暂停后不再启动新任务，在跑任务不受影响。
pub const DAEMON_FILE_PAUSE: u16 = 0x0111;
/// 体：{}（空）；data {"paused":bool}。恢复启动排队任务。
pub const DAEMON_FILE_RESUME: u16 = 0x0112;
/// 体：schema::FileCancelRequest {"job_id"}；data {"job_id","state"}。
pub const DAEMON_FILE_CANCEL: u16 = 0x0113;
/// 体：schema::FileFetchRequest {"job_id","output"?}；data {"job_id","output_path","name"}。
/// v1 同机：daemon 不复制，回受管结果绝对路径与缺省名（源文件名主干 + .wav），
/// CLI 复制到 output（缺省当前目录下缺省名）；结果不可取（未完成/已驱逐）拒绝 NotReady。
pub const DAEMON_FILE_FETCH: u16 = 0x0114;
/// 体：schema::FileStatusRequest {"job_id"}；data 为 schema::FileJob。
pub const DAEMON_FILE_STATUS: u16 = 0x0115;
/// 体：{}（空）；data {"jobs":[schema::FileJob],"paused":bool}。
pub const DAEMON_FILE_LIST: u16 = 0x0116;
/// 体：{}（空）；data {"models":[schema::ModelEntry]}。
pub const DAEMON_MODEL_LIST: u16 = 0x0117;
/// 体：schema::RestartRequest {"service"}。
pub const DAEMON_RESTART: u16 = 0x0118;
/// 体：{}（空）；data {"devices":[schema::DeviceEntry]}。
pub const DAEMON_DEVICE_LIST: u16 = 0x0119;
/// 体：schema::ConfigSetRequest {"key","value"}；
/// data {"key","value","restart_required","affected"}（SPEC-04 重启提示契约）。
pub const DAEMON_CONFIG_SET: u16 = 0x011A;
/// 体：schema::ConfigGetRequest {"key"?}；data {"value"}（缺省 key 回整份基线）。
pub const DAEMON_CONFIG_GET: u16 = 0x011B;

pub const PRE_HELLO: u16 = 0x0201;
pub const PRE_SET_ROUTE: u16 = 0x0202;
pub const PRE_SET_MUTE: u16 = 0x0203;
pub const PRE_GET_STATUS: u16 = 0x0204;
/// 体：{}（空）；清播报环。speech stop 经 daemon 转发（桥→TTS 走 TTS_JOB_STOP）。
pub const PRE_SPEECH_STOP: u16 = 0x0205;
/// 体：schema::FileModeSetRequest {"active": bool}；文件任务接管实时数据面
/// （SPEC-03 复用 + 状态门控交接）。active=true：pre 暂停 rvc AI 发送并释放
/// output_socket，应答前确认释放完成；active=false：恢复发送并重绑。asr 不受影响。
pub const PRE_SET_FILE_MODE: u16 = 0x0206;

pub const RVC_HELLO: u16 = 0x0301;
pub const RVC_LOAD_MODEL: u16 = 0x0302;
pub const RVC_START_STREAM: u16 = 0x0303;
pub const RVC_STOP_STREAM: u16 = 0x0304;
pub const RVC_RESET_STREAM: u16 = 0x0305;
pub const RVC_GET_STATUS: u16 = 0x0306;
/// 体：{}（空）；data {"models":[schema::ModelEntry]}。
pub const RVC_LIST_MODELS: u16 = 0x0307;

pub const ASR_HELLO: u16 = 0x0401;
pub const ASR_GET_STATUS: u16 = 0x0402;

pub const BRIDGE_HELLO: u16 = 0x0501;
pub const BRIDGE_GET_STATUS: u16 = 0x0502;
/// 体：{"enabled": bool}。
pub const BRIDGE_TRANSLATE_SET: u16 = 0x0503;
/// 体：{"enabled": bool}。
pub const BRIDGE_SPEECH_SET: u16 = 0x0504;
/// 体：{}（空）。
pub const BRIDGE_SPEECH_STOP: u16 = 0x0505;

pub const TTS_HELLO: u16 = 0x0601;
pub const TTS_GET_STATUS: u16 = 0x0602;
/// 任务面（tts-jobs.sock，桥连接）：体 {"text","utterance_id","voice_params"?}。
pub const TTS_JOB_SUBMIT: u16 = 0x0610;
/// 任务面受理应答：体 {"status","utterance_id","reason"?}，status 取 submitted/skipped/failed。
pub const TTS_JOB_REPLY: u16 = 0x0611;
/// 任务面停止：体 {}（空）；TTS 清空任务队列并中止在途合成，应答体 {"stopped":true}。
pub const TTS_JOB_STOP: u16 = 0x0612;

/// 控制连接空闲断开，各进程控制面同一纪律（SPEC-04）。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// recv 轮询步长：让停止标志在秒级内生效，再按累计空闲判断断开。
pub const RECV_POLL: Duration = Duration::from_secs(1);
/// accept 轮询间隔：无连接时周期检查停止标志，保证 SIGTERM 可退出。
pub const ACCEPT_POLL: Duration = Duration::from_millis(100);

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

#[derive(Debug)]
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
        Self::connect_timeout(path, None)
    }

    /// 监听积压满时 connect 会无限阻塞；带超时版本以非阻塞重试实现。
    pub fn connect_timeout(path: &Path, timeout: Option<Duration>) -> io::Result<Self> {
        let fd = new_seqpacket()?;
        set_nonblocking(&fd, true)?;
        let (addr, len) = sockaddr(path)?;
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            // SAFETY: fd 有效；addr 指向已零初始化的 sockaddr_un。
            let rc = unsafe {
                libc::connect(
                    fd.as_raw_fd(),
                    (&addr as *const libc::sockaddr_un).cast(),
                    len,
                )
            };
            if rc == 0 {
                set_nonblocking(&fd, false)?;
                return Ok(Self { fd });
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) | Some(libc::EINPROGRESS) | Some(libc::EAGAIN) => {}
                Some(libc::EISCONN) => {
                    set_nonblocking(&fd, false)?;
                    return Ok(Self { fd });
                }
                _ => return Err(err),
            }
            match deadline {
                None => std::thread::sleep(RETRY_STEP),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "control socket connect timeout",
                        ));
                    }
                    std::thread::sleep(RETRY_STEP.min(deadline - now));
                }
            }
        }
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
        self.send_timeout(type_, request_id, body, None)
    }

    /// 对端缓冲满时 send 会无限阻塞；带超时版本以 MSG_DONTWAIT + poll 实现。
    pub fn send_timeout(
        &self,
        type_: u16,
        request_id: u32,
        body: &[u8],
        timeout: Option<Duration>,
    ) -> io::Result<()> {
        if body.len() > MAX_BODY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control body exceeds MOZART_CTL_MAX_BODY_BYTES",
            ));
        }
        let mut packet = Vec::with_capacity(HEADER_BYTES + body.len());
        packet.extend_from_slice(&Header::new(type_, request_id, body.len() as u32).to_bytes());
        packet.extend_from_slice(body);
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            // SAFETY: packet 缓冲区有效；MSG_NOSIGNAL 防止对端关闭时触发 SIGPIPE，
            // MSG_DONTWAIT 让缓冲满时立即返回而不是阻塞。
            let sent = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    packet.as_ptr().cast(),
                    packet.len(),
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if sent >= 0 {
                if sent as usize == packet.len() {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short SEQPACKET send",
                ));
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    let wait = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                    self.wait_writable(wait)?;
                }
                _ => return Err(err),
            }
        }
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
            let body_len = header.body_len as usize;
            if body_len != n - HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control body length mismatch",
                ));
            }
            return Ok(Message {
                header,
                body: buf[HEADER_BYTES..n].to_vec(),
            });
        }
    }

    /// 裸 SEQPACKET 收包（事件面：一条消息一个 JSON，无控制信封）。
    /// 超时返回 TimedOut，对端关闭返回 UnexpectedEof，超上限消息按坏帧处理。
    pub fn recv_raw(&self, timeout: Option<Duration>) -> io::Result<Vec<u8>> {
        self.wait_readable(timeout)?;
        let mut buf = vec![0u8; MAX_BODY_BYTES + 1];
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
                    "event peer closed",
                ));
            }
            if n > MAX_BODY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "event message exceeds maximum size",
                ));
            }
            buf.truncate(n);
            return Ok(buf);
        }
    }

    pub fn wait_readable(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.wait_ready(libc::POLLIN, timeout)
    }

    fn wait_writable(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.wait_ready(libc::POLLOUT, timeout)
    }

    fn wait_ready(&self, events: libc::c_short, timeout: Option<Duration>) -> io::Result<()> {
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events,
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
    fn language_chain_command_numbers_match_c_header() {
        assert_eq!(DAEMON_TRANSLATE_SET, 0x0106);
        assert_eq!(DAEMON_SPEECH_SET, 0x0107);
        assert_eq!(DAEMON_SPEECH_STOP, 0x0108);
        assert_eq!(PRE_SPEECH_STOP, 0x0205);
        assert_eq!(ASR_HELLO, 0x0401);
        assert_eq!(ASR_GET_STATUS, 0x0402);
        assert_eq!(BRIDGE_HELLO, 0x0501);
        assert_eq!(BRIDGE_GET_STATUS, 0x0502);
        assert_eq!(BRIDGE_TRANSLATE_SET, 0x0503);
        assert_eq!(BRIDGE_SPEECH_SET, 0x0504);
        assert_eq!(BRIDGE_SPEECH_STOP, 0x0505);
        assert_eq!(TTS_HELLO, 0x0601);
        assert_eq!(TTS_GET_STATUS, 0x0602);
        assert_eq!(TTS_JOB_SUBMIT, 0x0610);
        assert_eq!(TTS_JOB_REPLY, 0x0611);
        assert_eq!(TTS_JOB_STOP, 0x0612);
    }

    #[test]
    fn file_chain_command_numbers_match_c_header() {
        assert_eq!(DAEMON_FILE_SUBMIT, 0x0110);
        assert_eq!(DAEMON_FILE_PAUSE, 0x0111);
        assert_eq!(DAEMON_FILE_RESUME, 0x0112);
        assert_eq!(DAEMON_FILE_CANCEL, 0x0113);
        assert_eq!(DAEMON_FILE_FETCH, 0x0114);
        assert_eq!(DAEMON_FILE_STATUS, 0x0115);
        assert_eq!(DAEMON_FILE_LIST, 0x0116);
        assert_eq!(DAEMON_MODEL_LIST, 0x0117);
        assert_eq!(DAEMON_RESTART, 0x0118);
        assert_eq!(DAEMON_DEVICE_LIST, 0x0119);
        assert_eq!(DAEMON_CONFIG_SET, 0x011A);
        assert_eq!(DAEMON_CONFIG_GET, 0x011B);
        assert_eq!(RVC_LIST_MODELS, 0x0307);
        assert_eq!(PRE_SET_FILE_MODE, 0x0206);
    }

    /// 全部命令号清单：唯一性与分段检查用，新增命令必须登记。
    const ALL_COMMANDS: &[(&str, u16)] = &[
        ("DAEMON_HELLO", DAEMON_HELLO),
        ("DAEMON_GET_STATUS", DAEMON_GET_STATUS),
        ("DAEMON_ROUTE_SET", DAEMON_ROUTE_SET),
        ("DAEMON_MUTE_SET", DAEMON_MUTE_SET),
        ("DAEMON_SHUTDOWN", DAEMON_SHUTDOWN),
        ("DAEMON_TRANSLATE_SET", DAEMON_TRANSLATE_SET),
        ("DAEMON_SPEECH_SET", DAEMON_SPEECH_SET),
        ("DAEMON_SPEECH_STOP", DAEMON_SPEECH_STOP),
        ("DAEMON_FILE_SUBMIT", DAEMON_FILE_SUBMIT),
        ("DAEMON_FILE_PAUSE", DAEMON_FILE_PAUSE),
        ("DAEMON_FILE_RESUME", DAEMON_FILE_RESUME),
        ("DAEMON_FILE_CANCEL", DAEMON_FILE_CANCEL),
        ("DAEMON_FILE_FETCH", DAEMON_FILE_FETCH),
        ("DAEMON_FILE_STATUS", DAEMON_FILE_STATUS),
        ("DAEMON_FILE_LIST", DAEMON_FILE_LIST),
        ("DAEMON_MODEL_LIST", DAEMON_MODEL_LIST),
        ("DAEMON_RESTART", DAEMON_RESTART),
        ("DAEMON_DEVICE_LIST", DAEMON_DEVICE_LIST),
        ("DAEMON_CONFIG_SET", DAEMON_CONFIG_SET),
        ("DAEMON_CONFIG_GET", DAEMON_CONFIG_GET),
        ("PRE_HELLO", PRE_HELLO),
        ("PRE_SET_ROUTE", PRE_SET_ROUTE),
        ("PRE_SET_MUTE", PRE_SET_MUTE),
        ("PRE_GET_STATUS", PRE_GET_STATUS),
        ("PRE_SPEECH_STOP", PRE_SPEECH_STOP),
        ("PRE_SET_FILE_MODE", PRE_SET_FILE_MODE),
        ("RVC_HELLO", RVC_HELLO),
        ("RVC_LOAD_MODEL", RVC_LOAD_MODEL),
        ("RVC_START_STREAM", RVC_START_STREAM),
        ("RVC_STOP_STREAM", RVC_STOP_STREAM),
        ("RVC_RESET_STREAM", RVC_RESET_STREAM),
        ("RVC_GET_STATUS", RVC_GET_STATUS),
        ("RVC_LIST_MODELS", RVC_LIST_MODELS),
        ("ASR_HELLO", ASR_HELLO),
        ("ASR_GET_STATUS", ASR_GET_STATUS),
        ("BRIDGE_HELLO", BRIDGE_HELLO),
        ("BRIDGE_GET_STATUS", BRIDGE_GET_STATUS),
        ("BRIDGE_TRANSLATE_SET", BRIDGE_TRANSLATE_SET),
        ("BRIDGE_SPEECH_SET", BRIDGE_SPEECH_SET),
        ("BRIDGE_SPEECH_STOP", BRIDGE_SPEECH_STOP),
        ("TTS_HELLO", TTS_HELLO),
        ("TTS_GET_STATUS", TTS_GET_STATUS),
        ("TTS_JOB_SUBMIT", TTS_JOB_SUBMIT),
        ("TTS_JOB_REPLY", TTS_JOB_REPLY),
        ("TTS_JOB_STOP", TTS_JOB_STOP),
    ];

    #[test]
    fn command_numbers_unique_and_segmented() {
        for (i, (name, value)) in ALL_COMMANDS.iter().enumerate() {
            for (other_name, other) in &ALL_COMMANDS[i + 1..] {
                assert_ne!(value, other, "{name} 与 {other_name} 命令号重复");
            }
            // 命令号按平面分段：高字节是平面号（ctl.h 注释）。
            let plane = match name.split('_').next().unwrap() {
                "DAEMON" => 0x01u16,
                "PRE" => 0x02,
                "RVC" => 0x03,
                "ASR" => 0x04,
                "BRIDGE" => 0x05,
                "TTS" => 0x06,
                other => panic!("未知命令平面：{other}"),
            };
            assert_eq!(value >> 8, plane, "{name} 不在 0x{plane:02x}xx 段");
        }
    }

    #[test]
    fn bridge_switch_commands_roundtrip() {
        let (a, b) = socketpair();
        a.send(BRIDGE_TRANSLATE_SET, 7, br#"{"enabled":true}"#)
            .unwrap();
        a.send(BRIDGE_SPEECH_STOP, 8, b"").unwrap();

        let first = b.recv(Some(Duration::from_secs(1))).unwrap();
        let first_type = first.header.type_;
        let first_request = first.header.request_id;
        assert_eq!(first_type, BRIDGE_TRANSLATE_SET);
        assert_eq!(first_request, 7);
        assert_eq!(first.body, br#"{"enabled":true}"#);

        let second = b.recv(Some(Duration::from_secs(1))).unwrap();
        let second_type = second.header.type_;
        assert_eq!(second_type, BRIDGE_SPEECH_STOP);
        assert!(second.body.is_empty());
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
    fn recv_raw_preserves_boundaries_without_envelope() {
        let (a, b) = socketpair();
        // SAFETY: a 是测试内创建的 SEQPACKET 套接字；裸 JSON 不带控制信封。
        let first = br#"{"type":"partial"}"#;
        let sent = unsafe {
            libc::send(
                a.fd.as_raw_fd(),
                first.as_ptr().cast(),
                first.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        assert_eq!(sent as usize, first.len());
        let second = br#"{"type":"final"}"#;
        let sent = unsafe {
            libc::send(
                a.fd.as_raw_fd(),
                second.as_ptr().cast(),
                second.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        assert_eq!(sent as usize, second.len());

        assert_eq!(b.recv_raw(Some(Duration::from_secs(1))).unwrap(), first);
        assert_eq!(b.recv_raw(Some(Duration::from_secs(1))).unwrap(), second);
    }

    #[test]
    fn recv_raw_timeout_and_peer_close() {
        let (a, b) = socketpair();
        let err = b.recv_raw(Some(Duration::from_millis(10))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(a);
        let err = b.recv_raw(Some(Duration::from_secs(1))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn send_rejects_oversized_body() {
        let (a, _b) = socketpair();
        let body = vec![0u8; MAX_BODY_BYTES + 1];
        let err = a.send(DAEMON_HELLO, 1, &body).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn recv_rejects_body_length_mismatch() {
        let (a, b) = socketpair();
        let mut packet = Header::new(PRE_GET_STATUS, 3, 5).to_bytes().to_vec();
        packet.extend_from_slice(b"{}");
        // SAFETY: packet 缓冲区有效；a 是测试内创建的 SEQPACKET 套接字。
        let sent = unsafe {
            libc::send(
                a.fd.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        assert_eq!(sent as usize, packet.len());
        let err = b.recv(Some(Duration::from_secs(1))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn connect_timeout_bounds_backlog_wait() {
        let dir = std::env::temp_dir().join(format!("mozart-ctl-connect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("listen.sock");
        let _ = std::fs::remove_file(&path);

        let listener = Seqpacket::bind(&path).unwrap();
        listener.listen(1).unwrap();
        // 内核对积压的比较是严格大于：backlog=1 实际容许两条未 accept 连接。
        let first = Seqpacket::connect(&path).unwrap();
        let second = Seqpacket::connect(&path).unwrap();

        let started = Instant::now();
        let err =
            Seqpacket::connect_timeout(&path, Some(Duration::from_millis(120))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // poll 毫秒超时允许内核略提前醒来，留 5 ms 容差。
        assert!(started.elapsed() >= Duration::from_millis(115));

        // accept 释放积压后，重试方在超时内接通。
        let retry_path = path.clone();
        let retry = std::thread::spawn(move || {
            Seqpacket::connect_timeout(&retry_path, Some(Duration::from_secs(2)))
        });
        std::thread::sleep(Duration::from_millis(50));
        let accepted_first = listener.accept().unwrap();
        retry.join().unwrap().unwrap();
        let accepted_second = listener.accept().unwrap();

        drop(accepted_first);
        drop(accepted_second);
        drop(first);
        drop(second);
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn send_timeout_when_peer_stalls() {
        let (a, _b) = socketpair();
        // SAFETY: setsockopt 只作用于测试内创建的套接字。Unix 套接字的流控
        // 记在发送方 sndbuf 上：内核按 2×8192 生效，两条 8 KiB 消息必然放不下第二条。
        let sndbuf: libc::c_int = 8192;
        let rc = unsafe {
            libc::setsockopt(
                a.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&sndbuf as *const libc::c_int).cast(),
                mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0);

        let body = vec![0u8; 8192];
        a.send_timeout(PRE_HELLO, 1, &body, Some(Duration::from_secs(1)))
            .unwrap();
        let started = Instant::now();
        let err = a
            .send_timeout(PRE_HELLO, 2, &body, Some(Duration::from_millis(120)))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // poll 毫秒超时允许内核略提前醒来，留 5 ms 容差。
        assert!(started.elapsed() >= Duration::from_millis(115));
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
