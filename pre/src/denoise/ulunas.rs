//! 16 kHz 神经网络降噪 IPC 后端（UL-UNAS 及后续同阶段模型）：
//! 普通工作线程串行发起 SEQPACKET IPC 请求，实时处理线程只写入有界环。

use super::{fanout, WorkItem};
use crate::{ai, state::Shared};
use mozart_core::config::EnhanceConfig;
use mozart_core::ctl::schema::PreEnhanceStatus;
use mozart_core::ring::Ring;
use mozart_core::{
    EnhanceReply, EnhanceRequest, ENHANCE_PACKET_MAGIC, ENHANCE_PRIMING_FRAMES, INPUT_PACKET_MAGIC,
};
use mozart_dsp::InputFrameAnalyzer;
use std::collections::VecDeque;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub struct UlunasWorker {
    pub queue: Ring<WorkItem>,
    path: PathBuf,
    timeout: Duration,
    status: Mutex<PreEnhanceStatus>,
}

fn lock_status(status: &Mutex<PreEnhanceStatus>) -> MutexGuard<'_, PreEnhanceStatus> {
    status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl UlunasWorker {
    pub fn new(config: &EnhanceConfig, run_dir: &Path) -> Self {
        Self {
            queue: Ring::new(config.queue_frames as usize),
            path: run_dir.join(&config.audio_socket),
            timeout: Duration::from_millis(config.request_timeout_ms.into()),
            status: Mutex::new(PreEnhanceStatus::default()),
        }
    }

    pub fn status(&self, enabled: bool) -> PreEnhanceStatus {
        let mut status = lock_status(&self.status).clone();
        status.enabled = enabled;
        status.queue_depth = self.queue.depth() as u64;
        status.dropped_frames += self.queue.dropped();
        status.connected &= enabled;
        status
    }
}

pub fn run(
    enhance: Arc<UlunasWorker>,
    shared: Arc<Shared>,
    mut senders: Vec<ai::Sender>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut socket = None;
    let mut pending = VecDeque::<WorkItem>::with_capacity(ENHANCE_PRIMING_FRAMES + 1);
    let mut analyzer = InputFrameAnalyzer::default();
    let mut epoch = shared.denoise_epoch();
    let mut last_idx: Option<u32> = None;
    let mut queue_drops = 0;
    while !stop.load(Ordering::Relaxed) {
        let item = enhance.queue.pop(Some(Duration::from_millis(10)));
        let current_epoch = shared.denoise_epoch();
        let dropped = enhance.queue.dropped();
        let gap = item
            .is_some_and(|item| last_idx.is_some_and(|idx| item.source_idx != idx.wrapping_add(1)));
        if epoch != current_epoch || dropped != queue_drops || gap {
            socket = None;
            analyzer.reset();
            let mut status = lock_status(&enhance.status);
            status.connected = false;
            status.dropped_frames += pending.len() as u64;
            if dropped != queue_drops || gap {
                status.last_error = Some("enhancement input gap; session reset".into());
            } else {
                // 模式/开关切换清掉旧会话故障，避免切去本地降噪或切回时残留上次断开报警。
                status.last_error = None;
            }
            pending.clear();
            last_idx = None;
            epoch = current_epoch;
            queue_drops = dropped;
        }
        let Some(item) = item else {
            continue;
        };
        last_idx = Some(item.source_idx);
        if item.enhance_backend.is_none() {
            fanout(&item, &shared, &mut senders);
            continue;
        }
        if item.epoch != current_epoch {
            lock_status(&enhance.status).dropped_frames += 1;
            continue;
        }
        lock_status(&enhance.status).frames_in += 1;
        pending.push_back(item);
        let deadline = Instant::now() + enhance.timeout;
        let result = (|| {
            if socket.is_none() {
                socket = Some(connect(&enhance.path, deadline, stop, &shared, epoch)?);
                lock_status(&enhance.status).connected = true;
            }
            let reply = exchange(
                socket.as_ref().unwrap(),
                &item.frame,
                deadline,
                stop,
                &shared,
                epoch,
            )?;
            validate(
                &reply,
                pending.front().unwrap(),
                pending.len() > ENHANCE_PRIMING_FRAMES,
            )?;
            Ok::<_, io::Error>(reply)
        })();
        match result {
            Ok(reply) => {
                // 切换后端、关闭或重新启用发生在推理期间时，旧结果不得进入新支路。
                if shared.denoise_epoch() != epoch
                    || enhance.queue.dropped() != queue_drops
                    || stop.load(Ordering::Relaxed)
                {
                    continue;
                }
                if reply.ready == 1 {
                    let mut source = pending.pop_front().unwrap();
                    source.frame.pcm = reply.frame.pcm;
                    analyzer.analyze(&mut source.frame);
                    fanout(&source, &shared, &mut senders);
                    let mut status = lock_status(&enhance.status);
                    status.frames_out += 1;
                    status.last_error = None;
                }
            }
            Err(err) => {
                socket = None;
                analyzer.reset();
                last_idx = None;
                let mut status = lock_status(&enhance.status);
                status.connected = false;
                status.dropped_frames += pending.len() as u64;
                if shared.denoise_epoch() == epoch && !stop.load(Ordering::Relaxed) {
                    status.last_error = Some(format!("enhancement: {err}"));
                }
                pending.clear();
            }
        }
    }
    let mut status = lock_status(&enhance.status);
    status.connected = false;
    status.dropped_frames += pending.len() as u64;
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(super) fn validate(
    reply: &EnhanceReply,
    source: &WorkItem,
    ready: bool,
) -> io::Result<()> {
    if reply.magic != ENHANCE_PACKET_MAGIC || reply.ready != u32::from(ready) {
        return Err(invalid("bad enhancement magic or priming state"));
    }
    let pcm = reply.frame.pcm;
    if !pcm.iter().all(|sample| {
        sample.is_finite()
            && (-mozart_core::INPUT_PCM_ABS_MAX..=mozart_core::INPUT_PCM_ABS_MAX).contains(sample)
    }) {
        return Err(invalid("bad enhancement PCM"));
    }
    if ready
        && (reply.frame.meta.frame_idx != source.frame.meta.frame_idx
            || reply.frame.meta.pts_ns != source.frame.meta.pts_ns)
    {
        return Err(invalid("enhancement source timestamp mismatch"));
    }
    Ok(())
}

fn wait(
    fd: &OwnedFd,
    events: i16,
    deadline: Instant,
    stop: &AtomicBool,
    shared: &Shared,
    epoch: u64,
) -> io::Result<()> {
    loop {
        if stop.load(Ordering::Relaxed) || shared.denoise_epoch() != epoch {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "enhancement epoch cancelled",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "enhancement request timeout",
            ));
        }
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        // 每 10 ms 检查停止与开关，整个请求共用同一截止时间。
        let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().clamp(1, 10) as i32) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

