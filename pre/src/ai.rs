//! 数据面接线：16 kHz InputFrame 恒定发送（rvc AI 支路 + asr 识别支路）与
//! OutputFrame 接收（rvc AI 支路）。
//! 发送侧惰性建链：pre 启动不依赖对端生命周期；对端未绑定 socket 时发送
//! 丢弃计数（旧 main.c L840-842「发送失败只计数继续」），对端就绪后下一帧
//! 自动接通。接收侧小缓冲只吸收送达抖动，满丢最旧计数（SPEC-03：大缓冲与
//! 欠载补零归 C++ 侧）。

use crate::state::Shared;
use mozart_core::dgram::FrameSocket;
use mozart_core::ring::Ring;
use mozart_core::{InputFrame, OutputFrame};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// AI 接收环容量：SPEC-02「接收缓冲限制在 4–8 帧」取上沿。
pub const RVC_QUEUE_FRAMES: usize = 8;

/// 输入支路发送缓冲帧数（SPEC-02 数据面 4–8 帧取上沿）：rvc 与 asr 共用。
const SEND_BUFFER_FRAMES: usize = 8;

/// 接收线程 recv 轮询步长：超时后检查停止标志，保证秒级内退出。
const RECV_POLL: Duration = Duration::from_millis(100);

/// 丢弃计数发布目标：rvc 与 asr 两条输入支路共用同一惰性发送机制，
/// 只有计数发布位置与错误标签不同。
#[derive(Clone, Copy)]
enum SendTarget {
    Rvc,
    Asr,
}

impl SendTarget {
    fn publish(self, shared: &Shared, total: u64) {
        match self {
            SendTarget::Rvc => shared.set_ai_send_dropped(total),
            SendTarget::Asr => shared.set_asr_send_dropped(total),
        }
    }

    fn label(self) -> &'static str {
        match self {
            SendTarget::Rvc => "rvc input socket",
            SendTarget::Asr => "asr input socket",
        }
    }
}

/// 惰性建链 + 恒定发送的共享机制。发送不随路由停发（旧 main.c L11「AI 路径
/// 永不停」）；丢弃计数以绝对值发布到 Shared，控制线程只读原子。
struct LazySender {
    /// 对端 `input_socket` 路径；连接失败可恢复，路径固定。
    path: PathBuf,
    socket: Option<FrameSocket<InputFrame>>,
    /// 未建链期（对端未绑定）与通道弃用后的发送丢弃合计；建链后的过载丢弃
    /// 由 FrameSocket 内部计数，发布时两者相加。
    dropped: u64,
    target: SendTarget,
}

impl LazySender {
    fn new(path: PathBuf, target: SendTarget) -> Self {
        Self {
            path,
            socket: None,
            dropped: 0,
            target,
        }
    }

    /// 恒定发送一帧。对端不在、过载、通道坏：丢弃计数继续，不阻塞不重试。
    fn send(&mut self, frame: &InputFrame, shared: &Shared) {
        if self.socket.is_none() {
            match FrameSocket::connect(&self.path, SEND_BUFFER_FRAMES) {
                Ok(socket) => self.socket = Some(socket),
                // 对端未绑定 socket（对端未起）：计入丢弃，下一帧再试建链。
                Err(_) => {
                    self.dropped += 1;
                    self.target.publish(shared, self.dropped);
                    return;
                }
            }
        }
        // 按值取回再放回：发送失败后通道状态未知，弃用并由下一帧重建链。
        if let Some(socket) = self.socket.take() {
            match socket.send(frame) {
                Ok(()) => {
                    let total = self.dropped + socket.stats().send_dropped;
                    self.socket = Some(socket);
                    self.target.publish(shared, total);
                }
                Err(err) => {
                    // 未预期错误（超出过载与对端不在的恢复集）：旧语义计数继续，
                    // 原因可经状态查询；内部计数并入后弃用通道。
                    self.dropped += 1 + socket.stats().send_dropped;
                    self.target.publish(shared, self.dropped);
                    shared.set_last_error(&format!("{}: {err}", self.target.label()));
                }
            }
        }
    }
}

