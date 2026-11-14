//! 数据面接线：16 kHz InputFrame 恒定发送（rvc AI 支路 + asr 识别支路）与
//! OutputFrame 接收（rvc AI 支路 + tts 播报支路）。
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

/// TTS 播报接收环容量（SPEC-02 数据面 4–8 帧取上沿）：吸收送达抖动，
/// 满丢最旧计数；播放线程播报优先消费。
pub const TTS_QUEUE_FRAMES: usize = 8;

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

/// 接收线程共享主体：绑定好的 output_socket → 接收环。
/// 路由/播报相关丢弃由播放线程消费时决定，本线程只搬运。
/// 返回 Err 表示致命 socket 错误；主线程据此退出进程。
fn recv_loop_inner(
    socket: FrameSocket<OutputFrame>,
    ring: Arc<Ring<OutputFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
    label: &str,
    publish_recv_dropped: fn(&Shared, u64),
) -> Result<(), String> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match socket.recv(Some(RECV_POLL)) {
            Ok(frame) => ring.push(frame),
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {}
            Err(err) => return Err(format!("{label}: {err}")),
        }
        // 坏包丢弃计数发布（绝对值）；接收环满丢弃由控制线程直接读环。
        publish_recv_dropped(&shared, socket.stats().recv_dropped);
    }
}

/// rvc AI 支路接收线程：output_socket → AI 接收环，坏包丢弃计入 ai_recv_dropped。
/// 单一权威门控（SPEC-04）：仅当 daemon 已断言实时态（mode_asserted && !file_mode）
/// 才绑定 output_socket 收帧；未断言（启动）或文件接管时释放绑定并空转等待。
/// pre 启动不单边绑定——等 daemon 首个 PRE_SET_FILE_MODE 断言才激活，重启同理，
/// 杜绝重启无条件 reclaim 孤儿化 daemon 绑定。状态门控交接（SPEC-03「切换时
/// 交接绑定」）：file_mode=true 释放给 daemon 跑文件数据面，交还重绑恢复接收。
/// 握手标志 output_released 供控制线程确认交接完成后再应答。
/// 释放只关 fd 不删 socket 文件：daemon 侧 bind 会清理陈旧文件后重绑。
pub fn recv_loop(
    path: PathBuf,
    ring: Arc<Ring<OutputFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    // 启动不绑定：socket 为 None，等 daemon 断言实时态后在活跃分支惰性绑定。
    let mut socket: Option<FrameSocket<OutputFrame>> = None;
    // 坏包丢弃按绝对值发布：重绑后新 socket 计数从零起，旧 socket 的计数
    // 累入基线，保证发布值单调不回退。
    let mut dropped_base = 0u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !shared.rvc_ai_active() {
            if let Some(old) = socket.take() {
                dropped_base += old.stats().recv_dropped;
                shared.set_ai_recv_dropped(dropped_base);
                // old 在此 drop：关闭 fd 解除绑定，文件留给 daemon bind 清理。
            }
            shared.set_output_released(true);
            // 未断言或文件接管：数据面不归 pre，本线程空转；按 RECV_POLL 查停止与断言。
            std::thread::sleep(RECV_POLL);
            continue;
        }
        let current = match socket.as_ref() {
            Some(bound) => bound,
            None => {
                // daemon 已断言实时态：绑定输出 socket（启动首次绑定或交接后重绑，
                // daemon 已先解绑，次序见 worker 守卫）。先清握手标志再绑：重绑
                // 窗口内 output_released 必须为 false，否则并发的 active=true 会
                // 误判「已释放」而抢先绑定，与本线程撞绑（标志只在静默分支置 true，
                // 该分支不会绑定）。绑定失败属部署错误，致命退出由 daemon 重启。
                shared.set_output_released(false);
                let fresh = FrameSocket::<OutputFrame>::bind(&path, RVC_QUEUE_FRAMES)
                    .map_err(|err| format!("rvc output socket bind: {err}"))?;
                socket.insert(fresh)
            }
        };
        match current.recv(Some(RECV_POLL)) {
            Ok(frame) => ring.push(frame),
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {}
            Err(err) => return Err(format!("rvc output socket: {err}")),
        }
        shared.set_ai_recv_dropped(dropped_base + current.stats().recv_dropped);
    }
}

