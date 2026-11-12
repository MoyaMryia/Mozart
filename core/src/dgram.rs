//! 数据面帧传输：Unix `SOCK_DGRAM`，一帧一条线包。
//! 线包布局对应 frame.h：4 B 魔数（u32 小端）+ 16 B meta（小端）+ f32 PCM（小端）；
//! 输入线包 1300 B，输出线包 3860 B。
//! 实时线程约束：send/recv 热路径只使用栈缓冲，不做堆分配。

use crate::{FrameMeta, InputFrame, OutputFrame, PACKET_HEADER_SIZE};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// 线包格式：帧类型与其魔数、总长和编解码的对应。
pub trait WirePacket: Copy {
    /// 魔数，取 frame.h 的 `MOZART_INPUT_PACKET_MAGIC` / `MOZART_OUTPUT_PACKET_MAGIC`。
    const MAGIC: u32;
    /// 线包总长：4 B 魔数 + 16 B meta + PCM。
    const PACKET_BYTES: usize;

    /// 编码为线包；`out` 长度必须等于 `PACKET_BYTES`。
    fn encode(&self, out: &mut [u8]);
    /// 从线包解码；`bytes` 长度必须等于 `PACKET_BYTES`。
    /// 魔数与长度由接收方在解码前校验。
    fn decode(bytes: &[u8]) -> Self;
}

/// send/recv 栈缓冲上限：两种线包取较大者（输出 3860 B）。
/// 实际收发长度按各帧类型的 `PACKET_BYTES` 截取。
const MAX_PACKET_BYTES: usize = <OutputFrame as WirePacket>::PACKET_BYTES;

const _: () = assert!(<InputFrame as WirePacket>::PACKET_BYTES == mem::size_of::<InputFrame>() + 4);
const _: () =
    assert!(<OutputFrame as WirePacket>::PACKET_BYTES == mem::size_of::<OutputFrame>() + 4);

impl WirePacket for InputFrame {
    const MAGIC: u32 = crate::INPUT_PACKET_MAGIC;
    const PACKET_BYTES: usize = PACKET_HEADER_SIZE + crate::INPUT_SAMPLES * 4;

    fn encode(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&Self::MAGIC.to_le_bytes());
        // packed 字段先按值复制，再取引用，避免非对齐引用。
        let meta = self.meta;
        encode_meta(&meta, out);
        let pcm = self.pcm;
        encode_pcm(&pcm, out);
    }

    fn decode(bytes: &[u8]) -> Self {
        Self {
            meta: decode_meta(bytes),
            pcm: decode_pcm(bytes),
        }
    }
}

impl WirePacket for OutputFrame {
    const MAGIC: u32 = crate::OUTPUT_PACKET_MAGIC;
    const PACKET_BYTES: usize = PACKET_HEADER_SIZE + crate::OUTPUT_SAMPLES * 4;

    fn encode(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&Self::MAGIC.to_le_bytes());
        let meta = self.meta;
        encode_meta(&meta, out);
        let pcm = self.pcm;
        encode_pcm(&pcm, out);
    }

    fn decode(bytes: &[u8]) -> Self {
        Self {
            meta: decode_meta(bytes),
            pcm: decode_pcm(bytes),
        }
    }
}

/// meta 编码到线包偏移 4..20，全部小端。
fn encode_meta(meta: &FrameMeta, out: &mut [u8]) {
    out[4..12].copy_from_slice(&meta.pts_ns.to_le_bytes());
    out[12..16].copy_from_slice(&meta.frame_idx.to_le_bytes());
    out[16] = meta.vad_flag;
    out[17] = meta.energy_db;
    out[18] = meta.conf;
    out[19] = meta.segment_id;
}