/// rvc AI 支路发送端：连接 rvc `input_socket`，恒定发送 16 kHz InputFrame。
pub struct Sender(LazySender);

impl Sender {
    pub fn new(path: PathBuf) -> Self {
        Self(LazySender::new(path, SendTarget::Rvc))
    }

    pub fn send(&mut self, frame: &InputFrame, shared: &Shared) {
        self.0.send(frame, shared);
    }
}

/// asr 识别支路发送端：连接 asr `input_socket`，与 rvc 同帧恒发。
/// asr 未起时建链失败丢弃计数，不阻塞 pre（SPEC-05 识别服务）。
pub struct AsrSender(LazySender);

impl AsrSender {
    pub fn new(path: PathBuf) -> Self {
        Self(LazySender::new(path, SendTarget::Asr))
    }

    pub fn send(&mut self, frame: &InputFrame, shared: &Shared) {
        self.0.send(frame, shared);
    }
}

/// 接收线程主体：绑定好的 output_socket → AI 接收环。
/// 路由相关丢弃（direct 期排空计数）由播放线程消费时决定，本线程只搬运。
/// 返回 Err 表示致命 socket 错误；主线程据此退出进程。
pub fn recv_loop(
    socket: FrameSocket<OutputFrame>,
    ring: Arc<Ring<OutputFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match socket.recv(Some(RECV_POLL)) {
            Ok(frame) => ring.push(frame),
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {}
            Err(err) => return Err(format!("rvc output socket: {err}")),
        }
        // 坏包丢弃计数发布（绝对值）；接收环满丢弃由控制线程直接读环。
        shared.set_ai_recv_dropped(socket.stats().recv_dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mozart_core::{FrameMeta, INPUT_SAMPLES};
    fn socket_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mozart-pre-ai-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_frame(idx: u32) -> InputFrame {
        let mut pcm = [0f32; INPUT_SAMPLES];
        for (i, slot) in pcm.iter_mut().enumerate() {
            *slot = i as f32 * 0.25 + idx as f32;
        }
        InputFrame {
            meta: FrameMeta {
                pts_ns: 1000 + idx as u64,
                frame_idx: idx,
                vad_flag: 1,
                energy_db: 2,
                conf: 3,
                segment_id: 4,
            },
            pcm,
        }
    }

    fn consumer_drops(shared: &Shared) -> (u64, u64) {
        let status = shared.status(crate::state::QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
        });
        (
            status.consumer_drops["rvc"],
            status.consumer_drops["rvc.skipped"],
        )
    }

    #[test]
    fn sender_counts_drops_until_peer_binds_then_delivers() {
        // 惰性建链：对端未起 → 丢弃计数；对端绑定 → 送达且计数不再增长。
        let dir = socket_dir("lazy");
        let path = dir.join("rvc-in.sock");
        let shared = Shared::new("cap".to_string(), "play".to_string());
        let mut sender = Sender::new(path.clone());

        for idx in 0..3u32 {
            sender.send(&sample_frame(idx), &shared);
        }
        let (rvc, skipped) = consumer_drops(&shared);
        assert_eq!((rvc, skipped), (3, 0), "对端未绑定：3 帧全部丢弃计数");

        let rx = FrameSocket::<InputFrame>::bind(&path, RVC_QUEUE_FRAMES).unwrap();
        sender.send(&sample_frame(3), &shared);
        // 建链后首帧送达（connect 成功，send 不走失败分支）。
        let got = rx.recv(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 3, "建链后帧必须送达");
        let (rvc, _) = consumer_drops(&shared);
        assert_eq!(rvc, 3, "送达后丢弃计数不再增长");
        sender.send(&sample_frame(4), &shared);
        let FrameMeta { frame_idx, .. } = rx.recv(Some(Duration::from_secs(1))).unwrap().meta;
        assert_eq!(frame_idx, 4);
        let (rvc, _) = consumer_drops(&shared);
        assert_eq!(rvc, 3);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn sender_recovers_after_peer_restart_and_keeps_count() {
        // 对端重启窗口内发送丢弃计数；重绑后自动恢复送达，且发布值把
        // FrameSocket 内部计数并入（绝对值单调不回退）。
        let dir = socket_dir("restart");
        let path = dir.join("rvc-in.sock");
        let shared = Shared::new("cap".to_string(), "play".to_string());
        let mut sender = Sender::new(path.clone());

        let rx1 = FrameSocket::<InputFrame>::bind(&path, RVC_QUEUE_FRAMES).unwrap();
        sender.send(&sample_frame(1), &shared);
        let FrameMeta { frame_idx, .. } = rx1.recv(None).unwrap().meta;
        assert_eq!(frame_idx, 1);
        drop(rx1); // socket 文件留在原地。
        sender.send(&sample_frame(2), &shared); // 丢弃 + 重连失败。
        let (rvc, _) = consumer_drops(&shared);
        assert_eq!(rvc, 1, "重启窗口的发送必须计入丢弃");

        let rx2 = FrameSocket::<InputFrame>::bind(&path, RVC_QUEUE_FRAMES).unwrap();
        sender.send(&sample_frame(3), &shared); // 仍走失败分支：丢弃 + 重连成功。
        sender.send(&sample_frame(4), &shared); // 通道已恢复：送达。
        let FrameMeta { frame_idx, .. } = rx2.recv(Some(Duration::from_secs(1))).unwrap().meta;
        assert_eq!(frame_idx, 4);
        let (rvc, _) = consumer_drops(&shared);
        assert_eq!(rvc, 2, "未建链丢弃 1 + 重启窗口丢弃 1");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn asr_sender_counts_drops_until_peer_binds_then_delivers() {
        // asr 支路与 rvc 同一惰性建链纪律：对端未起丢弃计数（计入 "asr"），
        // 对端绑定后送达且计数不再增长；与 rvc 计数互不串扰。
        let dir = socket_dir("asr");
        let path = dir.join("asr-in.sock");
        let shared = Shared::new("cap".to_string(), "play".to_string());
        let mut sender = AsrSender::new(path.clone());

        for idx in 0..3u32 {
            sender.send(&sample_frame(idx), &shared);
        }
        let status = shared.status(crate::state::QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
        });
        assert_eq!(status.consumer_drops["asr"], 3, "asr 未绑定：3 帧全部丢弃计数");
        assert_eq!(status.consumer_drops["rvc"], 0, "asr 丢弃不得串入 rvc 计数");

        let rx = FrameSocket::<InputFrame>::bind(&path, SEND_BUFFER_FRAMES).unwrap();
        sender.send(&sample_frame(3), &shared);
        let got = rx.recv(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 3, "建链后帧必须送达");
        let status = shared.status(crate::state::QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
        });
        assert_eq!(status.consumer_drops["asr"], 3, "送达后丢弃计数不再增长");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn recv_loop_pushes_frames_and_stops_on_flag() {
        let dir = socket_dir("recv");
        let path = dir.join("rvc-out.sock");
        let rx = FrameSocket::<OutputFrame>::bind(&path, RVC_QUEUE_FRAMES).unwrap();
        let tx = FrameSocket::<OutputFrame>::connect(&path, RVC_QUEUE_FRAMES).unwrap();
        let ring = Arc::new(Ring::<OutputFrame>::new(RVC_QUEUE_FRAMES));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string()));
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            std::thread::spawn(move || recv_loop(rx, ring, shared, stop))
        };

        let frame = OutputFrame {
            meta: FrameMeta {
                pts_ns: 42,
                frame_idx: 7,
                vad_flag: 1,
                energy_db: 2,
                conf: 3,
                segment_id: 4,
            },
            pcm: [0.5; mozart_core::OUTPUT_SAMPLES],
        };
        tx.send(&frame).unwrap();
        let got = ring.pop(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 7, "接收线程必须把输出帧推入 AI 环");

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn queue_frames_within_confirmed_range() {
        assert!(
            (4..=8).contains(&RVC_QUEUE_FRAMES),
            "SPEC-02：接收缓冲限制在 4–8 帧"
        );
    }
}
