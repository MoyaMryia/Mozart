//! 播报音频输出（SPEC-05 合成一段播一段）：synth 线程经生成回调把每段 PCM 重采样
//! 到 48 kHz 后送入有界通道，本模块的输出线程收一段即打包为 OutputFrame
//! （960 samples / 20 ms）按 FRAME_MS 节拍经 tts-out.sock 逐帧发送，不等整句合成完。
//! v1 不走变声：pre 绑定该 socket 收帧播放，TTS 连接发送（SPEC-05 播报投递）。惰性
//! 建链：pre 未绑定时发送丢弃计数，不阻塞合成路径，pre 就绪后下一帧自动接通
//! （与 pre 侧 AsrSender 同一纪律）。

use crate::state::{Abort, Shared};
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, OutputFrame, FRAME_MS, OUTPUT_SAMPLES};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 输出 socket 缓冲帧数（SPEC-02 数据面上限 4–8 帧取上界）。
const OUTPUT_BUFFER_FRAMES: usize = 8;

/// 把一段 PCM 打包为单帧：不足 960 samples 补零。
/// meta：pts_ns 与 frame_idx 按句内 20 ms 步进，vad_flag 恒 1（合成语音），
/// energy_db/conf/segment_id 置 0（播报帧不做能量/置信度标注）。
fn make_frame(chunk: &[f32], idx: u32) -> OutputFrame {
    // packed 字段不能取引用：先在本地数组填充，再按值赋入。
    let mut pcm = [0f32; OUTPUT_SAMPLES];
    pcm[..chunk.len()].copy_from_slice(chunk);
    OutputFrame {
        meta: FrameMeta {
            pts_ns: idx as u64 * FRAME_MS as u64 * 1_000_000,
            frame_idx: idx,
            vad_flag: 1,
            energy_db: 0,
            conf: 0,
            segment_id: 0,
        },
        pcm,
    }
}