/// 从线包偏移 4..20 解码 meta，全部小端。
fn decode_meta(bytes: &[u8]) -> FrameMeta {
    FrameMeta {
        pts_ns: u64::from_le_bytes(bytes[4..12].try_into().unwrap()),
        frame_idx: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        vad_flag: bytes[16],
        energy_db: bytes[17],
        conf: bytes[18],
        segment_id: bytes[19],
    }
}

/// PCM 编码到线包偏移 20 之后，逐样本小端。
fn encode_pcm<const N: usize>(pcm: &[f32; N], out: &mut [u8]) {
    for (slot, sample) in out[PACKET_HEADER_SIZE..].chunks_exact_mut(4).zip(pcm) {
        slot.copy_from_slice(&sample.to_le_bytes());
    }
}

/// 从线包偏移 20 之后解码 PCM，逐样本小端。
fn decode_pcm<const N: usize>(bytes: &[u8]) -> [f32; N] {
    let mut pcm = [0f32; N];
    for (slot, chunk) in pcm.iter_mut().zip(bytes[PACKET_HEADER_SIZE..].chunks_exact(4)) {
        *slot = f32::from_le_bytes(chunk.try_into().unwrap());
    }
    pcm
}

/// 丢弃计数：发送方过载丢弃与接收方坏包丢弃。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DgramStats {
    pub send_dropped: u64,
    pub recv_dropped: u64,
}

/// connect 方记录的对端地址，供对端重启后重连。
struct PeerAddr {
    addr: libc::sockaddr_un,
    len: libc::socklen_t,
}

/// Unix `SOCK_DGRAM` 帧传输。bind 方收帧，connect 方发帧；一帧一条线包。
/// 方向约定（SPEC-02）：rvc 绑定 `input_socket` 收输入帧，pre 绑定 `output_socket` 收输出帧。
pub struct FrameSocket<F: WirePacket> {
    fd: OwnedFd,
    peer: Option<PeerAddr>,
    send_dropped: AtomicU64,
    recv_dropped: AtomicU64,
    frame: std::marker::PhantomData<F>,
}

/// 输入帧 socket：rvc bind 收输入帧；pre connect 发输入帧。
pub type InputSocket = FrameSocket<InputFrame>;
/// 输出帧 socket：pre bind 收输出帧；rvc connect 发输出帧。
pub type OutputSocket = FrameSocket<OutputFrame>;

impl<F: WirePacket> FrameSocket<F> {
    /// 服务方绑定 socket 文件并设置接收缓冲。
    /// `frames` 为接收缓冲帧数（SPEC-02 数据面上限 4–8 帧），
    /// 换算为 `SO_RCVBUF = frames × PACKET_BYTES`。
    /// Linux AF_UNIX 数据报流控不看接收方该值（依据见 connect），
    /// 该设置在 Linux 上不约束发送方，仅保留。
    /// 绑定前清理同名陈旧 socket 文件，进程重启后无需外部清理。
    pub fn bind(path: &Path, frames: usize) -> io::Result<Self> {
        let fd = new_dgram()?;
        set_socket_buffer(&fd, libc::SO_RCVBUF, frames * F::PACKET_BYTES)?;
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
        Ok(Self::with_fd(fd, None))
    }

