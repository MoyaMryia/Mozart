//! 播报音频输出：48 kHz PCM 打包为 OutputFrame（960 samples / 20 ms），按
//! FRAME_MS 节拍经 tts-out.sock 逐帧发送（SPEC-05 合成一段播一段）。v1 不走
//! 变声：pre 绑定该 socket 收帧播放，TTS 连接发送（SPEC-05 播报投递）。惰性
//! 建链：pre 未绑定时发送丢弃计数，不阻塞合成路径，pre 就绪后下一帧自动接通
//! （与 pre 侧 AsrSender 同一纪律）。

use crate::state::Shared;
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, OutputFrame, FRAME_MS, OUTPUT_SAMPLES};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 输出 socket 缓冲帧数（SPEC-02 数据面上限 4–8 帧取上界）。
const OUTPUT_BUFFER_FRAMES: usize = 8;

/// 把整句 48 kHz PCM 打包为契约帧：每帧 960 samples，末帧不足补零。
/// meta：pts_ns 与 frame_idx 按句内 20 ms 步进，vad_flag 恒 1（合成语音），
/// energy_db/conf/segment_id 置 0（播报帧不做能量/置信度标注）。
pub fn pack_utterance(pcm: &[f32]) -> Vec<OutputFrame> {
    pcm.chunks(OUTPUT_SAMPLES)
        .enumerate()
        .map(|(idx, chunk)| {
            // packed 字段不能取引用：先在本地数组填充，再按值赋入。
            let mut pcm = [0f32; OUTPUT_SAMPLES];
            pcm[..chunk.len()].copy_from_slice(chunk);
            OutputFrame {
                meta: FrameMeta {
                    pts_ns: idx as u64 * FRAME_MS as u64 * 1_000_000,
                    frame_idx: idx as u32,
                    vad_flag: 1,
                    energy_db: 0,
                    conf: 0,
                    segment_id: 0,
                },
                pcm,
            }
        })
        .collect()
}

/// 输出端：连接 pre 绑定的帧 socket，synth 线程逐句写入。
/// 惰性建链 + 非阻塞发送：对端不在、过载、通道坏一律丢弃计数继续，
/// 不阻塞合成路径（降级经 GetStatus 可查）。
pub struct FrameWriter {
    /// pre `tts_output_socket` 路径；连接失败可恢复，路径固定。
    path: PathBuf,
    socket: Option<FrameSocket<OutputFrame>>,
    shared: Arc<Shared>,
    /// 未建链期（pre 未绑定）与通道弃用后的发送丢弃合计；建链后的过载丢弃
    /// 由 FrameSocket 内部计数，发布时两者相加。
    dropped: u64,
}

impl FrameWriter {
    pub fn new(path: PathBuf, shared: Arc<Shared>) -> Self {
        Self {
            path,
            socket: None,
            shared,
            dropped: 0,
        }
    }