fn connect(
    path: &Path,
    deadline: Instant,
    stop: &AtomicBool,
    shared: &Shared,
    epoch: u64,
) -> io::Result<OwnedFd> {
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return Err(invalid("enhancement socket path too long"));
    }
    addr.sun_family = libc::AF_UNIX as _;
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as _;
    }
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let result = unsafe {
        libc::connect(
            raw,
            &addr as *const _ as *const libc::sockaddr,
            size_of::<libc::sockaddr_un>() as _,
        )
    };
    if result < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(err);
        }
        wait(&fd, libc::POLLOUT, deadline, stop, shared, epoch)?;
        let mut error: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut error as *mut _ as *mut _,
                &mut len,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }
    Ok(fd)
}

pub(super) fn exchange(
    fd: &OwnedFd,
    frame: &mozart_core::InputFrame,
    deadline: Instant,
    stop: &AtomicBool,
    shared: &Shared,
    epoch: u64,
) -> io::Result<EnhanceReply> {
    let request = EnhanceRequest {
        magic: INPUT_PACKET_MAGIC,
        frame: *frame,
    };
    loop {
        wait(fd, libc::POLLOUT, deadline, stop, shared, epoch)?;
        let sent = unsafe {
            libc::send(
                fd.as_raw_fd(),
                &request as *const _ as *const _,
                size_of::<EnhanceRequest>(),
                libc::MSG_NOSIGNAL,
            )
        };
        if sent == size_of::<EnhanceRequest>() as isize {
            break;
        }
        if sent >= 0 {
            return Err(invalid("short enhancement request"));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock && err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    loop {
        wait(fd, libc::POLLIN, deadline, stop, shared, epoch)?;
        let mut reply: EnhanceReply = unsafe { zeroed() };
        let received = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                &mut reply as *mut _ as *mut _,
                size_of::<EnhanceReply>(),
                libc::MSG_TRUNC,
            )
        };
        if received == size_of::<EnhanceReply>() as isize {
            return Ok(reply);
        }
        if received >= 0 {
            return Err(invalid("bad enhancement reply length or disconnected"));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::WouldBlock && err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}
