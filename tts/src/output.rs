//! 播报音频输出：16 kHz PCM 打包为 InputFrame（320 samples / 20 ms）经
//! tts-out.sock 发送。v1 由 TTS 绑定该 socket（接收方接 RVC 留 FUTURE），
//! 无接收方时非阻塞发送全部丢弃并计数，不阻塞合成路径。

use crate::state::Shared;
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, InputFrame, FRAME_MS, INPUT_SAMPLES};
use std::io;
use std::path::Path;
use std::sync::Arc;

/// 输出 socket 缓冲帧数（SPEC-02 数据面上限 4–8 帧取上界）。
const OUTPUT_BUFFER_FRAMES: usize = 8;

/// 把整句 16 kHz PCM 打包为契约帧：每帧 320 samples，末帧不足补零。
/// meta：pts_ns 与 frame_idx 按句内 20 ms 步进，vad_flag 恒 1（合成语音），
/// energy_db/conf/segment_id 置 0（播报帧不做能量/置信度标注）。
pub fn pack_utterance(pcm: &[f32]) -> Vec<InputFrame> {
    pcm.chunks(INPUT_SAMPLES)
        .enumerate()
        .map(|(idx, chunk)| {
            // packed 字段不能取引用：先在本地数组填充，再按值赋入。
            let mut pcm = [0f32; INPUT_SAMPLES];
            pcm[..chunk.len()].copy_from_slice(chunk);
            InputFrame {
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

/// 输出端：独占绑定的帧 socket，synth 线程逐句写入。
pub struct FrameWriter {
    socket: FrameSocket<InputFrame>,
    shared: Arc<Shared>,
}

impl FrameWriter {
    pub fn bind(path: &Path, shared: Arc<Shared>) -> io::Result<Self> {
        Ok(Self {
            socket: FrameSocket::<InputFrame>::bind(path, OUTPUT_BUFFER_FRAMES)?,
            shared,
        })
    }

    /// 发送整句帧。丢弃由 FrameSocket 计数；意外错误记录原因后继续，
    /// 不打断合成路径（降级经 GetStatus 可查）。
    pub fn write_utterance(&mut self, pcm: &[f32]) {
        for frame in pack_utterance(pcm) {
            if let Err(err) = self.socket.send(&frame) {
                let message = format!("输出帧发送失败：{err}");
                eprintln!("mozart-tts: {message}");
                self.shared.set_last_error(&message);
            }
            self.shared.add_frames_out(1);
        }
        self.shared
            .set_output_dropped(self.socket.stats().send_dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_full_frames_meta_and_pcm() {
        let pcm: Vec<f32> = (0..640).map(|i| i as f32 * 0.001).collect();
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
            assert_eq!(&chunk[..], &pcm[idx * 320..(idx + 1) * 320]);
        }
    }

    #[test]
    fn pack_pads_partial_last_frame_with_zeros() {
        let pcm = vec![0.5f32; 321];
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

    /// v1 绑定侧无接收方：发送全部丢弃计数，不阻塞、不报错。
    #[test]
    fn bind_side_send_drops_and_counts() {
        let dir = std::env::temp_dir().join(format!("mozart-tts-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tts-out.sock");
        let shared = Arc::new(Shared::new());
        let mut writer = FrameWriter::bind(&path, shared.clone()).unwrap();
        writer.write_utterance(&vec![0.25f32; 1600]);
        let status = shared.status();
        assert_eq!(status.frames_out, 5);
        assert_eq!(status.output_dropped, 5);
        drop(writer);
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }
}