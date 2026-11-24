//! 合成工作线程：从任务队列取句 → 文本预处理 → PocketTTS 流式合成（生成回调每段
//! 交付 PCM）→ 逐段重采样 48 kHz → 经有界通道交输出线程按节拍发契约帧（v1 不走
//! 变声，直接喂 pre 播放，合成一段播一段）。模型加载失败为致命错误（进程退出由
//! daemon 重启）；单句合成失败计数并记录原因，不影响后续任务。SIGTERM 放弃在途
//! 合成与队列剩余。

use crate::ffi::{self, Synthesizer};
use crate::jobs::Job;
use crate::output::FrameWriter;
use crate::resample::{Upsample2, TTS_NATIVE_SAMPLE_RATE};
use crate::state::Shared;
use mozart_core::OUTPUT_SAMPLE_RATE;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

/// 队列轮询步长：让停止标志在百毫秒级生效（daemon 宽限 2 s）。
const QUEUE_POLL: Duration = Duration::from_millis(100);

/// 合成段通道容量（段数）：输出线程按实时节拍消费，通道满时 sink 阻塞反压，
/// 把引擎解码节流到播放速率，内存上限约 容量 × 单段时长 × 48 kHz × 4 字节。
const SEG_BUFFER: usize = 4;

/// 每句时长上限（沿用旧 speech_service.py:520，中文按字符）：
/// min(45, max(4, 字符数 × 0.35 + 2)) 秒。超限拒绝计 failed。
const DURATION_CAP_MAX_MS: usize = 45_000;
const DURATION_CAP_MIN_MS: usize = 4_000;
const DURATION_PER_CHAR_MS: usize = 350;
const DURATION_BASE_MS: usize = 2_000;
/// 引擎幅度安全上限（沿用旧 clone_worker.py:201-203）：peak > 4 拒绝。
const PEAK_LIMIT: f32 = 4.0;
/// 归一化目标峰值（沿用旧 clone_worker.py:204）：gain = min(1, 0.98 / peak)。
const TARGET_PEAK: f32 = 0.98;

fn duration_cap_samples(text_chars: usize) -> usize {
    let ms = text_chars.saturating_mul(DURATION_PER_CHAR_MS)
        .saturating_add(DURATION_BASE_MS)
        .clamp(DURATION_CAP_MIN_MS, DURATION_CAP_MAX_MS);
    ms * OUTPUT_SAMPLE_RATE as usize / 1000
}

/// 合成结果安全校验与归一化（沿用旧 clone_worker.py:195-211）：
/// 空样本、非有限样本（NaN/Inf）、时长超限、peak > 4 一律拒绝（计 failed）；
/// 否则按句内单调不增增益（`running_gain = min(*running_gain, min(1, 0.98 / peak))`）
/// 原地缩放，避免前段压低增益后后续段跳回 1.0。
fn sanitize_segment(
    pcm: &mut [f32],
    sample_rate: u32,
    remaining: &mut usize,
    running_gain: &mut f32,
) -> Result<(), String> {
    if pcm.is_empty() {
        return Err("引擎返回空音频".to_string());
    }
    if !pcm.iter().all(|v| v.is_finite()) {
        return Err("引擎返回非有限音频（NaN/Inf）".to_string());
    }
    if pcm.len() > *remaining {
        let rate = sample_rate as f64;
        let segment = pcm.len() as f64 / rate;
        let budget = *remaining as f64 / rate;
        return Err(format!("合成段时长 {segment:.3}s 超上限 {budget:.3}s（本句剩余），拒绝"));
    }
    let peak = pcm.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak > PEAK_LIMIT {
        return Err(format!("引擎幅度峰值 {peak:.2} 超安全范围 {PEAK_LIMIT}，拒绝"));
    }
    let seg_gain = if peak > 0.0 {
        (TARGET_PEAK / peak).min(1.0)
    } else {
        1.0
    };
    *running_gain = (*running_gain).min(seg_gain);
    for v in pcm.iter_mut() {
        *v *= *running_gain;
    }
    *remaining -= pcm.len();
    Ok(())
}

