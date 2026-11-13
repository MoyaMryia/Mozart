//! 文件采集源（SPEC-02 测试注入）：读 WAV 替代 ALSA 采集，播放仍走 ALSA。
//! 契约：PCM16、48000 Hz、至少 1 声道，取首声道（与设备采集取 FL 一致）；
//! 启动时入口校验一次，不合契约报错退出。按实时节拍每 20 ms 发射一帧，
//! 文件播完循环；下游 DSP 与分发链路不变。

use crate::capture::{now_ns, S16_NORM};
use crate::state::Shared;
use mozart_core::ring::Ring;
use mozart_core::{FrameMeta, RawFrame, FRAME_MS, RAW_SAMPLES, RAW_SAMPLE_RATE};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 帧节拍：由契约帧时长推导（core 单一定义源）。
const FRAME_PERIOD_NS: u64 = FRAME_MS as u64 * 1_000_000;
/// 等待发射截止时间的单次睡眠上限，保证停止标志的响应延迟有界。
const SLEEP_STEP: Duration = Duration::from_millis(5);

// 调用方已保证偏移处字节齐全（块头循环条件、fmt 长度检查），直接读取。
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// 加载 WAV 并提取首声道样本。契约校验只在此入口做一次：
/// RIFF/WAVE 结构、PCM16、48000 Hz、至少 1 声道、数据非空。
/// 末尾不足一个采样块的字节忽略。
pub fn load_wav(path: &Path) -> Result<Vec<i16>, String> {
    let bytes = std::fs::read(path)
        .map_err(|err| format!("capture file {}: {err}", path.display()))?;
    let fail = |reason: &str| format!("capture file {}: {reason}", path.display());

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(fail("not a RIFF/WAVE file"));
    }

    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32_at(&bytes, offset + 4) as usize;
        let body_start = offset + 8;
        let body_end = body_start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| fail("truncated chunk body"))?;
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(fail("fmt chunk too short"));
                }
                fmt = Some((
                    u16_at(body, 0),
                    u16_at(body, 2),
                    u32_at(body, 4),
                    u16_at(body, 14),
                ));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // RIFF 块按字对齐：奇数长度补一个填充字节。
        offset = body_end + (size & 1);
    }

    let (format, channels, rate, bits) = fmt.ok_or_else(|| fail("missing fmt chunk"))?;
    if format != 1 {
        return Err(fail("not PCM (format tag must be 1)"));
    }
    if bits != 16 {
        return Err(fail("sample width must be 16-bit"));
    }
    if rate != RAW_SAMPLE_RATE {
        return Err(fail("sample rate must be 48000 Hz"));
    }
    if channels == 0 {
        return Err(fail("channel count must be at least 1"));
    }
    let data = data.ok_or_else(|| fail("missing data chunk"))?;
    if data.is_empty() {
        return Err(fail("empty data chunk"));
    }

    let block = channels as usize * 2;
    let samples: Vec<i16> = data
        .chunks_exact(block)
        .map(|frame| i16::from_le_bytes([frame[0], frame[1]]))
        .collect();
    // 数据非空但不足一个采样块时 chunks_exact 产出空序列，
    // 必须在入口拦住，否则采集线程首帧越界。
    if samples.is_empty() {
        return Err(fail("data shorter than one sample block"));
    }
    Ok(samples)
}