/// 把整句 48 kHz PCM 打包为契约帧：每帧 960 samples，末帧不足补零。
/// 测试夹具：生产路径经 write_stream 逐段打包。
#[cfg(test)]
pub fn pack_utterance(pcm: &[f32]) -> Vec<OutputFrame> {
    pcm.chunks(OUTPUT_SAMPLES)
        .enumerate()
        .map(|(idx, chunk)| make_frame(chunk, idx as u32))
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

    /// 发送整句帧：把整句作为单段经流式路径投递（测试夹具）。
    /// 生产路径由 synth 线程逐段喂入 write_stream，见该函数。
    #[cfg(test)]
    pub fn write_utterance(&mut self, pcm: &[f32], abort: &Abort<'_>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = tx.send(pcm.to_vec());
        drop(tx);
        write_stream(rx, self, abort);
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

/// 按 FRAME_MS 节拍发送一帧并计数。首帧立即发送；其后单调时钟节拍：发送耗时
/// 计入周期，落后时从当前时刻重新起算，不追赶补发。输出线程非实时，睡眠让出 CPU。
fn emit_frame(writer: &mut FrameWriter, frame: &OutputFrame, next: &mut Instant, started: &mut bool) {
    if *started {
        let frame_duration = Duration::from_millis(FRAME_MS as u64);
        *next = (*next).max(Instant::now()) + frame_duration;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    *started = true;
    writer.send_frame(frame);
    writer.shared.add_frames_out(1);
}

/// 流式输出（SPEC-05 合成一段播一段）：从通道收 synth 线程逐段交付的 48 kHz PCM，
/// 收一段即按 FRAME_MS 节拍发帧，不等整句。帧序号与 pts_ns 跨段连续累计，段边界
/// 不足一帧的样本缓存到下一段拼接，句末不足一帧补零。中止判定置真（SIGTERM 或
/// TTS_JOB_STOP）即放弃剩余帧并返回（丢弃接收端，反压使 synth 侧 sink 发送失败、
/// 引擎中止）。通道关闭（synth 完成或中止）后退出。每帧计入 frames_out（含丢弃）。
pub fn write_stream(rx: Receiver<Vec<f32>>, writer: &mut FrameWriter, abort: &Abort<'_>) {
    let mut next = Instant::now();
    let mut started = false;
    let mut frame_idx: u32 = 0;
    let mut pending: Vec<f32> = Vec::new();
    while let Ok(segment) = rx.recv() {
        if abort.aborted() {
            return;
        }
        pending.extend_from_slice(&segment);
        while pending.len() >= OUTPUT_SAMPLES {
            if abort.aborted() {
                return;
            }
            let frame = make_frame(&pending[..OUTPUT_SAMPLES], frame_idx);
            pending.drain(..OUTPUT_SAMPLES);
            emit_frame(writer, &frame, &mut next, &mut started);
            frame_idx += 1;
        }
    }
    // 句末不足一帧补零（与 pack_utterance 一致）。
    if !pending.is_empty() && !abort.aborted() {
        let frame = make_frame(&pending, frame_idx);
        emit_frame(writer, &frame, &mut next, &mut started);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
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
        let abort = shared.abort(&stop);
        writer.write_utterance(&vec![0.25f32; 4800], &abort);
        let status = shared.status();
        assert_eq!(status.frames_out, 5);
        assert_eq!(status.output_dropped, 5, "pre 未绑定：5 帧全部丢弃计数");

        // pre 绑定后下一句自动接通并送达。
        let rx = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        writer.write_utterance(&vec![0.25f32; 960], &abort);
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
        let abort = shared.abort(&stop);
        let pcm: Vec<f32> = (0..1920).map(|i| i as f32 * 0.0001).collect();
        writer.write_utterance(&pcm, &abort);
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
        let abort = shared.abort(&stop);

        let rx1 = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        writer.write_utterance(&vec![0.25f32; 960], &abort);
        let FrameMeta { frame_idx, .. } = rx1.recv(None).unwrap().meta;
        assert_eq!(frame_idx, 0);
        drop(rx1); // socket 文件留在原地。
        writer.write_utterance(&vec![0.25f32; 960], &abort); // 丢弃 + 重连失败。
        assert_eq!(shared.status().output_dropped, 1, "重启窗口的发送必须计入丢弃");

        let rx2 = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        writer.write_utterance(&vec![0.25f32; 960], &abort); // 仍走失败分支：丢弃 + 重连成功。
        writer.write_utterance(&vec![0.25f32; 960], &abort); // 通道已恢复：送达。
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
        let abort = shared.abort(&stop);
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
        writer.write_utterance(&pcm, &abort);
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
        let abort = shared.abort(&stop);
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
        writer.write_utterance(&vec![0.25f32; OUTPUT_SAMPLES * 16], &abort);
        receiver.join().unwrap();
        let frames_out = shared.status().frames_out;
        assert!(frames_out >= 2, "已送达的 2 帧必须计入：{frames_out}");
        assert!(frames_out < 16, "停止后剩余帧必须放弃：{frames_out}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 跨段拼接：段长不对齐帧边界时缓存拼接，帧序号连续，句末不足一帧补零。
    #[test]
    fn stream_buffers_partial_frames_across_segments() {
        let dir = socket_dir("streampartial");
        let path = dir.join("tts-out.sock");
        let rx = FrameSocket::<OutputFrame>::bind(&path, OUTPUT_BUFFER_FRAMES).unwrap();
        let shared = Arc::new(Shared::new());
        let mut writer = FrameWriter::new(path.clone(), shared.clone());
        let stop = no_stop();
        let abort = shared.abort(&stop);

        // 两段各 1000 样本（共 2000）→ 2 整帧（1920）+ 80 样本末帧补零。
        let seg1: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let seg2: Vec<f32> = (1000..2000).map(|i| i as f32).collect();
        let (tx, seg_rx) = std::sync::mpsc::sync_channel(2);
        tx.send(seg1).unwrap();
        tx.send(seg2).unwrap();
        drop(tx);
        write_stream(seg_rx, &mut writer, &abort);

        let f0 = rx.recv(Some(Duration::from_secs(1))).unwrap();
        let f1 = rx.recv(Some(Duration::from_secs(1))).unwrap();
        let f2 = rx.recv(Some(Duration::from_secs(1))).unwrap();
        // packed 字段经按值拷贝/解构读取，不能取引用。
        let FrameMeta { frame_idx: idx0, .. } = f0.meta;
        let FrameMeta { frame_idx: idx1, .. } = f1.meta;
        let FrameMeta { frame_idx: idx2, .. } = f2.meta;
        let p1 = f1.pcm;
        let p2 = f2.pcm;
        assert_eq!(idx0, 0);
        assert_eq!(idx1, 1);
        assert_eq!(idx2, 2);
        // 帧 1 跨段边界：全局 960..1000 来自 seg1，1000..1920 来自 seg2。
        assert_eq!(p1[0], 960.0);
        assert_eq!(p1[40], 1000.0, "seg2 首样本须拼接到帧 1");
        assert_eq!(p1[959], 1919.0);
        // 末帧 80 样本 + 补零。
        assert_eq!(p2[0], 1920.0);
        assert_eq!(p2[79], 1999.0);
        assert!(p2[80..].iter().all(|&v| v == 0.0));
        assert_eq!(shared.status().frames_out, 3);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}