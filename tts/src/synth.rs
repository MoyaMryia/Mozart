//! 合成工作线程：从任务队列取句 → 文本预处理 → PocketTTS 合成 →
//! 重采样 48 kHz → 契约帧输出（v1 不走变声，直接喂 pre 播放）。模型加载失败为
//! 致命错误（进程退出由 daemon 重启）；单句合成失败计数并记录原因，不影响后续
//! 任务。SIGTERM 放弃在途合成与队列剩余。

use crate::ffi::{self, Synthesizer};
use crate::jobs::Job;
use crate::output::FrameWriter;
use crate::resample::{resample_24k_to_48k, TTS_NATIVE_SAMPLE_RATE};
use crate::state::Shared;
use mozart_core::OUTPUT_SAMPLE_RATE;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// 队列轮询步长：让停止标志在百毫秒级生效（daemon 宽限 2 s）。
const QUEUE_POLL: Duration = Duration::from_millis(100);

/// 句尾闭引号/括号：标点归一化时允许终标点出现在它们之前（沿用旧实现）。
const CLOSERS: [char; 5] = ['"', '\'', '”', '’', ')'];

/// 文本预处理（沿用旧 pocket_input_text）：
/// 1) 去首尾空白，".." 与 "…" 折叠为单个 "."；
/// 2) 尾部 ",;:"（可带闭引号/括号）归一为 "."；
/// 3) 无终标点（.!?，可带闭引号/括号）时补 "."。
pub fn precondition(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 1);
    let mut in_dots = false;
    for ch in text.trim().chars() {
        let ch = if ch == '…' { '.' } else { ch };
        if ch == '.' {
            if !in_dots {
                out.push('.');
            }
            in_dots = true;
        } else {
            in_dots = false;
            out.push(ch);
        }
    }
    let core_end = out.trim_end_matches(CLOSERS).len();
    let core = &out[..core_end];
    let stripped = core.trim_end_matches([',', ';', ':']);
    if stripped.len() < core.len() {
        out = format!("{stripped}.{}", &out[core_end..]);
    }
    let core_end = out.trim_end_matches(CLOSERS).len();
    if !out[..core_end].ends_with(['.', '?', '!']) {
        out.push('.');
    }
    out
}

/// 音色参考音频：启动时从配置 WAV 加载，单声道 f32。
#[derive(Debug, Clone)]
pub struct ReferenceAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// 加载参考 WAV。路径由配置给出；读取或格式失败返回 Err（启动路径据此退出）。
pub fn load_reference_wav(path: &Path) -> Result<ReferenceAudio, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("读取 {}：{err}", path.display()))?;
    parse_wav(&bytes).map_err(|err| format!("{}：{err}", path.display()))
}

/// 解析 RIFF/WAVE：仅支持 16-bit PCM 与 32-bit float；多声道取第一声道。
fn parse_wav(bytes: &[u8]) -> Result<ReferenceAudio, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("不是 RIFF/WAVE 文件".to_string());
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let start = pos + 8;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "chunk 越界".to_string())?;
        match id {
            b"fmt " => {
                if size < 16 {
                    return Err("fmt chunk 过短".to_string());
                }
                let le16 = |off: usize| u16::from_le_bytes(bytes[start + off..start + off + 2].try_into().unwrap());
                let le32 = |off: usize| u32::from_le_bytes(bytes[start + off..start + off + 4].try_into().unwrap());
                fmt = Some((le16(0), le16(2), le32(4), le16(14)));
            }
            b"data" => data = Some(&bytes[start..end]),
            _ => {}
        }
        pos = end + (size & 1);
    }
    let (format, channels, rate, bits) = fmt.ok_or_else(|| "缺少 fmt chunk".to_string())?;
    let data = data.ok_or_else(|| "缺少 data chunk".to_string())?;
    if channels == 0 || rate == 0 {
        return Err("声道数或采样率为零".to_string());
    }
    let samples: Vec<f32> = match (format, bits) {
        (1, 16) => data
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes(b.try_into().unwrap()) as f32 / 32768.0)
            .collect(),
        (3, 32) => data
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect(),
        _ => {
            return Err(format!(
                "不支持的 WAV 格式 format={format} bits={bits}（仅 16-bit PCM 与 32-bit float）"
            ))
        }
    };
    let mono: Vec<f32> = if channels == 1 {
        samples
    } else {
        samples.into_iter().step_by(channels as usize).collect()
    };
    if mono.is_empty() {
        return Err("参考音频为空".to_string());
    }
    Ok(ReferenceAudio {
        samples: mono,
        sample_rate: rate,
    })
}

/// 原生采样率 → 48 kHz 播放采样率。24 kHz 线性插值升采样；48 kHz 直通；其余拒绝。
fn to_48k(samples: Vec<f32>, rate: u32) -> Result<Vec<f32>, String> {
    match rate {
        OUTPUT_SAMPLE_RATE => Ok(samples),
        TTS_NATIVE_SAMPLE_RATE => Ok(resample_24k_to_48k(&samples)),
        _ => Err(format!("不支持的 TTS 采样率 {rate} Hz")),
    }
}