    /// 连接单一对端并设置发送缓冲为 `frames × PACKET_BYTES`。
    /// Linux 对 Unix 数据报的流控按队列长度（`net.unix.max_dgram_qlen`）与
    /// 发送方 `SO_SNDBUF` 记账，接收方 `SO_RCVBUF` 不约束发送方；
    /// 本调用把对端排队上限夹在约 `frames` 帧，是 SPEC-02「发送方
    /// `SO_SNDBUF` 执行 4–8 帧上限」的实际执行点。对端重启后由 `send`
    /// 在失败时自动重连，无需调用方介入。
    pub fn connect(path: &Path, frames: usize) -> io::Result<Self> {
        let fd = new_dgram()?;
        set_socket_buffer(&fd, libc::SO_SNDBUF, frames * F::PACKET_BYTES)?;
        let (addr, len) = sockaddr(path)?;
        // SAFETY: fd 有效；addr 指向已零初始化的 sockaddr_un。
        let rc = unsafe {
            libc::connect(fd.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self::with_fd(fd, Some(PeerAddr { addr, len })))
    }

    fn with_fd(fd: OwnedFd, peer: Option<PeerAddr>) -> Self {
        Self {
            fd,
            peer,
            send_dropped: AtomicU64::new(0),
            recv_dropped: AtomicU64::new(0),
            frame: std::marker::PhantomData,
        }
    }

    /// 非阻塞发送一条完整线包。过载（EAGAIN/ENOBUFS）与对端不在
    /// （ENOENT/ECONNREFUSED/ENOTCONN）一律丢弃并计数，不阻塞不重试；
    /// 对端不在时顺带重连，只恢复后续帧的通道，当前帧不再补发。
    /// bind 方没有对端，调用 send 只会丢弃计数。
    /// 其余错误向上返回。
    pub fn send(&self, frame: &F) -> io::Result<()> {
        let mut buf = [0u8; MAX_PACKET_BYTES];
        let packet = &mut buf[..F::PACKET_BYTES];
        frame.encode(packet);
        loop {
            // SAFETY: packet 有效且长度与传入一致；数据报全有或全无。
            // MSG_DONTWAIT 让缓冲满时立即返回，MSG_NOSIGNAL 防对端关闭触发 SIGPIPE。
            let rc = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    packet.as_ptr().cast(),
                    packet.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if rc >= 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) | Some(libc::ENOBUFS) => {
                    self.send_dropped.fetch_add(1, Relaxed);
                    return Ok(());
                }
                Some(libc::ENOENT) | Some(libc::ECONNREFUSED) | Some(libc::ENOTCONN) => {
                    self.send_dropped.fetch_add(1, Relaxed);
                    self.try_reconnect();
                    return Ok(());
                }
                _ => return Err(err),
            }
        }
    }

    /// 尽力重连：失败不阻塞、不补发，留待下一次 send 再试。
    fn try_reconnect(&self) {
        let Some(peer) = &self.peer else {
            return;
        };
        // SAFETY: fd 有效；peer.addr 在 connect 时构造，长度已校验。
        let _ = unsafe {
            libc::connect(
                self.fd.as_raw_fd(),
                (&peer.addr as *const libc::sockaddr_un).cast(),
                peer.len,
            )
        };
    }

    /// 接收一帧。`None` 阻塞等待，`Some(t)` 超时后返回 TimedOut。
    /// 长度或魔数不符的包（短包、超长包、坏魔数）计数丢弃后继续收，
    /// 等待预算以本次调用时刻为终点，坏包不清零预算。
    pub fn recv(&self, timeout: Option<Duration>) -> io::Result<F> {
        let deadline = timeout.map(|t| Instant::now() + t);
        // 接收缓冲按线包精确尺寸 + 1：超长包被截断为 PACKET_BYTES + 1 后按长度不符丢弃。
        let mut buf = [0u8; MAX_PACKET_BYTES + 1];
        loop {
            loop {
                // SAFETY: buf 有效；长度按帧类型截取为 PACKET_BYTES + 1；
                // MSG_DONTWAIT 使队列空时返回 EAGAIN。
                let n = unsafe {
                    libc::recv(
                        self.fd.as_raw_fd(),
                        buf.as_mut_ptr().cast(),
                        F::PACKET_BYTES + 1,
                        libc::MSG_DONTWAIT,
                    )
                };
                if n >= 0 {
                    let n = n as usize;
                    if n == F::PACKET_BYTES && buf[0..4] == F::MAGIC.to_le_bytes() {
                        return Ok(F::decode(&buf[..F::PACKET_BYTES]));
                    }
                    self.recv_dropped.fetch_add(1, Relaxed);
                    continue;
                }
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => break,
                    _ => return Err(err),
                }
            }
            let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            self.wait_readable(remaining)?;
        }
    }

    /// 丢弃计数快照，供状态上报。
    pub fn stats(&self) -> DgramStats {
        DgramStats {
            send_dropped: self.send_dropped.load(Relaxed),
            recv_dropped: self.recv_dropped.load(Relaxed),
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
                    "frame socket recv timeout",
                ));
            }
            return Ok(());
        }
    }
}