/// tts 播报支路接收线程：tts-out.sock → 播报接收环。坏包丢弃计入 tts_recv_dropped，
/// 与播报环满丢最旧（控制线程读环）一并经 consumer_drops["tts"] 上报 GetStatus。
pub fn tts_recv_loop(
    socket: FrameSocket<OutputFrame>,
    ring: Arc<Ring<OutputFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    recv_loop_inner(
        socket,
        ring,
        shared,
        stop,
        "tts output socket",
        Shared::set_tts_recv_dropped,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mozart_core::{FrameMeta, INPUT_SAMPLES};
    use std::path::Path;
    use std::time::Instant;
    fn socket_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mozart-pre-ai-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 接收线程在 daemon 断言后惰性绑定；轮询连接直到绑定完成（时限内）。
    fn connect_output(path: &Path, timeout: Duration) -> FrameSocket<OutputFrame> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(tx) = FrameSocket::<OutputFrame>::connect(path, RVC_QUEUE_FRAMES) {
                return tx;
            }
            assert!(Instant::now() < deadline, "output socket 未在时限内绑定");
            std::thread::sleep(Duration::from_millis(5));
        }
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
            tts_dropped: 0,
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
        let shared = Shared::new("cap".to_string(), "play".to_string(), "startup".to_string());
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
        let shared = Shared::new("cap".to_string(), "play".to_string(), "startup".to_string());
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
        let shared = Shared::new("cap".to_string(), "play".to_string(), "startup".to_string());
        let mut sender = AsrSender::new(path.clone());

        for idx in 0..3u32 {
            sender.send(&sample_frame(idx), &shared);
        }
        let status = shared.status(crate::state::QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
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
            tts_dropped: 0,
        });
        assert_eq!(status.consumer_drops["asr"], 3, "送达后丢弃计数不再增长");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn recv_loop_pushes_frames_and_stops_on_flag() {
        let dir = socket_dir("recv");
        let path = dir.join("rvc-out.sock");
        let ring = Arc::new(Ring::<OutputFrame>::new(RVC_QUEUE_FRAMES));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string(), "startup".to_string()));
        // daemon 断言实时态后接收线程才惰性绑定（启动门控）。
        shared.set_mode_asserted(true);
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            let path = path.clone();
            std::thread::spawn(move || recv_loop(path, ring, shared, stop))
        };
        let tx = connect_output(&path, Duration::from_secs(2));

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

    /// 输出 socket 状态门控交接（SPEC-03）：file_mode=true 释放绑定并置握手
    /// 标志，对端（模拟 daemon）可接管同一路径；file_mode=false 重绑恢复接收。
    #[test]
    fn recv_loop_releases_and_rebinds_output_socket_on_file_mode() {
        let dir = socket_dir("handoff");
        let path = dir.join("rvc-out.sock");
        let ring = Arc::new(Ring::<OutputFrame>::new(RVC_QUEUE_FRAMES));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string(), "startup".to_string()));
        shared.set_mode_asserted(true);
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            let path = path.clone();
            std::thread::spawn(move || recv_loop(path, ring, shared, stop))
        };
        let frame = OutputFrame {
            meta: FrameMeta {
                pts_ns: 1, frame_idx: 1, vad_flag: 1, energy_db: 0, conf: 0, segment_id: 0,
            },
            pcm: [0.25; mozart_core::OUTPUT_SAMPLES],
        };

        // 实时态：送达入环。
        let tx = connect_output(&path, Duration::from_secs(2));
        tx.send(&frame).unwrap();
        let got = ring.pop(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 1);
        assert!(!shared.output_released());

        // 文件接管：释放绑定，握手标志置位；对端可绑定同一路径（daemon 侧）。
        shared.set_file_mode(true);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !shared.output_released() {
            assert!(Instant::now() < deadline, "释放握手必须在时限内完成");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(tx); // 旧连接指向已释放的 inode，弃用。
        let daemon_rx = FrameSocket::<OutputFrame>::bind(&path, RVC_QUEUE_FRAMES).unwrap();
        let daemon_tx = FrameSocket::<OutputFrame>::connect(&path, RVC_QUEUE_FRAMES).unwrap();
        daemon_tx.send(&frame).unwrap();
        let got = daemon_rx.recv(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 1, "释放后对端必须能接管同一 socket");
        // 接管期间 pre 不再收帧：环无新帧。
        assert!(ring.pop(Some(Duration::from_millis(100))).is_none());
        drop(daemon_rx);
        drop(daemon_tx);
        let _ = std::fs::remove_file(&path);

        // 交还：重绑恢复接收，握手标志清零。
        shared.set_file_mode(false);
        let deadline = Instant::now() + Duration::from_secs(2);
        while shared.output_released() {
            assert!(Instant::now() < deadline, "重绑必须在时限内完成");
            std::thread::sleep(Duration::from_millis(5));
        }
        let tx2 = connect_output(&path, Duration::from_secs(2));
        tx2.send(&frame).unwrap();
        let got = ring.pop(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 1, "交还后必须恢复接收");

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 启动门控（SPEC-04 单一权威，根治重启单边 reclaim）：未断言时接收线程
    /// 不得绑定 output_socket（pre 启动/重启静默等 daemon 断言）；断言实时态后
    /// 才绑定并恢复接收。旧实现启动无条件绑定，本用例必失败。
    #[test]
    fn recv_loop_does_not_bind_until_asserted() {
        let dir = socket_dir("startup-gate");
        let path = dir.join("rvc-out.sock");
        let ring = Arc::new(Ring::<OutputFrame>::new(RVC_QUEUE_FRAMES));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string(), "startup".to_string()));
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            let path = path.clone();
            std::thread::spawn(move || recv_loop(path, ring, shared, stop))
        };

        // 未断言：给接收线程数个节拍，仍不得绑定（连接失败 = 路径无绑定）。
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            FrameSocket::<OutputFrame>::connect(&path, RVC_QUEUE_FRAMES).is_err(),
            "未断言不得绑定 output socket（pre 不单边 reclaim）"
        );
        assert!(shared.output_released(), "未断言应处于释放空转态");

        // daemon 断言实时态：绑定并恢复接收。
        shared.set_mode_asserted(true);
        let tx = connect_output(&path, Duration::from_secs(2));
        let frame = OutputFrame {
            meta: FrameMeta {
                pts_ns: 9, frame_idx: 9, vad_flag: 1, energy_db: 0, conf: 0, segment_id: 0,
            },
            pcm: [0.5; mozart_core::OUTPUT_SAMPLES],
        };
        tx.send(&frame).unwrap();
        let got = ring.pop(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 9, "断言后必须绑定并接收");

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn tts_recv_loop_pushes_frames_and_stops_on_flag() {
        let dir = socket_dir("tts-recv");
        let path = dir.join("tts-out.sock");
        let rx = FrameSocket::<OutputFrame>::bind(&path, TTS_QUEUE_FRAMES).unwrap();
        let tx = FrameSocket::<OutputFrame>::connect(&path, TTS_QUEUE_FRAMES).unwrap();
        let ring = Arc::new(Ring::<OutputFrame>::new(TTS_QUEUE_FRAMES));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string(), "startup".to_string()));
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            std::thread::spawn(move || tts_recv_loop(rx, ring, shared, stop))
        };

        let frame = OutputFrame {
            meta: FrameMeta {
                pts_ns: 42,
                frame_idx: 7,
                vad_flag: 1,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm: [0.5; mozart_core::OUTPUT_SAMPLES],
        };
        tx.send(&frame).unwrap();
        let got = ring.pop(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 7, "接收线程必须把播报帧推入播报环");

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
