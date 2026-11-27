//! 合成工作线程：从任务队列取句 → 文本预处理 → PocketTTS 流式合成（生成回调每段
//! 交付 PCM）→ 逐段重采样 48 kHz → 经单句预合成槽 + 有界段通道交由常驻播放线程
//! 按 20 ms 节拍发契约帧（边合成边播，且在播放当前句时提前合成下一句以消除句间空白；
//! v1 不走变声，直接喂 pre 播放）。模型加载失败为致命错误（进程退出由 daemon 重启）；
//! 单句合成失败计数并记录原因，不影响后续任务。SIGTERM 放弃在途合成、预合成槽与队列剩余。

use crate::ffi::{self, Synthesizer};
use crate::jobs::Job;
use crate::output::FrameWriter;
use crate::resample::{Upsample2, TTS_NATIVE_SAMPLE_RATE};
use crate::state::{Abort, Shared};
use mozart_core::OUTPUT_SAMPLE_RATE;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// 队列轮询步长：让停止标志在百毫秒级生效（daemon 宽限 2 s）。
const QUEUE_POLL: Duration = Duration::from_millis(100);

/// 合成段通道容量（段数）：输出线程按实时节拍消费，通道满时 sink 阻塞反压，
/// 把引擎解码节流到播放速率；取值 32 使预合成中的下一句短句可先跑完解码
/// 再等播放槽切换，同时仍把峰值缓冲限制在有界范围内。
const SEG_BUFFER: usize = 32;

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

enum SynthOutcome {
    Finished(usize),
    Failed(String),
}

struct PlaybackItem<'a> {
    utterance_id: String,
    abort: Abort<'a>,
    seg_rx: Receiver<Vec<f32>>,
    done_rx: Receiver<SynthOutcome>,
}

struct SlotState<'a> {
    item: Option<PlaybackItem<'a>>,
    closed: bool,
}

/// 容量为 1 的单句预合成槽：允许合成线程在播放线程按 20 ms 节拍消费当前句时，
/// 提前启动下一句的 LM + 解码；槽满时阻塞合成线程，避免超前堆积多个长句。
struct PrefetchSlot<'a> {
    state: Mutex<SlotState<'a>>,
    not_full: Condvar,
    not_empty: Condvar,
}

impl<'a> PrefetchSlot<'a> {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState { item: None, closed: false }),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
        }
    }

    /// 等待预合成槽空出或停止信号置位；返回 false 表示应结束循环。
    fn wait_empty(&self, stop: &AtomicBool) -> bool {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while guard.item.is_some() {
            if stop.load(Relaxed) {
                return false;
            }
            let (next, _) = self.not_full.wait_timeout(guard, QUEUE_POLL)
                .unwrap_or_else(|p| p.into_inner());
            guard = next;
        }
        !stop.load(Relaxed)
    }

    /// 放入待播报句；调用方（唯一生产者）已在调用前通过 wait_empty 确认槽位为空。
    fn push(&self, item: PlaybackItem<'a>) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        debug_assert!(guard.item.is_none() && !guard.closed);
        guard.item = Some(item);
        self.not_empty.notify_one();
    }

    /// 取出下一句，直至槽内已无未消费句且生产者已关闭。
    fn pop(&self) -> Option<PlaybackItem<'a>> {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(item) = guard.item.take() {
                self.not_full.notify_one();
                return Some(item);
            }
            if guard.closed {
                return None;
            }
            guard = self.not_empty.wait(guard).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn close(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.closed = true;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }
}

struct CloseOnDrop<'s, 'a>(&'s PrefetchSlot<'a>);