/// 工作线程入口：加载引擎（失败致命）后进入服务循环。
/// stop 为进程级 static 标志：真实 FFI 的生成回调跨 C 边界持有它。
pub fn run(
    jobs: Receiver<Job>,
    model_dir: PathBuf,
    threads: u32,
    provider: String,
    reference: Option<ReferenceAudio>,
    writer: &mut FrameWriter,
    shared: &Shared,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let engine = ffi::load(&model_dir, threads, &provider, stop)?;
    shared.set_model_loaded(true);
    run_with(jobs, engine.as_ref(), reference.as_ref(), writer, shared, stop);
    Ok(())
}

/// 服务循环（引擎可注入，供测试）。停止标志置位后放弃在途结果与队列剩余；
/// 真实 FFI 的在途生成另经进度回调中止（见 ffi::real）。
pub fn run_with(
    jobs: Receiver<Job>,
    engine: &dyn Synthesizer,
    reference: Option<&ReferenceAudio>,
    writer: &mut FrameWriter,
    shared: &Shared,
    stop: &AtomicBool,
) {
    let (ref_samples, ref_rate) = match reference {
        Some(reference) => (reference.samples.as_slice(), reference.sample_rate),
        None => (&[][..], 0),
    };
    while !stop.load(Relaxed) {
        let job = match jobs.recv_timeout(QUEUE_POLL) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        shared.queue_pop();
        if stop.load(Relaxed) {
            break;
        }
        shared.set_synthesizing(true);
        let text = precondition(&job.text);
        let result = engine
            .synthesize(&text, ref_samples, ref_rate)
            .and_then(|samples| to_48k(samples, engine.sample_rate()));
        match result {
            Ok(pcm) => {
                if !stop.load(Relaxed) {
                    writer.write_utterance(&pcm);
                    shared.add_synthesized(1);
                }
                // 停止标志置位：在途结果丢弃，不投递。
            }
            Err(reason) => {
                shared.add_failed(1);
                shared.set_last_error(&format!("{}: {reason}", job.utterance_id));
            }
        }
        shared.set_synthesizing(false);
    }
    shared.set_synthesizing(false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mozart_core::dgram::FrameSocket;
    use mozart_core::OutputFrame;
    use std::sync::mpsc;
    use std::sync::Arc;

    #[test]
    fn precondition_normalizes_punctuation() {
        let cases = [
            ("Hello world", "Hello world."),
            ("  Hello  ", "Hello."),
            ("Hello..", "Hello."),
            ("Hello...", "Hello."),
            ("Hello…", "Hello."),
            ("Hello……", "Hello."),
            ("Hello…world", "Hello.world."),
            ("Hello,", "Hello."),
            ("Hello;;", "Hello."),
            ("Hello:", "Hello."),
            ("Hello.", "Hello."),
            ("Hello?", "Hello?"),
            ("Hello!", "Hello!"),
            ("Hello... world", "Hello. world."),
            ("He said \"hi\",", "He said \"hi\"."),
            ("He said \"hi\"", "He said \"hi\"."),
        ];
        for (input, expected) in cases {
            assert_eq!(precondition(input), expected, "输入 {input:?}");
        }
    }

    /// 构造最小 WAV 字节（fmt + data）。
    fn wav_bytes(format: u16, bits: u16, channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&format.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * channels as u32 * bits as u32 / 8).to_le_bytes());
        out.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn wav_pcm16_roundtrip() {
        let samples = [0i16, 16384, -16384, 32767];
        let data: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
        let wav = wav_bytes(1, 16, 1, 24000, &data);
        let reference = parse_wav(&wav).unwrap();
        assert_eq!(reference.sample_rate, 24000);
        assert_eq!(reference.samples.len(), 4);
        assert_eq!(reference.samples[0], 0.0);
        assert!((reference.samples[1] - 0.5).abs() < 1e-4);
        assert!((reference.samples[2] + 0.5).abs() < 1e-4);
    }

    #[test]
    fn wav_float32_and_stereo_first_channel() {
        let left = [0.25f32, -0.75];
        let mut data = Vec::new();
        for v in left {
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&0.5f32.to_le_bytes());
        }
        let wav = wav_bytes(3, 32, 2, 16000, &data);
        let reference = parse_wav(&wav).unwrap();
        assert_eq!(reference.samples, left);
        assert_eq!(reference.sample_rate, 16000);
    }

    #[test]
    fn wav_rejects_bad_input() {
        assert!(parse_wav(b"not a wav").is_err());
        assert!(parse_wav(&wav_bytes(1, 16, 1, 24000, &[])).is_err()); // 空 data
        assert!(parse_wav(&wav_bytes(1, 8, 1, 24000, &[0])).is_err()); // 8-bit 不支持
        assert!(parse_wav(&wav_bytes(1, 16, 0, 24000, &[0, 0])).is_err()); // 零声道
        let mut truncated = wav_bytes(1, 16, 1, 24000, &[0; 8]);
        truncated.truncate(truncated.len() - 4);
        assert!(parse_wav(&truncated).is_err()); // chunk 越界
    }

    // ---- 工作循环（假引擎注入） ----

    struct FakeEngine<'a> {
        rate: u32,
        outcome: Result<Vec<f32>, String>,
        hook: Option<Box<dyn Fn() + Send + 'a>>,
    }

    impl Synthesizer for FakeEngine<'_> {
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn synthesize(&self, _text: &str, _reference: &[f32], _ref_rate: u32) -> Result<Vec<f32>, String> {
            if let Some(hook) = &self.hook {
                hook();
            }
            self.outcome.clone()
        }
    }

    /// 临时输出 socket + 接收方 + writer + shared。接收方先绑定（模拟 pre），
    /// writer 惰性建链后逐帧送达；_rx 保持存活到用例结束。
    fn writer_fixture(tag: &str) -> (PathBuf, FrameWriter, Arc<Shared>, FrameSocket<OutputFrame>) {
        let dir = std::env::temp_dir().join(format!("mozart-tts-synth-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.sock");
        let rx = FrameSocket::<OutputFrame>::bind(&path, 8).unwrap();
        let shared = Arc::new(Shared::new());
        let writer = FrameWriter::new(path.clone(), shared.clone());
        (path, writer, shared, rx)
    }

    fn cleanup(path: &Path) {
        std::fs::remove_file(path).unwrap();
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    fn job(id: &str) -> Job {
        Job { text: "Hello world".to_string(), utterance_id: id.to_string() }
    }

    #[test]
    fn worker_synthesizes_resamples_and_outputs() {
        let (path, mut writer, shared, _rx) = writer_fixture("ok");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        let engine = FakeEngine { rate: TTS_NATIVE_SAMPLE_RATE, outcome: Ok(vec![0.25; 2400]), hook: None };
        tx.send(job("a:1")).unwrap();
        tx.send(job("a:2")).unwrap();
        shared.queue_push();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.synthesized, 2);
        assert_eq!(status.failed, 0);
        assert_eq!(status.queue_depth, 0);
        assert_eq!(status.frames_out, 10);
        assert_eq!(status.state, "idle");
        cleanup(&path);
    }

    #[test]
    fn worker_failure_counts_and_records_reason() {
        let (path, mut writer, shared, _rx) = writer_fixture("fail");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        let engine = FakeEngine { rate: TTS_NATIVE_SAMPLE_RATE, outcome: Err("boom".to_string()), hook: None };
        tx.send(job("a:9")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.synthesized, 0);
        assert_eq!(status.failed, 1);
        assert_eq!(status.frames_out, 0);
        assert_eq!(status.last_error.as_deref(), Some("a:9: boom"));
        cleanup(&path);
    }

    #[test]
    fn worker_rejects_unsupported_rate() {
        let (path, mut writer, shared, _rx) = writer_fixture("rate");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        let engine = FakeEngine { rate: 44100, outcome: Ok(vec![0.25; 4410]), hook: None };
        tx.send(job("a:1")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.failed, 1);
        assert!(status.last_error.unwrap().contains("44100"));
        cleanup(&path);
    }

    #[test]
    fn worker_passes_through_48k() {
        let (path, mut writer, shared, _rx) = writer_fixture("48k");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        let engine = FakeEngine { rate: OUTPUT_SAMPLE_RATE, outcome: Ok(vec![0.25; 960]), hook: None };
        tx.send(job("a:1")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.synthesized, 1);
        assert_eq!(status.frames_out, 1);
        cleanup(&path);
    }

    #[test]
    fn stop_during_synthesis_discards_result() {
        let (path, mut writer, shared, _rx) = writer_fixture("stop");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        let engine = FakeEngine {
            rate: TTS_NATIVE_SAMPLE_RATE,
            outcome: Ok(vec![0.25; 2400]),
            hook: Some(Box::new(|| stop.store(true, Relaxed))),
        };
        tx.send(job("a:1")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.synthesized, 0);
        assert_eq!(status.frames_out, 0);
        assert_eq!(status.state, "idle");
        cleanup(&path);
    }

    #[test]
    fn stop_before_run_abandons_queue() {
        let (path, mut writer, shared, _rx) = writer_fixture("prestop");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(true);
        let engine = FakeEngine { rate: TTS_NATIVE_SAMPLE_RATE, outcome: Ok(vec![0.25; 2400]), hook: None };
        tx.send(job("a:1")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.synthesized, 0);
        assert_eq!(status.frames_out, 0);
        cleanup(&path);
    }
}