//! 收包线程（SPEC-05）：绑定输入 DGRAM，收 16 kHz/320 样本帧推入 SPSC 环。
//! 热路径只用栈缓冲与原子计数，不分配、不阻塞。坏包由 FrameSocket 计数拒收。
//! 1 s 空闲兜底：向环投递 EndOfInput 标记，由解码线程切出最后 final。

use crate::state::{unix_ms, Shared};
use mozart_core::dgram::FrameSocket;
use mozart_core::ring::Ring;
use mozart_core::InputFrame;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 收包 → 解码环的载荷：音频帧或空闲切句标记。Copy，环内零分配。
#[derive(Clone, Copy)]
pub enum Inbox {
    Frame(InputFrame),
    /// 输入空闲超过阈值，请求解码线程切句。
    EndOfInput,
}

/// pre 断流空闲兜底切句阈值（SPEC-05：1 s）。
const IDLE_CUT: Duration = Duration::from_secs(1);
/// recv 轮询步长：超时后检查停止标志与空闲累计。
const RECV_POLL: Duration = Duration::from_millis(100);

/// 收包循环。socket 由调用方绑定后移交；停止标志置位后返回。
pub fn run(
    socket: FrameSocket<InputFrame>,
    ring: Arc<Ring<Inbox>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let mut last_packet = Instant::now();
    // 首包前不投递切句标记。
    let mut idle_cut_sent = true;
    while !stop.load(Relaxed) {
        match socket.recv(Some(RECV_POLL)) {
            Ok(frame) => {
                ring.push(Inbox::Frame(frame));
                shared.add_received(1);
                shared.set_last_packet_unix_ms(unix_ms());
                last_packet = Instant::now();
                idle_cut_sent = false;
            }
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                if !idle_cut_sent && last_packet.elapsed() >= IDLE_CUT {
                    ring.push(Inbox::EndOfInput);
                    idle_cut_sent = true;
                }
            }
            Err(err) => return Err(format!("输入收包：{err}")),
        }
        shared.set_frames_rejected(socket.stats().recv_dropped);
    }
    Ok(())
}