/// 文件采集线程主体：启动时加载并校验文件，随后按实时节拍攒 960 样本
/// 入队，播完循环。返回 Err 表示契约或 I/O 错误，主线程据此退出进程，
/// daemon 按退避重启（与设备打开失败同语义）。
pub fn run(
    path: &Path,
    ring: Arc<Ring<RawFrame>>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let samples = load_wav(path).map_err(|err| {
        shared.set_last_error(&err);
        err
    })?;
    eprintln!(
        "mozart-pre: capture file {} ready ({} samples, loop)",
        path.display(),
        samples.len()
    );

    let mut block = [0f32; RAW_SAMPLES];
    let mut filled = 0usize;
    let mut cursor = 0usize;
    let mut frame_idx: u32 = 0;
    let mut emitted: u64 = 0;
    let start = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        while filled < RAW_SAMPLES {
            block[filled] = samples[cursor] as f32 / S16_NORM;
            filled += 1;
            cursor += 1;
            if cursor == samples.len() {
                cursor = 0;
            }
        }
        filled = 0;
        let frame = RawFrame {
            meta: FrameMeta {
                pts_ns: now_ns(),
                frame_idx,
                vad_flag: 0,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm: block,
        };
        ring.push(frame);
        frame_idx = frame_idx.wrapping_add(1);
        shared.add_captured(1);
        emitted += 1;

        let deadline = start + Duration::from_nanos(emitted * FRAME_PERIOD_NS);
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep((deadline - now).min(SLEEP_STEP));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 WAV 字节：可指定格式标签、声道数、采样率、位深与数据，
    /// 用于覆盖合法与各类非法输入。
    struct WavBuilder {
        format: u16,
        channels: u16,
        rate: u32,
        bits: u16,
        data: Vec<u8>,
        extra_chunk: bool,
        odd_chunk: bool,
    }

    impl WavBuilder {
        fn mono(samples: &[i16]) -> Self {
            Self {
                format: 1,
                channels: 1,
                rate: 48_000,
                bits: 16,
                data: samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
                extra_chunk: false,
                odd_chunk: false,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            let mut fmt = Vec::new();
            fmt.extend_from_slice(&self.format.to_le_bytes());
            fmt.extend_from_slice(&self.channels.to_le_bytes());
            fmt.extend_from_slice(&self.rate.to_le_bytes());
            fmt.extend_from_slice(&48_000u32.to_le_bytes()); // byte rate（解析不校验）
            fmt.extend_from_slice(&(self.channels * self.bits / 8).to_le_bytes());
            fmt.extend_from_slice(&self.bits.to_le_bytes());

            let mut body = Vec::new();
            // 未知块在前：验证跳过逻辑；奇数长度验证字对齐填充。
            if self.extra_chunk {
                let payload: &[u8] = if self.odd_chunk { b"xyz" } else { b"xy" };
                body.extend_from_slice(b"LIST");
                body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                body.extend_from_slice(payload);
                if payload.len() % 2 == 1 {
                    body.push(0);
                }
            }
            body.extend_from_slice(b"fmt ");
            body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
            body.extend_from_slice(&fmt);
            body.extend_from_slice(b"data");
            body.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
            body.extend_from_slice(&self.data);

            let mut out = Vec::new();
            out.extend_from_slice(b"RIFF");
            out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
            out.extend_from_slice(b"WAVE");
            out.extend_from_slice(&body);
            out
        }
    }

    fn temp_wav(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mozart-filecap-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("input.wav");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn ramp(count: usize) -> Vec<i16> {
        (0..count).map(|i| (i as i16 % 1000) * 3).collect()
    }

    #[test]
    fn mono_roundtrip() {
        let samples = ramp(960);
        let path = temp_wav("mono", &WavBuilder::mono(&samples).bytes());
        assert_eq!(load_wav(&path).unwrap(), samples);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn stereo_takes_first_channel() {
        // 与 ALSA 采集取 FL 一致：右声道任意值不影响输出。
        let mut data = Vec::new();
        for i in 0..4i16 {
            data.extend_from_slice(&i.to_le_bytes());
            data.extend_from_slice(&(-9999i16).to_le_bytes());
        }
        let wav = WavBuilder {
            channels: 2,
            data,
            ..WavBuilder::mono(&[])
        };
        let path = temp_wav("stereo", &wav.bytes());
        assert_eq!(load_wav(&path).unwrap(), vec![0, 1, 2, 3]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unknown_chunks_and_odd_padding_skipped() {
        let samples = ramp(48);
        let wav = WavBuilder {
            extra_chunk: true,
            odd_chunk: true,
            ..WavBuilder::mono(&samples)
        };
        let path = temp_wav("chunks", &wav.bytes());
        assert_eq!(load_wav(&path).unwrap(), samples);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn trailing_partial_sample_block_ignored() {
        let samples = ramp(5);
        let mut wav = WavBuilder::mono(&samples);
        wav.data.push(0x7f); // 末尾半个采样
        let path = temp_wav("partial", &wav.bytes());
        assert_eq!(load_wav(&path).unwrap(), samples);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_invalid_inputs() {
        let good = WavBuilder::mono(&ramp(16)).bytes();
        // 逐例断言拒绝原因：守卫必须在它声称要拦住的错误上失败。
        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            ("empty", Vec::new(), "RIFF/WAVE"),
            ("short-header", good[..8].to_vec(), "RIFF/WAVE"),
            ("bad-magic", {
                let mut b = good.clone();
                b[0] = b'X';
                b
            }, "RIFF/WAVE"),
            ("not-wave", {
                let mut b = good.clone();
                b[8..12].copy_from_slice(b"AVI ");
                b
            }, "RIFF/WAVE"),
            ("float-format", {
                let mut w = WavBuilder::mono(&ramp(16));
                w.format = 3;
                w.bytes()
            }, "PCM"),
            ("8-bit", {
                let mut w = WavBuilder::mono(&ramp(16));
                w.bits = 8;
                w.bytes()
            }, "16-bit"),
            ("rate-16k", {
                let mut w = WavBuilder::mono(&ramp(16));
                w.rate = 16_000;
                w.bytes()
            }, "48000"),
            ("rate-44k", {
                let mut w = WavBuilder::mono(&ramp(16));
                w.rate = 44_100;
                w.bytes()
            }, "48000"),
            ("zero-channels", {
                let mut w = WavBuilder::mono(&ramp(16));
                w.channels = 0;
                w.bytes()
            }, "channel count"),
            ("empty-data", WavBuilder::mono(&[]).bytes(), "empty data"),
            ("data-one-byte", {
                let mut w = WavBuilder::mono(&[]);
                w.data = vec![0x7f];
                w.bytes()
            }, "one sample block"),
            ("data-less-than-block", {
                let mut w = WavBuilder::mono(&[]);
                w.channels = 3;
                w.data = vec![0; 4];
                w.bytes()
            }, "one sample block"),
            ("missing-fmt", {
                // 只留 data 块：手工拼 RIFF 头 + data。
                let mut body = Vec::new();
                body.extend_from_slice(b"data");
                body.extend_from_slice(&4u32.to_le_bytes());
                body.extend_from_slice(&[0u8; 4]);
                let mut out = Vec::new();
                out.extend_from_slice(b"RIFF");
                out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
                out.extend_from_slice(b"WAVE");
                out.extend_from_slice(&body);
                out
            }, "missing fmt"),
            ("missing-data", {
                let mut b = good.clone();
                let data_pos = b.len() - 8 - 32; // data 头在末尾 32 字节数据前
                b.truncate(data_pos);
                b
            }, "missing data"),
            ("truncated-body", {
                let mut b = good.clone();
                b.truncate(b.len() - 10);
                b
            }, "truncated chunk"),
            ("short-fmt", {
                let mut b = good.clone();
                // fmt 块长度字段改成 8：块体不足 16 字节。
                let fmt_pos = 12 + 4; // 无 extra chunk 时 fmt 紧跟 WAVE 头
                b[fmt_pos..fmt_pos + 4].copy_from_slice(&8u32.to_le_bytes());
                b
            }, "fmt chunk too short"),
        ];
        for (tag, bytes, reason) in cases {
            let path = temp_wav(tag, &bytes);
            let err = load_wav(&path).expect_err(tag);
            assert!(err.contains(reason), "{tag}: 期望「{reason}」，实际「{err}」");
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }

    #[test]
    fn missing_file_is_error() {
        let path = std::env::temp_dir().join("mozart-filecap-absent.wav");
        let _ = std::fs::remove_file(&path);
        assert!(load_wav(&path).is_err());
    }

    /// 发射值、帧序号、循环回绕与实时节拍：2 帧内容的文件发 5 帧，
    /// 第 3 帧起回绕重复；5 帧至少耗时 4 个节拍（80 ms）。
    #[test]
    fn run_emits_paced_looping_frames() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let samples = ramp(RAW_SAMPLES * 2);
        let path = temp_wav("run", &WavBuilder::mono(&samples).bytes());
        let ring = Arc::new(Ring::<RawFrame>::new(8));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string()));

        // 计时起点先于线程启动：run 内部起点不早于此，下界断言无竞态。
        let started = Instant::now();
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            let path = path.clone();
            std::thread::spawn(move || run(&path, ring, shared, &STOP))
        };

        let mut frames = Vec::new();
        for _ in 0..5 {
            frames.push(ring.pop(Some(Duration::from_secs(2))).expect("帧按时到达"));
        }
        let elapsed = started.elapsed();
        STOP.store(true, Ordering::Relaxed);
        let joined = worker.join().unwrap();
        assert!(joined.is_ok(), "停止后应正常返回：{joined:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        for (idx, frame) in frames.iter().enumerate() {
            // packed 结构字段先按值拷贝，不取引用（C 契约镜像）。
            let meta_idx = frame.meta.frame_idx;
            assert_eq!(meta_idx, idx as u32);
            let pcm = frame.pcm;
            let base = (idx % 2) * RAW_SAMPLES;
            for (i, sample) in pcm.iter().enumerate() {
                let expect = samples[base + i] as f32 / 32768.0;
                assert_eq!(*sample, expect, "帧 {idx} 样本 {i}");
            }
        }
        assert!(
            elapsed >= Duration::from_millis(80),
            "实时节拍不得快于 20 ms/帧：{elapsed:?}"
        );
    }

    /// 单样本文件：回绕发生在块内，每帧全部样本同值。
    #[test]
    fn single_sample_wraps_inside_block() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let path = temp_wav("one", &WavBuilder::mono(&[12345]).bytes());
        let ring = Arc::new(Ring::<RawFrame>::new(4));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string()));
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            let path = path.clone();
            std::thread::spawn(move || run(&path, ring, shared, &STOP))
        };
        let frame = ring.pop(Some(Duration::from_secs(2))).expect("首帧");
        STOP.store(true, Ordering::Relaxed);
        worker.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let pcm = frame.pcm;
        assert!(pcm.iter().all(|s| *s == 12345.0 / 32768.0));
    }

    /// 非整帧文件：回绕跨块边界，第二帧首样本接上文件末尾之后回到开头。
    #[test]
    fn partial_frame_wraps_across_blocks() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let samples = ramp(RAW_SAMPLES + 100);
        let path = temp_wav("wrap", &WavBuilder::mono(&samples).bytes());
        let ring = Arc::new(Ring::<RawFrame>::new(4));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string()));
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            let path = path.clone();
            std::thread::spawn(move || run(&path, ring, shared, &STOP))
        };
        let first = ring.pop(Some(Duration::from_secs(2))).expect("首帧");
        let second = ring.pop(Some(Duration::from_secs(2))).expect("次帧");
        STOP.store(true, Ordering::Relaxed);
        worker.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let first_pcm = first.pcm;
        let second_pcm = second.pcm;
        assert_eq!(first_pcm[0], samples[0] as f32 / 32768.0);
        // 第二帧前 100 样本是文件尾部，其后回绕到文件开头。
        assert_eq!(second_pcm[0], samples[RAW_SAMPLES] as f32 / 32768.0);
        assert_eq!(second_pcm[100], samples[0] as f32 / 32768.0);
    }

    /// 契约失败：返回 Err 且原因写入 last_error 供 GetStatus 查询。
    #[test]
    fn invalid_file_reports_queryable_error() {
        use crate::state::QueueReadout;
        static STOP: AtomicBool = AtomicBool::new(false);
        let path = temp_wav("bad", b"not a wav at all");
        let ring = Arc::new(Ring::<RawFrame>::new(4));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string()));
        let err = run(&path, ring, shared.clone(), &STOP).expect_err("契约失败必须报错");
        assert!(err.contains("capture file"));
        let status = shared.status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
        });
        assert_eq!(status.last_error.as_deref(), Some(err.as_str()));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 停止响应：置位后在一个睡眠步长量级内退出，不等满节拍。
    #[test]
    fn stop_responds_within_sleep_step() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let path = temp_wav("stop", &WavBuilder::mono(&ramp(RAW_SAMPLES)).bytes());
        let ring = Arc::new(Ring::<RawFrame>::new(4));
        let shared = Arc::new(Shared::new("cap".to_string(), "play".to_string()));
        let worker = {
            let ring = ring.clone();
            let shared = shared.clone();
            let path = path.clone();
            std::thread::spawn(move || run(&path, ring, shared, &STOP))
        };
        ring.pop(Some(Duration::from_secs(2))).expect("首帧");
        let marked = Instant::now();
        STOP.store(true, Ordering::Relaxed);
        worker.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        assert!(
            marked.elapsed() < Duration::from_millis(50),
            "停止响应必须有界：{:?}",
            marked.elapsed()
        );
    }
}