    /// 发送整句帧：按 FRAME_MS 节拍逐帧投递（合成一段播一段），突发会超出
    /// 接收方 8 帧缓冲造成丢帧。停止标志置位即放弃剩余帧（SIGTERM 及时退出）。
    /// 每帧计入 frames_out（含丢弃）；丢弃累计绝对值发布到 output_dropped，
    /// 意外错误记录原因后继续，不打断合成路径。
    pub fn write_utterance(&mut self, pcm: &[f32], stop: &AtomicBool) {
        let frame_duration = Duration::from_millis(FRAME_MS as u64);
        let mut next = Instant::now();
        for (idx, frame) in pack_utterance(pcm).iter().enumerate() {
            if stop.load(Relaxed) {
                break;
            }
            if idx > 0 {
                // 单调时钟节拍：发送耗时计入周期；落后时从当前时刻重新起算，
                // 不追赶补发。合成线程非实时，睡眠让出 CPU。
                next = next.max(Instant::now()) + frame_duration;
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
            self.send_frame(frame);
            self.shared.add_frames_out(1);
        }
    }

    /// 恒定发送一帧。对端不在、过载、通道坏：丢弃计数继续，不阻塞不重试。
    fn send_frame(&mut self, frame: &OutputFrame) {
        if self.socket.is_none() {
            match FrameSocket::connect(&self.path, OUTPUT_BUFFER_FRAMES) {
                Ok(socket) => self.socket = Some(socket),
                // pre 未绑定 socket：计入丢弃，下一帧再试建链。
                Err(_) => {
                    self.dropped += 1;
                    self.shared.set_output_dropped(self.dropped);
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
                    self.shared.set_output_dropped(total);
                }
                Err(err) => {
                    // 未预期错误（超出过载与对端不在的恢复集）：计数继续，
                    // 原因可经状态查询；内部计数并入后弃用通道。
                    self.dropped += 1 + socket.stats().send_dropped;
                    self.shared.set_output_dropped(self.dropped);
                    self.shared
                        .set_last_error(&format!("tts output socket: {err}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn socket_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mozart-tts-out-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn no_stop() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn pack_full_frames_meta_and_pcm() {
        let pcm: Vec<f32> = (0..1920).map(|i| i as f32 * 0.001).collect();
        let frames = pack_utterance(&pcm);
        assert_eq!(frames.len(), 2);
        for (idx, frame) in frames.iter().enumerate() {
            let FrameMeta {
                pts_ns,
                frame_idx,
                vad_flag,
                energy_db,
                conf,
                segment_id,
            } = frame.meta;
            assert_eq!(pts_ns, idx as u64 * 20_000_000);
            assert_eq!(frame_idx, idx as u32);
            assert_eq!(vad_flag, 1);
            assert_eq!((energy_db, conf, segment_id), (0, 0, 0));
            let chunk = frame.pcm;
            assert_eq!(&chunk[..], &pcm[idx * 960..(idx + 1) * 960]);
        }
    }

    #[test]
    fn pack_pads_partial_last_frame_with_zeros() {
        let pcm = vec![0.5f32; 961];
        let frames = pack_utterance(&pcm);
        assert_eq!(frames.len(), 2);
        let last = frames[1].pcm;
        assert_eq!(last[0], 0.5);
        assert!(last[1..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn pack_empty_yields_no_frames() {
        assert!(pack_utterance(&[]).is_empty());
    }

    /// pre 未绑定：惰性建链失败，发送全部丢弃计数，不阻塞、不报错。
    #[test]
    fn lazy_connect_drops_and_counts_until_peer_binds() {
        let dir = socket_dir("lazy");
        let path = dir.join("tts-out.sock");
        let shared = Arc::new(Shared::new());
        let mut writer = FrameWriter::new(path.clone(), shared.clone());
        let stop = no_stop();
        writer.write_utterance(&vec![0.25f32; 4800], &stop);
        let status = shared.status();
        assert_eq!(status.frames_out, 5);
        assert_eq!(status.output_dropped, 5, "pre 未绑定：5 帧全部丢弃计数");

        // pre 绑定后下一句自动接通并送达。
        let rx = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        writer.write_utterance(&vec![0.25f32; 960], &stop);
        let got = rx.recv(Some(Duration::from_secs(1))).unwrap();
        let FrameMeta { frame_idx, .. } = got.meta;
        assert_eq!(frame_idx, 0);
        let status = shared.status();
        assert_eq!(status.frames_out, 6);
        assert_eq!(status.output_dropped, 5, "送达后丢弃计数不再增长");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 接收方在场：整句 OutputFrame 逐帧送达，内容与时序正确。
    #[test]
    fn frames_arrive_at_bound_receiver() {
        let dir = socket_dir("arrive");
        let path = dir.join("tts-out.sock");
        let rx = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        let shared = Arc::new(Shared::new());
        let mut writer = FrameWriter::new(path.clone(), shared.clone());
        let stop = no_stop();
        let pcm: Vec<f32> = (0..1920).map(|i| i as f32 * 0.0001).collect();
        writer.write_utterance(&pcm, &stop);
        for idx in 0..2u32 {
            let got = rx.recv(Some(Duration::from_secs(1))).unwrap();
            let FrameMeta { frame_idx, pts_ns, .. } = got.meta;
            assert_eq!(frame_idx, idx);
            assert_eq!(pts_ns, idx as u64 * 20_000_000);
            let chunk = got.pcm;
            assert_eq!(&chunk[..], &pcm[idx as usize * 960..(idx as usize + 1) * 960]);
        }
        let status = shared.status();
        assert_eq!(status.frames_out, 2);
        assert_eq!(status.output_dropped, 0);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// pre 重启窗口：发送丢弃计数，重绑后自动恢复送达（绝对值单调不回退）。
    #[test]
    fn recovers_after_peer_restart_and_keeps_count() {
        let dir = socket_dir("restart");
        let path = dir.join("tts-out.sock");
        let shared = Arc::new(Shared::new());
        let mut writer = FrameWriter::new(path.clone(), shared.clone());
        let stop = no_stop();

        let rx1 = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        writer.write_utterance(&vec![0.25f32; 960], &stop);
        let FrameMeta { frame_idx, .. } = rx1.recv(None).unwrap().meta;
        assert_eq!(frame_idx, 0);
        drop(rx1); // socket 文件留在原地。
        writer.write_utterance(&vec![0.25f32; 960], &stop); // 丢弃 + 重连失败。
        assert_eq!(shared.status().output_dropped, 1, "重启窗口的发送必须计入丢弃");

        let rx2 = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        writer.write_utterance(&vec![0.25f32; 960], &stop); // 仍走失败分支：丢弃 + 重连成功。
        writer.write_utterance(&vec![0.25f32; 960], &stop); // 通道已恢复：送达。
        let FrameMeta { frame_idx, .. } = rx2.recv(Some(Duration::from_secs(1))).unwrap().meta;
        assert_eq!(frame_idx, 0);
        assert_eq!(shared.status().output_dropped, 2, "重启窗口丢弃 1 + 重绑首帧丢弃 1");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 节拍投递：12 帧超出 8 帧接收缓冲，接收线程持续取帧。突发发送必丢帧；
    /// 节拍发送全部送达，整句耗时不小于 11 个帧周期（首帧立即发送）。
    #[test]
    fn paced_delivery_fits_receiver_buffer() {
        let dir = socket_dir("paced");
        let path = dir.join("tts-out.sock");
        let rx = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        let shared = Arc::new(Shared::new());
        let mut writer = FrameWriter::new(path.clone(), shared.clone());
        let stop = no_stop();
        let receiver = std::thread::spawn(move || {
            for idx in 0..12u32 {
                let frame = rx.recv(Some(Duration::from_secs(2))).unwrap();
                // packed 字段经解构按值读取，不能取引用。
                let FrameMeta { frame_idx, .. } = frame.meta;
                assert_eq!(frame_idx, idx);
            }
        });
        let pcm = vec![0.25f32; OUTPUT_SAMPLES * 12];
        let start = Instant::now();
        writer.write_utterance(&pcm, &stop);
        let elapsed = start.elapsed();
        receiver.join().unwrap();
        assert!(
            elapsed >= Duration::from_millis(11 * FRAME_MS as u64),
            "12 帧节拍投递耗时 {elapsed:?}，不足 11 个帧周期"
        );
        let status = shared.status();
        assert_eq!(status.frames_out, 12);
        assert_eq!(status.output_dropped, 0, "节拍投递不得超出接收缓冲");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 投递中途置停止标志：剩余帧放弃（SIGTERM 在宽限内退出）。接收线程收到
    /// 前 2 帧后置位；置位与下一帧发送之间隔一个帧周期，计数允许 1 帧在途余量。
    #[test]
    fn stop_interrupts_remaining_frames() {
        let dir = socket_dir("stop");
        let path = dir.join("tts-out.sock");
        let rx = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        let shared = Arc::new(Shared::new());
        let mut writer = FrameWriter::new(path.clone(), shared.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_rx = stop.clone();
        let receiver = std::thread::spawn(move || {
            for idx in 0..2u32 {
                let frame = rx.recv(Some(Duration::from_secs(2))).unwrap();
                // packed 字段经解构按值读取，不能取引用。
                let FrameMeta { frame_idx, .. } = frame.meta;
                assert_eq!(frame_idx, idx);
            }
            stop_rx.store(true, Relaxed);
        });
        writer.write_utterance(&vec![0.25f32; OUTPUT_SAMPLES * 16], &stop);
        receiver.join().unwrap();
        let frames_out = shared.status().frames_out;
        assert!(frames_out >= 2, "已送达的 2 帧必须计入：{frames_out}");
        assert!(frames_out < 16, "停止后剩余帧必须放弃：{frames_out}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}