fn new_dgram() -> io::Result<OwnedFd> {
    // SAFETY: socket() 不访问其他内存。CLOEXEC 防止套接字泄漏给子进程。
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw 是刚创建且未复制的文件描述符。
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn set_socket_buffer(fd: &OwnedFd, option: libc::c_int, bytes: usize) -> io::Result<()> {
    let value = bytes as libc::c_int;
    // SAFETY: setsockopt 只读取 value。
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&value as *const libc::c_int).cast(),
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
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
            "frame socket path too long",
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
    use std::os::fd::FromRawFd;
    use std::path::PathBuf;

    /// 独立目录存放本组用例的 socket 文件；bind 会清理陈旧文件，用后手动删除。
    fn socket_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mozart-dgram-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{tag}.sock"))
    }

    fn cleanup(path: &Path) {
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    fn sample_input(idx: u32) -> InputFrame {
        let mut pcm = [0f32; crate::INPUT_SAMPLES];
        for (i, slot) in pcm.iter_mut().enumerate() {
            *slot = i as f32 * 0.5 + idx as f32;
        }
        InputFrame {
            meta: FrameMeta {
                pts_ns: 0x0102_0304_0506_0708 + idx as u64,
                frame_idx: idx,
                vad_flag: 1,
                energy_db: 2,
                conf: 3,
                segment_id: 4,
            },
            pcm,
        }
    }

    fn sample_output(idx: u32) -> OutputFrame {
        let mut pcm = [0f32; crate::OUTPUT_SAMPLES];
        for (i, slot) in pcm.iter_mut().enumerate() {
            *slot = i as f32 * 0.25 + idx as f32;
        }
        OutputFrame {
            meta: FrameMeta {
                pts_ns: 0x0102_0304_0506_0708 + idx as u64,
                frame_idx: idx,
                vad_flag: 1,
                energy_db: 2,
                conf: 3,
                segment_id: 4,
            },
            pcm,
        }
    }

    /// meta 比较按值传参；packed 字段只能经解构按值读取，不能取引用。
    fn assert_same_meta(actual: FrameMeta, expect: FrameMeta) {
        let FrameMeta {
            pts_ns,
            frame_idx,
            vad_flag,
            energy_db,
            conf,
            segment_id,
        } = actual;
        let FrameMeta {
            pts_ns: expect_pts_ns,
            frame_idx: expect_frame_idx,
            vad_flag: expect_vad_flag,
            energy_db: expect_energy_db,
            conf: expect_conf,
            segment_id: expect_segment_id,
        } = expect;
        assert_eq!(pts_ns, expect_pts_ns);
        assert_eq!(frame_idx, expect_frame_idx);
        assert_eq!(vad_flag, expect_vad_flag);
        assert_eq!(energy_db, expect_energy_db);
        assert_eq!(conf, expect_conf);
        assert_eq!(segment_id, expect_segment_id);
    }

    /// PCM 比较按值传参，避免引用 packed 字段。
    fn assert_same_pcm<const N: usize>(actual: [f32; N], expect: [f32; N]) {
        assert_eq!(actual, expect);
    }

    #[test]
    fn input_packet_bytes_are_little_endian() {
        let mut pcm = [0f32; crate::INPUT_SAMPLES];
        pcm[0] = -1.5;
        pcm[crate::INPUT_SAMPLES - 1] = 0.25;
        let frame = InputFrame {
            meta: FrameMeta {
                pts_ns: 0x8877_6655_4433_2211,
                frame_idx: 0xD4D3_D2D1,
                vad_flag: 1,
                energy_db: 2,
                conf: 3,
                segment_id: 4,
            },
            pcm,
        };
        let mut packet = [0u8; <InputFrame as WirePacket>::PACKET_BYTES];
        frame.encode(&mut packet);
        assert_eq!(packet.len(), 1300);
        // 魔数 0x4D5A5254 小端。
        assert_eq!(&packet[0..4], &[0x54, 0x52, 0x5A, 0x4D]);
        // pts_ns 小端。
        assert_eq!(
            &packet[4..12],
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
        // frame_idx 小端。
        assert_eq!(&packet[12..16], &[0xD1, 0xD2, 0xD3, 0xD4]);
        assert_eq!(&packet[16..20], &[1, 2, 3, 4]);
        // pcm[0] = -1.5f32 小端。
        assert_eq!(&packet[20..24], &[0x00, 0x00, 0xC0, 0xBF]);
        // 末样本 pcm[319] = 0.25f32 小端。
        assert_eq!(&packet[1296..1300], &[0x00, 0x00, 0x80, 0x3E]);
    }

    #[test]
    fn output_packet_bytes_are_little_endian() {
        let mut pcm = [0f32; crate::OUTPUT_SAMPLES];
        pcm[0] = 1.0;
        pcm[crate::OUTPUT_SAMPLES - 1] = -0.5;
        let frame = OutputFrame {
            meta: FrameMeta {
                pts_ns: 0x8877_6655_4433_2211,
                frame_idx: 0xD4D3_D2D1,
                vad_flag: 1,
                energy_db: 2,
                conf: 3,
                segment_id: 4,
            },
            pcm,
        };
        let mut packet = [0u8; <OutputFrame as WirePacket>::PACKET_BYTES];
        frame.encode(&mut packet);
        assert_eq!(packet.len(), 3860);
        // 魔数 0x54525A4D 小端。
        assert_eq!(&packet[0..4], &[0x4D, 0x5A, 0x52, 0x54]);
        assert_eq!(&packet[4..12], &0x8877_6655_4433_2211u64.to_le_bytes());
        assert_eq!(&packet[12..16], &0xD4D3_D2D1u32.to_le_bytes());
        assert_eq!(&packet[16..20], &[1, 2, 3, 4]);
        // pcm[0] = 1.0f32 小端。
        assert_eq!(&packet[20..24], &[0x00, 0x00, 0x80, 0x3F]);
        // 末样本 pcm[959] = -0.5f32 小端。
        assert_eq!(&packet[3856..3860], &[0x00, 0x00, 0x00, 0xBF]);
    }

    #[test]
    fn decode_reads_hand_crafted_packet() {
        // 手工构造线包，验证解码不依赖编码路径。
        let mut packet = [0u8; <InputFrame as WirePacket>::PACKET_BYTES];
        packet[0..4].copy_from_slice(&crate::INPUT_PACKET_MAGIC.to_le_bytes());
        packet[4..12].copy_from_slice(&0x8877_6655_4433_2211u64.to_le_bytes());
        packet[12..16].copy_from_slice(&0xD4D3_D2D1u32.to_le_bytes());
        packet[16..20].copy_from_slice(&[1, 2, 3, 4]);
        packet[20..24].copy_from_slice(&(-1.5f32).to_le_bytes());
        let frame = <InputFrame as WirePacket>::decode(&packet);
        // packed 字段经解构按值读取。
        let FrameMeta {
            pts_ns,
            frame_idx,
            vad_flag,
            energy_db,
            conf,
            segment_id,
        } = frame.meta;
        let pcm = frame.pcm;
        assert_eq!(pts_ns, 0x8877_6655_4433_2211);
        assert_eq!(frame_idx, 0xD4D3_D2D1);
        assert_eq!(vad_flag, 1);
        assert_eq!(energy_db, 2);
        assert_eq!(conf, 3);
        assert_eq!(segment_id, 4);
        assert_eq!(pcm[0], -1.5);
    }

    #[test]
    fn packet_roundtrip_preserves_frame() {
        let input = sample_input(9);
        let mut packet = [0u8; <InputFrame as WirePacket>::PACKET_BYTES];
        input.encode(&mut packet);
        let back = <InputFrame as WirePacket>::decode(&packet);
        assert_same_meta(back.meta, input.meta);
        assert_same_pcm(back.pcm, input.pcm);

        let output = sample_output(9);
        let mut packet = [0u8; <OutputFrame as WirePacket>::PACKET_BYTES];
        output.encode(&mut packet);
        let back = <OutputFrame as WirePacket>::decode(&packet);
        assert_same_meta(back.meta, output.meta);
        assert_same_pcm(back.pcm, output.pcm);
    }

    #[test]
    fn bind_connect_roundtrip_input_frame() {
        let path = socket_path("in");
        let rx = FrameSocket::<InputFrame>::bind(&path, 8).unwrap();
        let tx = FrameSocket::<InputFrame>::connect(&path, 8).unwrap();
        let frame = sample_input(3);
        tx.send(&frame).unwrap();
        let got = rx.recv(None).unwrap();
        assert_same_meta(got.meta, frame.meta);
        assert_same_pcm(got.pcm, frame.pcm);
        assert_eq!(tx.stats(), DgramStats::default());
        assert_eq!(rx.stats(), DgramStats::default());
        cleanup(&path);
    }

    #[test]
    fn bind_connect_roundtrip_output_frame() {
        let path = socket_path("out");
        let rx = FrameSocket::<OutputFrame>::bind(&path, 8).unwrap();
        let tx = FrameSocket::<OutputFrame>::connect(&path, 8).unwrap();
        let frame = sample_output(4);
        tx.send(&frame).unwrap();
        let got = rx.recv(None).unwrap();
        assert_same_meta(got.meta, frame.meta);
        assert_same_pcm(got.pcm, frame.pcm);
        cleanup(&path);
    }

    #[test]
    fn send_counts_drops_when_receiver_stalls() {
        let path = socket_path("stall");
        let rx = FrameSocket::<InputFrame>::bind(&path, 4).unwrap();
        let tx = FrameSocket::<InputFrame>::connect(&path, 4).unwrap();
        for idx in 0..32u32 {
            tx.send(&sample_input(idx)).unwrap();
        }
        let stats = tx.stats();
        // 4 帧 SNDBUF 下实测入队约 5 条；不假设精确生效值，断言保守下限。
        assert!(stats.send_dropped >= 16, "send_dropped: {}", stats.send_dropped);
        assert_eq!(stats.recv_dropped, 0);
        // 队列保持 FIFO：最早送出的 0 号帧未被丢弃。
        let oldest = rx.recv(Some(Duration::from_secs(1))).unwrap();
        assert_same_meta(oldest.meta, sample_input(0).meta);
        cleanup(&path);
    }

    #[test]
    fn recv_counts_and_skips_malformed_packets() {
        let path = socket_path("badpkt");
        let rx = FrameSocket::<InputFrame>::bind(&path, 8).unwrap();
        // 原始套接字注入坏包，绕过 FrameSocket 的编码路径。
        // SAFETY: socket() 不访问其他内存。
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0);
        let (addr, len) = sockaddr(&path).unwrap();
        let send_raw = |bytes: &[u8]| {
            // SAFETY: bytes 有效；addr 指向已初始化的 sockaddr_un。
            let rc = unsafe {
                libc::sendto(
                    raw,
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                    (&addr as *const libc::sockaddr_un).cast(),
                    len,
                )
            };
            assert_eq!(rc as usize, bytes.len());
        };
        let mut wrong_magic = vec![0u8; <InputFrame as WirePacket>::PACKET_BYTES];
        wrong_magic[0..4].copy_from_slice(&[0xFF; 4]);
        send_raw(&wrong_magic); // 坏魔数（等长）。
        send_raw(&wrong_magic[..100]); // 短包。
        let mut oversize = vec![0u8; <InputFrame as WirePacket>::PACKET_BYTES + 64];
        oversize[0..4].copy_from_slice(&crate::INPUT_PACKET_MAGIC.to_le_bytes());
        send_raw(&oversize); // 超长包。
        // SAFETY: raw 是本用例创建且未复制的文件描述符。
        drop(unsafe { OwnedFd::from_raw_fd(raw) });

        // 坏包之后继续收：好包仍能到达。
        let tx = FrameSocket::<InputFrame>::connect(&path, 8).unwrap();
        let frame = sample_input(5);
        tx.send(&frame).unwrap();
        let got = rx.recv(Some(Duration::from_secs(1))).unwrap();
        assert_same_meta(got.meta, frame.meta);
        assert_eq!(rx.stats().recv_dropped, 3);
        assert_eq!(tx.stats().send_dropped, 0);
        cleanup(&path);
    }

    #[test]
    fn recv_times_out_when_idle() {
        let path = socket_path("idle");
        let rx = FrameSocket::<InputFrame>::bind(&path, 4).unwrap();
        let started = Instant::now();
        let err = match rx.recv(Some(Duration::from_millis(120))) {
            Ok(frame) => {
                let FrameMeta { frame_idx, .. } = frame.meta;
                panic!("expected timeout, got frame idx {frame_idx}");
            }
            Err(err) => err,
        };
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // poll 毫秒超时允许内核略提前醒来，留 5 ms 容差（同 ctl 用例）。
        assert!(started.elapsed() >= Duration::from_millis(115));
        cleanup(&path);
    }

    #[test]
    fn recv_without_timeout_blocks_until_frame_arrives() {
        let path = socket_path("blocking");
        let rx = FrameSocket::<InputFrame>::bind(&path, 8).unwrap();
        let tx = FrameSocket::<InputFrame>::connect(&path, 8).unwrap();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            tx.send(&sample_input(7)).unwrap();
        });
        let started = Instant::now();
        let got = rx.recv(None).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert_same_meta(got.meta, sample_input(7).meta);
        sender.join().unwrap();
        cleanup(&path);
    }

    #[test]
    fn send_recovers_after_peer_restart() {
        let path = socket_path("restart");
        let rx1 = FrameSocket::<InputFrame>::bind(&path, 4).unwrap();
        let tx = FrameSocket::<InputFrame>::connect(&path, 4).unwrap();
        tx.send(&sample_input(1)).unwrap();
        assert_same_meta(rx1.recv(None).unwrap().meta, sample_input(1).meta);

        // 对端关闭，socket 文件留在原地。
        drop(rx1);
        tx.send(&sample_input(2)).unwrap(); // ECONNREFUSED：丢弃 + 重连失败。
        tx.send(&sample_input(3)).unwrap(); // ENOTCONN：丢弃 + 重连失败。
        assert_eq!(tx.stats().send_dropped, 2);

        // 对端重绑（bind 清理陈旧文件）；发送方在失败分支自动重连成功。
        let rx2 = FrameSocket::<InputFrame>::bind(&path, 4).unwrap();
        tx.send(&sample_input(4)).unwrap(); // 仍走失败分支：丢弃计数，重连成功。
        assert_eq!(tx.stats().send_dropped, 3);
        tx.send(&sample_input(5)).unwrap(); // 通道已恢复：送达。
        assert_eq!(tx.stats().send_dropped, 3);
        let got = rx2.recv(Some(Duration::from_secs(1))).unwrap();
        assert_same_meta(got.meta, sample_input(5).meta);
        cleanup(&path);
    }
}