impl Drop for CloseOnDrop<'_, '_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn synthesize_utterance(
    job: &Job,
    engine: &dyn Synthesizer,
    ref_samples: &[f32],
    ref_rate: u32,
    abort: &Abort<'_>,
    seg_tx: mpsc::SyncSender<Vec<f32>>,
) -> SynthOutcome {
    let text = precondition(&job.text);
    let budget = duration_cap_samples(job.text.chars().count());
    let rate = engine.sample_rate();
    let upsample = match rate {
        OUTPUT_SAMPLE_RATE => false,
        TTS_NATIVE_SAMPLE_RATE => true,
        _ => return SynthOutcome::Failed(format!("不支持的 TTS 采样率 {rate} Hz")),
    };

    // 用累计样本核算时长，包括句末插值样本。句内增益单调不增。
    let mut remaining_samples = budget;
    let mut running_gain = 1.0f32;
    let mut total_samples = 0usize;
    let mut stream_err: Option<String> = None;
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
            engine.synthesize(&text, ref_samples, ref_rate, abort, &mut sink)
        };
        if result.is_ok() && stream_err.is_none() && !abort.aborted() {
            if let Some(tail) = resampler.finish() {
                stream_err = submit(vec![tail]).err();
            }
        }
        result
    };
    // 显式关闭通道（drop 按值接管的 seg_tx），让播放端按正常收尾发完末帧。
    drop(seg_tx);

    if let Some(reason) = stream_err {
        SynthOutcome::Failed(reason)
    } else if let Err(reason) = result {
        SynthOutcome::Failed(reason)
    } else {
        SynthOutcome::Finished(total_samples)
    }
}

fn playback_loop(slot: &PrefetchSlot<'_>, writer: &mut FrameWriter, shared: &Shared) {
    while let Some(item) = slot.pop() {
        if !item.abort.aborted() {
            crate::output::write_stream(item.seg_rx, writer, &item.abort);
        } else {
            // 已中止则立刻弃置段接收端，解除合成端可能阻塞的 seg_tx.send。
            drop(item.seg_rx);
        }
        let outcome = item.done_rx.recv();
        // 先撤出在途计数（SeqCst），再发布终态计数，保证外部看到完成增量即可见状态回落 idle。
        shared.leave_flight();
        if item.abort.aborted() {
            // TTS_JOB_STOP 或 SIGTERM 在途到达：取消本句，不计失败。
            shared.add_stopped(1);
            continue;
        }
        match outcome {
            Ok(SynthOutcome::Failed(reason)) => {
                shared.set_last_error(&format!("{}: {reason}", item.utterance_id));
                shared.add_failed(1);
            }
            Ok(SynthOutcome::Finished(0)) => {
                shared.set_last_error(&format!("{}: 引擎返回空音频", item.utterance_id));
                shared.add_failed(1);
            }
            Ok(SynthOutcome::Finished(_)) => shared.add_synthesized(1),
            Err(_) => {
                shared.set_last_error(&format!("{}: 合成通道意外关闭", item.utterance_id));
                shared.add_failed(1);
            }
        }
    }
}