/// 单段安全校验与归一化便利入口（供单元测试复用）。
#[cfg(test)]
fn sanitize(pcm: &mut [f32], sample_rate: u32, cap_s: f64) -> Result<(), String> {
    let mut running_gain = 1.0;
    let mut remaining = (cap_s * sample_rate as f64) as usize;
    sanitize_segment(pcm, sample_rate, &mut remaining, &mut running_gain)
}

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
    let engine = ffi::load(&model_dir, threads, &provider)?;
    shared.set_model_loaded(true);
    run_with(jobs, engine.as_ref(), reference.as_ref(), writer, shared, stop);
    Ok(())
}

/// 服务循环（引擎可注入，供测试）。进程停止标志置位后放弃在途结果与队列剩余，
/// 帧投递中途同样中断；TTS_JOB_STOP 推进任务代际，在途合成经 Abort 中止、在队
/// 任务下一轮清空（真实 FFI 的在途生成另经进度回调中止，见 ffi::real）。
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
        let abort = shared.abort_job(stop, job.issued_ns);
        if abort.aborted() { shared.add_stopped(1); continue; }
        shared.set_synthesizing(true);
        let text = precondition(&job.text);
        let budget = duration_cap_samples(job.text.chars().count());
        let rate = engine.sample_rate();
        let upsample = match rate {
            OUTPUT_SAMPLE_RATE => false,
            TTS_NATIVE_SAMPLE_RATE => true,
            _ => {
                shared.add_failed(1);
                shared.set_last_error(&format!("{}: 不支持的 TTS 采样率 {rate} Hz", job.utterance_id));
                shared.set_synthesizing(false);
                continue;
            }
        };

        // 流式：synth 线程经生成回调逐段交付，sink 重采样 + 安全校验后送有界通道；
        //  scoped 输出线程独占 writer 按节拍发帧。通道满时 sink 阻塞反压引擎解码。
        let (seg_tx, seg_rx) = mpsc::sync_channel::<Vec<f32>>(SEG_BUFFER);
        // 用累计样本核算时长，包括句末插值样本。句内增益单调不增。
        let mut remaining_samples = budget;
        let mut running_gain = 1.0f32;
        let mut total_samples = 0usize;
        let mut stream_err: Option<String> = None;

        let result = std::thread::scope(|scope| {
            let output = scope.spawn(|| crate::output::write_stream(seg_rx, writer, &abort));
            let mut resampler = Upsample2::default();
            let result = {
                let mut submit = |mut pcm: Vec<f32>| -> Result<(), String> {
                    sanitize_segment(&mut pcm, OUTPUT_SAMPLE_RATE, &mut remaining_samples, &mut running_gain)?;
                    total_samples += pcm.len();
                    seg_tx.send(pcm).map_err(|_| "播报输出已停止".to_string())
                };
                let result = {
                    let mut sink = |segment: &[f32]| -> bool {
                        // The FFI callback only delivers non-empty segments.
                        let pcm = if upsample { resampler.push(segment) } else { segment.to_vec() };
                        match submit(pcm) {
                            Ok(()) => true,
                            Err(reason) => {
                                stream_err = Some(reason);
                                false
                            }
                        }
                    };
                    engine.synthesize(&text, ref_samples, ref_rate, &abort, &mut sink)
                };
                if result.is_ok() && stream_err.is_none() && !abort.aborted() {
                    if let Some(tail) = resampler.finish() {
                        stream_err = submit(vec![tail]).err();
                    }
                }
                result
            };
            // sink 按引用借用 seg_tx（send 取 &self），析构不关闭通道；显式 drop
            // 关闭通道，输出线程排空后退出，再回收 writer 供下一句复用。
            drop(seg_tx);
            output.join().expect("输出线程 panic");
            result
        });

        if abort.aborted() {
            // TTS_JOB_STOP 或 SIGTERM 在途到达：取消本句，不投递、不计失败。
            shared.add_stopped(1);
        } else if let Some(reason) = stream_err {
            shared.add_failed(1);
            shared.set_last_error(&format!("{}: {reason}", job.utterance_id));
        } else if let Err(reason) = result {
            shared.add_failed(1);
            shared.set_last_error(&format!("{}: {reason}", job.utterance_id));
        } else if total_samples == 0 {
            shared.add_failed(1);
            shared.set_last_error(&format!("{}: 引擎返回空音频", job.utterance_id));
        } else {
            shared.add_synthesized(1);
        }
        shared.set_synthesizing(false);
    }
    shared.set_synthesizing(false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Abort;
    use mozart_core::dgram::FrameSocket;
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

    #[test]
    fn duration_cap_clamps_to_bounds() {
        assert_eq!(duration_cap_samples(0), 192_000);
        assert_eq!(duration_cap_samples(10), 264_000);
        assert_eq!(duration_cap_samples(100), 1_776_000);
        assert_eq!(duration_cap_samples(200), 2_160_000);
    }

    #[test]
    fn sample_budget_accepts_the_exact_tail_and_rejects_one_beyond_it() {
        let mut remaining = 4;
        let mut gain = 1.0;
        let mut resampler = Upsample2::default();
        for mut segment in [resampler.push(&[0.4]), resampler.push(&[0.6])] {
            sanitize_segment(&mut segment, OUTPUT_SAMPLE_RATE, &mut remaining, &mut gain).unwrap();
        }
        assert_eq!(remaining, 1);
        let mut tail = [resampler.finish().unwrap()];
        sanitize_segment(&mut tail, OUTPUT_SAMPLE_RATE, &mut remaining, &mut gain).unwrap();
        assert_eq!(tail, [0.6]);
        assert_eq!(remaining, 0);
        assert!(sanitize_segment(&mut [0.1], OUTPUT_SAMPLE_RATE, &mut remaining, &mut gain).is_err());
    }

    #[test]
    fn sanitize_rejects_empty_and_non_finite() {
        assert!(sanitize(&mut [], 48000, 45.0).is_err());
        assert!(sanitize(&mut [f32::NAN], 48000, 45.0).is_err());
        assert!(sanitize(&mut [f32::INFINITY], 48000, 45.0).is_err());
        assert!(sanitize(&mut [0.1, f32::NEG_INFINITY], 48000, 45.0).is_err());
    }

    #[test]
    fn sanitize_rejects_over_duration_cap() {
        // 5 s @ 1 kHz，上限 4 s。
        let mut pcm = vec![0.1f32; 5000];
        let err = sanitize(&mut pcm, 1000, 4.0).unwrap_err();
        assert!(err.contains("超上限"), "{err}");
    }

    #[test]
    fn sanitize_rejects_peak_over_limit() {
        let mut pcm = [0.5f32, 4.5];
        let err = sanitize(&mut pcm, 48000, 45.0).unwrap_err();
        assert!(err.contains("超安全范围"), "{err}");
    }

    #[test]
    fn sanitize_normalizes_peak_to_target() {
        let mut pcm = [2.0f32, -2.0, 1.0];
        sanitize(&mut pcm, 48000, 45.0).unwrap();
        // gain = 0.98 / 2 = 0.49。
        assert!((pcm[0] - 0.98).abs() < 1e-6);
        assert!((pcm[1] + 0.98).abs() < 1e-6);
        assert!((pcm[2] - 0.49).abs() < 1e-6);
    }

    #[test]
    fn sanitize_keeps_quiet_audio_unchanged() {
        let mut pcm = [0.5f32, -0.25];
        sanitize(&mut pcm, 48000, 45.0).unwrap();
        assert_eq!(pcm, [0.5, -0.25], "peak < 0.98 不放大");
        let mut silent = [0.0f32; 4];
        sanitize(&mut silent, 48000, 45.0).unwrap();
        assert_eq!(silent, [0.0; 4], "peak 为 0 增益 1");
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
        fn synthesize(&self, _text: &str, _reference: &[f32], _ref_rate: u32, abort: &Abort<'_>, sink: &mut dyn FnMut(&[f32]) -> bool) -> Result<(), String> {
            if let Some(hook) = &self.hook {
                hook();
            }
            let pcm = self.outcome.clone()?;
            if abort.aborted() {
                return Ok(());
            }
            sink(&pcm);
            Ok(())
        }
    }

    /// 临时输出 socket + 接收方 + writer + shared。接收方先绑定（模拟 pre），
    /// writer 惰性建链后逐帧送达；_rx 保持存活到用例结束。
    fn writer_fixture(tag: &str) -> (PathBuf, FrameWriter, Arc<Shared>, FrameSocket<mozart_core::SpeechFrame>) {
        let dir = std::env::temp_dir().join(format!("mozart-tts-synth-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.sock");
        let rx = FrameSocket::<mozart_core::SpeechFrame>::bind(&path, 8).unwrap();
        let shared = Arc::new(Shared::new());
        let writer = FrameWriter::new(path.clone(), shared.clone());
        (path, writer, shared, rx)
    }

    fn cleanup(path: &Path) {
        std::fs::remove_file(path).unwrap();
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    fn job(id: &str) -> Job {
        Job { issued_ns: mozart_core::speech::monotonic_ns(), text: "Hello world".to_string(), utterance_id: id.to_string() }
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
    fn worker_rejects_non_finite_audio() {
        let (path, mut writer, shared, _rx) = writer_fixture("nan");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        let engine = FakeEngine { rate: TTS_NATIVE_SAMPLE_RATE, outcome: Ok(vec![f32::NAN; 2400]), hook: None };
        tx.send(job("a:1")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.failed, 1);
        assert_eq!(status.synthesized, 0);
        assert_eq!(status.frames_out, 0);
        assert!(status.last_error.unwrap().contains("非有限"));
        cleanup(&path);
    }

    #[test]
    fn worker_rejects_over_duration_cap() {
        // 文本 1 字符 → 上限 4 s；引擎返回 5 s @ 24 kHz（重采样后仍 5 s）。
        let (path, mut writer, shared, _rx) = writer_fixture("long");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        let engine = FakeEngine { rate: TTS_NATIVE_SAMPLE_RATE, outcome: Ok(vec![0.1; 120000]), hook: None };
        tx.send(Job { issued_ns: mozart_core::speech::monotonic_ns(), text: "x".to_string(), utterance_id: "a:1".to_string() }).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.failed, 1);
        assert_eq!(status.frames_out, 0);
        assert!(status.last_error.unwrap().contains("超上限"));
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

    #[test]
    fn job_stop_cancels_inflight_and_drains_queue() {
        // TTS_JOB_STOP 推进代际：在途句取消、在队两句下一轮清空，全部计 stopped。
        let (path, mut writer, shared, _rx) = writer_fixture("jobstop");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        let hook_shared = shared.clone();
        let engine = FakeEngine {
            rate: TTS_NATIVE_SAMPLE_RATE,
            outcome: Ok(vec![0.25; 2400]),
            hook: Some(Box::new(move || hook_shared.stop_jobs())),
        };
        for id in ["a:1", "a:2", "a:3"] {
            tx.send(job(id)).unwrap();
            shared.queue_push();
        }
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.synthesized, 0);
        assert_eq!(status.failed, 0);
        assert_eq!(status.frames_out, 0);
        assert_eq!(status.stopped, 3, "在途 1 + 在队 2 全部取消");
        assert_eq!(status.queue_depth, 0);
        cleanup(&path);
    }

    #[test]
    fn job_stop_then_new_job_synthesizes_normally() {
        // 停止只取消当时在途/在队任务；代际稳定后新任务正常合成。
        let (path, mut writer, shared, _rx) = writer_fixture("jobstop-resume");
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        shared.stop_jobs(); // 先停止（无在途任务）。
        let engine = FakeEngine { rate: TTS_NATIVE_SAMPLE_RATE, outcome: Ok(vec![0.25; 2400]), hook: None };
        tx.send(job("a:9")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(rx, &engine, None, &mut writer, &shared, &stop);
        let status = shared.status();
        assert_eq!(status.synthesized, 1, "停止后的新任务必须正常合成");
        assert_eq!(status.stopped, 0);
        cleanup(&path);
    }

    /// 流式直播：引擎分三段交付（段间睡眠模拟解码耗时），首帧须在整句合成完成前
    /// 送达接收方（边合成边发），帧序号跨段连续。整句批量实现会在此超时失败。
    #[test]
    fn streaming_outputs_first_segment_before_synthesis_completes() {
        use std::sync::atomic::AtomicU64;
        use std::time::Instant;

        let (path, mut writer, shared, rx) = writer_fixture("stream");
        let stop = AtomicBool::new(false);
        let start = Instant::now();
        let first_frame_ms = Arc::new(AtomicU64::new(u64::MAX));
        let synth_done_ms = Arc::new(AtomicU64::new(u64::MAX));

        let receiver = {
            let first = first_frame_ms.clone();
            std::thread::spawn(move || {
                for idx in 0..3u32 {
                    let frame = rx.recv(Some(Duration::from_secs(5))).unwrap();
                    if idx == 0 {
                        first.store(start.elapsed().as_millis() as u64, Relaxed);
                    }
                    let mozart_core::FrameMeta { frame_idx, .. } = frame.frame.meta;
                    assert_eq!(frame_idx, idx, "帧序号须跨段连续");
                }
            })
        };

        struct SegEngine {
            start: Instant,
            done: Arc<AtomicU64>,
        }
        impl Synthesizer for SegEngine {
            fn sample_rate(&self) -> u32 {
                OUTPUT_SAMPLE_RATE
            }
            fn synthesize(&self, _t: &str, _r: &[f32], _rr: u32, abort: &Abort<'_>, sink: &mut dyn FnMut(&[f32]) -> bool) -> Result<(), String> {
                for i in 0..3 {
                    if abort.aborted() {
                        return Ok(());
                    }
                    if !sink(&vec![0.25; mozart_core::OUTPUT_SAMPLES]) {
                        return Ok(());
                    }
                    if i < 2 {
                        std::thread::sleep(Duration::from_millis(150));
                    }
                }
                self.done.store(self.start.elapsed().as_millis() as u64, Relaxed);
                Ok(())
            }
        }
        let engine = SegEngine { start, done: synth_done_ms.clone() };

        let (tx, jobs_rx) = mpsc::sync_channel(1);
        tx.send(job("a:1")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(jobs_rx, &engine, None, &mut writer, &shared, &stop);
        receiver.join().unwrap();

        let first = first_frame_ms.load(Relaxed);
        let done = synth_done_ms.load(Relaxed);
        assert!(first < done, "首帧 {first} ms 须早于整句合成完成 {done} ms（边合成边发）");
        let status = shared.status();
        assert_eq!(status.synthesized, 1);
        assert_eq!(status.frames_out, 3);
        cleanup(&path);
    }

    /// 流式跨段增益：首段峰值超 0.98 将句内增益压到 0.5 后，次段峰值 0.5
    /// 须沿用 0.5 增益（输出 0.25），不得跳回 1.0。
    #[test]
    fn streaming_keeps_running_gain_monotonic_across_segments() {
        struct TwoSegEngine;
        impl Synthesizer for TwoSegEngine {
            fn sample_rate(&self) -> u32 {
                OUTPUT_SAMPLE_RATE
            }
            fn synthesize(
                &self,
                _text: &str,
                _reference: &[f32],
                _ref_rate: u32,
                _abort: &Abort<'_>,
                sink: &mut dyn FnMut(&[f32]) -> bool,
            ) -> Result<(), String> {
                assert!(sink(&vec![1.96f32; mozart_core::OUTPUT_SAMPLES]));
                assert!(sink(&vec![0.5f32; mozart_core::OUTPUT_SAMPLES]));
                Ok(())
            }
        }

        let (path, mut writer, shared, rx) = writer_fixture("gain");
        let stop = AtomicBool::new(false);
        let (tx, jobs_rx) = mpsc::sync_channel(1);
        tx.send(job("a:1")).unwrap();
        shared.queue_push();
        drop(tx);
        run_with(jobs_rx, &TwoSegEngine, None, &mut writer, &shared, &stop);

        let f0 = rx.recv(Some(Duration::from_secs(2))).unwrap();
        let f1 = rx.recv(Some(Duration::from_secs(2))).unwrap();
        let p0 = f0.frame.pcm;
        let p1 = f1.frame.pcm;
        assert!((p0[0] - TARGET_PEAK).abs() < 1e-6);
        assert!((p1[0] - 0.25).abs() < 1e-6, "次段须沿用 0.5 增益：{}", p1[0]);
        cleanup(&path);
    }
}