/// 服务循环（引擎可注入，供测试）。合成线程与常驻播放线程通过单句预合成槽解耦：
/// 播放线程按 20 ms 节拍输出当前句时，合成线程可提前合成下一句以消除句间空白。
/// 进程停止标志置位后放弃在途结果与队列剩余；TTS_JOB_STOP 推进任务代际，在途
/// 合成与预合成槽中的旧句经 Abort 中止并计 stopped。
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
    let slot = PrefetchSlot::new();
    std::thread::scope(|scope| {
        let playback = scope.spawn(|| playback_loop(&slot, writer, shared));
        let _close = CloseOnDrop(&slot);

        while !stop.load(Relaxed) {
            if !slot.wait_empty(stop) {
                break;
            }
            let job = match jobs.recv_timeout(QUEUE_POLL) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if stop.load(Relaxed) {
                shared.queue_pop();
                break;
            }
            // 先 enter_flight 再 queue_pop，避免两者之间出现 queue_depth==0 && state=="idle" 假窗口。
            shared.enter_flight();
            shared.queue_pop();
            let abort = shared.abort_job(stop, job.issued_ns);
            if abort.aborted() {
                shared.leave_flight();
                shared.add_stopped(1);
                continue;
            }

            shared.set_synthesizing(true);
            let (seg_tx, seg_rx) = mpsc::sync_channel::<Vec<f32>>(SEG_BUFFER);
            let (done_tx, done_rx) = mpsc::sync_channel::<SynthOutcome>(1);
            slot.push(PlaybackItem {
                utterance_id: job.utterance_id.clone(),
                abort: shared.abort_job(stop, job.issued_ns),
                seg_rx,
                done_rx,
            });
            let outcome = synthesize_utterance(&job, engine, ref_samples, ref_rate, &abort, seg_tx);
            // 须在 done_tx.send 前清除 synthesizing，避免 playback_loop 先 leave_flight 并加
            // 完成计数后，status() 在 idle 检查窗口读到残留的 synthesizing == true。
            shared.set_synthesizing(false);
            if done_tx.send(outcome).is_err() {
                shared.set_last_error(&format!("{}: 播放线程已提前退出", job.utterance_id));
                break;
            }
        }

        drop(_close);
        playback.join().expect("播放线程 panic");
    });
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

    /// 预合成解耦：播放第 1 句（~280 ms）的同时提前跑完第 2 句 LM 耗时（180 ms），
    /// 第 1 句末帧与第 2 句首帧之间的空白须明显小于 LM 耗时，不再串行卡顿。
    #[test]
    fn prefetch_overlaps_next_sentence_lm_with_current_playback() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Instant;

        const LM_DELAY: Duration = Duration::from_millis(180);
        const FRAMES_PER_UTT: usize = 15; // 14 * 20ms = 280ms 播放时长
        struct SlowLmEngine {
            calls: Arc<AtomicUsize>,
            second_synth_start: Arc<Mutex<Option<Instant>>>,
        }
        impl Synthesizer for SlowLmEngine {
            fn sample_rate(&self) -> u32 {
                OUTPUT_SAMPLE_RATE
            }
            fn synthesize(
                &self,
                _t: &str,
                _r: &[f32],
                _rr: u32,
                abort: &Abort<'_>,
                sink: &mut dyn FnMut(&[f32]) -> bool,
            ) -> Result<(), String> {
                let idx = self.calls.fetch_add(1, Relaxed);
                if idx == 1 {
                    *self.second_synth_start.lock().unwrap() = Some(Instant::now());
                }
                std::thread::sleep(LM_DELAY);
                if abort.aborted() {
                    return Ok(());
                }
                let _ = sink(&vec![0.25; FRAMES_PER_UTT * mozart_core::OUTPUT_SAMPLES]);
                Ok(())
            }
        }

        let (path, mut writer, shared, rx) = writer_fixture("prefetch-overlap");
        let stop = AtomicBool::new(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let second_synth_start = Arc::new(Mutex::new(None));
        let engine = SlowLmEngine {
            calls: calls.clone(),
            second_synth_start: second_synth_start.clone(),
        };

        let j1 = job("a:1");
        let mut j2 = job("a:2");
        j2.issued_ns = j1.issued_ns + 1;

        let (tx, jobs_rx) = mpsc::sync_channel(4);
        tx.send(j1).unwrap();
        tx.send(j2).unwrap();
        shared.queue_push();
        shared.queue_push();
        drop(tx);

        let receiver = std::thread::spawn(move || {
            let mut s1_last = None;
            let mut s2_first = None;
            for idx in 0..(FRAMES_PER_UTT * 2) {
                let frame = rx.recv(Some(Duration::from_secs(5))).unwrap();
                let stamp = frame.issued_ns;
                if idx == FRAMES_PER_UTT - 1 {
                    s1_last = Some((Instant::now(), stamp));
                } else if idx == FRAMES_PER_UTT {
                    s2_first = Some((Instant::now(), stamp));
                }
            }
            (s1_last.unwrap(), s2_first.unwrap())
        });

        run_with(jobs_rx, &engine, None, &mut writer, &shared, &stop);
        let ((s1_last_at, _s1_stamp), (s2_first_at, _s2_stamp)) = receiver.join().unwrap();

        let s2_start = second_synth_start.lock().unwrap().expect("应已开始合成第 2 句");
        assert!(
            s2_start < s1_last_at,
            "第 2 句须在第 1 句播完前提前开始合成"
        );
        let gap = s2_first_at.saturating_duration_since(s1_last_at);
        assert!(
            gap < Duration::from_millis(100),
            "句间切换空白 {} ms 须远小于 LM 耗时 {} ms",
            gap.as_millis(),
            LM_DELAY.as_millis()
        );
        let status = shared.status();
        assert_eq!(status.synthesized, 2);
        assert_eq!(status.frames_out, (FRAMES_PER_UTT * 2) as u64);
        assert_eq!(status.state, "idle");
        cleanup(&path);
    }

    /// 预合成槽容量严格为 1：第 1 句播放期间最多预合成第 2 句，第 3 句须等第 1 句交出
    /// 播放槽（第 2 句弹出预合成槽）后才能开始合成，避免超前堆积多个长句。
    #[test]
    fn prefetch_slot_bounds_ahead_to_single_utterance() {
        use std::sync::atomic::AtomicUsize;

        const FRAMES_S1: usize = 45; // 44 * 20ms = 880ms
        struct CountingEngine {
            calls: Arc<AtomicUsize>,
        }
        impl Synthesizer for CountingEngine {
            fn sample_rate(&self) -> u32 {
                OUTPUT_SAMPLE_RATE
            }
            fn synthesize(
                &self,
                _t: &str,
                _r: &[f32],
                _rr: u32,
                _abort: &Abort<'_>,
                sink: &mut dyn FnMut(&[f32]) -> bool,
            ) -> Result<(), String> {
                let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let frames = if n == 1 { FRAMES_S1 } else { 3 };
                let _ = sink(&vec![0.25; frames * mozart_core::OUTPUT_SAMPLES]);
                Ok(())
            }
        }

        let (path, mut writer, shared, rx) = writer_fixture("prefetch-bound");
        let stop = AtomicBool::new(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = CountingEngine { calls: calls.clone() };

        let j1 = job("a:1");
        let mut j2 = job("a:2");
        let mut j3 = job("a:3");
        j2.issued_ns = j1.issued_ns + 1;
        j3.issued_ns = j1.issued_ns + 2;

        let (tx, jobs_rx) = mpsc::sync_channel(4);
        tx.send(j1).unwrap();
        tx.send(j2).unwrap();
        tx.send(j3).unwrap();
        shared.queue_push();
        shared.queue_push();
        shared.queue_push();
        drop(tx);

        let receiver = std::thread::spawn(move || {
            for _ in 0..(FRAMES_S1 + 6) {
                let _ = rx.recv(Some(Duration::from_secs(5))).unwrap();
            }
        });

        let probe_calls = calls.clone();
        let probe_shared = shared.clone();
        let probe = std::thread::spawn(move || {
            // 等待第 2 句也进入预合成槽且第 1 句仍在播放窗口内。
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while probe_calls.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                assert!(std::time::Instant::now() < deadline, "超时未见第 2 句预合成");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(100));
            let s1_frames_so_far = probe_shared.status().frames_out;
            assert!(
                s1_frames_so_far < FRAMES_S1 as u64,
                "探针超时未停在第 1 句播放窗口内：{s1_frames_so_far}/{FRAMES_S1}"
            );
            assert_eq!(
                probe_calls.load(std::sync::atomic::Ordering::SeqCst),
                2,
                "第 1 句仅播出 {s1_frames_so_far}/{FRAMES_S1} 帧时不得开始合成第 3 句"
            );
        });

        run_with(jobs_rx, &engine, None, &mut writer, &shared, &stop);
        probe.join().unwrap();
        receiver.join().unwrap();

        let status = shared.status();
        assert_eq!(status.synthesized, 3);
        assert_eq!(status.queue_depth, 0);
        assert_eq!(status.state, "idle");
        cleanup(&path);
    }

    /// 运行期触发 TTS_JOB_STOP：正播放的第 1 句立即中止，已在预合成槽的第 2 句与
    /// 在队列的第 3 句均不得播出且全部计入 stopped，最终回落 idle。
    #[test]
    fn job_stop_cancels_playing_prefetched_and_queued_utterances() {
        use std::sync::atomic::AtomicUsize;

        struct ThreeJobEngine {
            calls: Arc<AtomicUsize>,
        }
        impl Synthesizer for ThreeJobEngine {
            fn sample_rate(&self) -> u32 {
                OUTPUT_SAMPLE_RATE
            }
            fn synthesize(
                &self,
                _t: &str,
                _r: &[f32],
                _rr: u32,
                _abort: &Abort<'_>,
                sink: &mut dyn FnMut(&[f32]) -> bool,
            ) -> Result<(), String> {
                let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let frames = if n == 1 { 30 } else { 5 }; // 第 1 句 ~580ms，足够第 2 句进入预合成槽
                let _ = sink(&vec![0.25; frames * mozart_core::OUTPUT_SAMPLES]);
                Ok(())
            }
        }

        let (path, mut writer, shared, rx) = writer_fixture("jobstop-prefetch");
        let stop = AtomicBool::new(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = ThreeJobEngine { calls: calls.clone() };

        let j1 = job("a:1");
        let mut j2 = job("a:2");
        let mut j3 = job("a:3");
        j2.issued_ns = j1.issued_ns + 1;
        j3.issued_ns = j1.issued_ns + 2;
        let s1_ns = j1.issued_ns;
        let s2_ns = j2.issued_ns;
        let s3_ns = j3.issued_ns;

        let (tx, jobs_rx) = mpsc::sync_channel(4);
        tx.send(j1).unwrap();
        tx.send(j2).unwrap();
        tx.send(j3).unwrap();
        shared.queue_push();
        shared.queue_push();
        shared.queue_push();
        drop(tx);

        let stopper_shared = shared.clone();
        let stopper_calls = calls.clone();
        let stopper = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while stopper_calls.load(std::sync::atomic::Ordering::SeqCst) < 2
                || stopper_shared.status().frames_out < 1
            {
                assert!(std::time::Instant::now() < deadline, "超时未进入第1句播放+第2句预合成态");
                std::thread::sleep(Duration::from_millis(5));
            }
            stopper_shared.stop_jobs();
        });

        run_with(jobs_rx, &engine, None, &mut writer, &shared, &stop);
        stopper.join().unwrap();

        let mut seen_s1 = 0usize;
        let mut seen_other = 0usize;
        while let Ok(frame) = rx.recv(Some(Duration::from_millis(50))) {
            let stamp = frame.issued_ns;
            if stamp == s1_ns {
                seen_s1 += 1;
            } else {
                assert!(stamp != s2_ns && stamp != s3_ns, "被取消的预合成句/排队句不得发帧");
                seen_other += 1;
            }
        }
        assert!(seen_s1 >= 1 && seen_s1 < 30, "第 1 句应在中途停止：seen_s1={seen_s1}");
        assert_eq!(seen_other, 0);

        let status = shared.status();
        assert_eq!(status.synthesized, 0);
        assert_eq!(status.failed, 0);
        assert_eq!(status.stopped, 3, "播放中 1 + 预合成槽 1 + 队列 1 全部计 stopped");
        assert_eq!(status.queue_depth, 0);
        assert_eq!(status.state, "idle");
        cleanup(&path);
    }
}
