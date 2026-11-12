//! 解码线程（SPEC-05）：从环取帧喂流式识别器，发 partial/final 事件，
//! endpoint 切句并把整句音频所有权移交复核队列。句音频缓冲 700 帧（14 s）上限，
//! 触发即返回致命错误、进程退出由 daemon 重启。SIGTERM 喂 0.3 s 静音切出最后 final。
//! 解码与复核线程只交付事件，不碰事件 socket（输出线程独占）。

use crate::event::{Event, REVIEW_SKIPPED};
use crate::ffi::{self, Recognizer, Stream};
use crate::receiver::Inbox;
use crate::review::ReviewJob;
use crate::state::Shared;
use mozart_core::ring::Ring;
use mozart_core::{InputFrame, INPUT_SAMPLES};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 句音频缓冲上限（帧）：700 帧 = 14 s，超过即进程退出（SPEC-05 兜底）。
pub const MAX_UTTERANCE_FRAMES: usize = 700;
/// SIGTERM/空闲收尾喂入的静音样本数：0.3 s × 16 kHz。
const TAIL_SILENCE_SAMPLES: usize = mozart_core::INPUT_SAMPLE_RATE as usize * 3 / 10;
/// partial 最小发送间隔（沿用旧 0.1 s 节流）。
const PARTIAL_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// 出队轮询步长：超时后检查停止标志。
const POP_POLL: Duration = Duration::from_millis(100);
/// 一帧时长（ns），用于句尾 pts。
const FRAME_NS: u64 = mozart_core::FRAME_MS as u64 * 1_000_000;

/// 解码线程入口：加载在线识别器后进入循环。加载失败返回致命错误。
pub fn run(
    ring: Arc<Ring<Inbox>>,
    model_dir: PathBuf,
    threads: u32,
    review: Option<SyncSender<ReviewJob>>,
    events: Sender<Event>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let recognizer = ffi::load_online(&model_dir, threads)?;
    run_with(recognizer, ring, review, events, shared, stop)
}

/// 以给定识别器运行循环（测试注入假识别器）。
pub fn run_with(
    recognizer: Box<dyn Recognizer>,
    ring: Arc<Ring<Inbox>>,
    review: Option<SyncSender<ReviewJob>>,
    events: Sender<Event>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let mut decoder = Decoder::new(recognizer, review, events, shared);
    while !stop.load(Relaxed) {
        match ring.pop(Some(POP_POLL)) {
            Some(Inbox::Frame(frame)) => decoder.feed(frame)?,
            Some(Inbox::EndOfInput) => decoder.finish_utterance(),
            None => {}
        }
    }
    // SIGTERM 收尾：喂静音切出最后 final。
    decoder.finish_utterance();
    Ok(())
}

/// 解码状态机。feed/finish_utterance 公开供单元测试直接驱动。
pub struct Decoder {
    recognizer: Box<dyn Recognizer>,
    stream: Option<Box<dyn Stream>>,
    review: Option<SyncSender<ReviewJob>>,
    events: Sender<Event>,
    shared: Arc<Shared>,
    /// 整句 PCM 缓冲（16 kHz f32），endpoint 时所有权移交复核。
    pcm: Vec<f32>,
    start_pts: Option<u64>,
    end_pts: u64,
    /// 已切出 final 的累计字符位置（get_result 是全连接累计文本）。
    cut_chars: usize,
    last_partial: String,
    last_partial_emit: Instant,
    finals: u64,
}

impl Decoder {
    pub fn new(
        recognizer: Box<dyn Recognizer>,
        review: Option<SyncSender<ReviewJob>>,
        events: Sender<Event>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            recognizer,
            stream: None,
            review,
            events,
            shared,
            pcm: Vec::with_capacity(MAX_UTTERANCE_FRAMES * INPUT_SAMPLES),
            start_pts: None,
            end_pts: 0,
            cut_chars: 0,
            last_partial: String::new(),
            last_partial_emit: Instant::now(),
            finals: 0,
        }
    }

    /// 喂入一帧。首包开流（模型约束：须从连接首包喂数据）。超 700 帧返回致命错误。
    pub fn feed(&mut self, frame: InputFrame) -> Result<(), String> {
        // packed 字段按值读取，避免非对齐引用。
        let pts = frame.meta.pts_ns;
        let pcm = frame.pcm;
        if self.stream.is_none() {
            self.stream = Some(self.recognizer.create_stream());
            self.cut_chars = 0;
            self.last_partial.clear();
            self.shared.set_decoding(true);
        }
        // 每句首帧记录起点：endpoint 切句后流复用，缓冲已清空。
        if self.pcm.is_empty() {
            self.start_pts = Some(pts);
        }
        if self.pcm.len() / INPUT_SAMPLES >= MAX_UTTERANCE_FRAMES {
            return Err(format!(
                "句音频缓冲超过 {MAX_UTTERANCE_FRAMES} 帧（14 s）上限，进程退出"
            ));
        }
        self.pcm.extend_from_slice(&pcm);
        self.end_pts = pts + FRAME_NS;
        {
            let stream = self.stream.as_mut().expect("stream 已在首包创建");
            stream.accept_waveform(&pcm);
            stream.decode();
        }
        self.show_partial();
        if self.stream.as_ref().expect("stream 存在").is_endpoint() {
            self.cut_final();
            self.stream.as_mut().expect("stream 存在").reset();
            self.cut_chars = 0;
            self.last_partial.clear();
        }
        Ok(())
    }

    /// 空闲/收尾切句：喂 0.3 s 静音 + input_finished，切出最后 final，丢弃流。
    /// 下次首包重建流（识别器前端状态由识别器保留）。
    pub fn finish_utterance(&mut self) {
        if self.stream.is_none() {
            return;
        }
        let silence = [0f32; TAIL_SILENCE_SAMPLES];
        {
            let stream = self.stream.as_mut().expect("stream 存在");
            stream.accept_waveform(&silence);
            stream.input_finished();
            stream.decode();
        }
        self.cut_final();
        self.stream = None;
        self.shared.set_decoding(false);
    }

    /// 节流发送 partial：增量变化且距上次 ≥ 100 ms 且有文本内容。
    fn show_partial(&mut self) {
        let Some(stream) = self.stream.as_ref() else {
            return;
        };
        let text = stream.get_result();
        let delta: String = text.chars().skip(self.cut_chars).collect();
        let now = Instant::now();
        if delta != self.last_partial && now.duration_since(self.last_partial_emit) >= PARTIAL_MIN_INTERVAL
        {
            self.last_partial = delta.clone();
            self.last_partial_emit = now;
            if has_text_content(&delta) {
                let seq = self.finals + 1;
                let event = Event::partial(
                    &self.shared.startup_id,
                    seq,
                    delta,
                    self.start_pts,
                    Some(self.end_pts),
                );
                let _ = self.events.send(event);
            }
        }
    }

    /// 切出当前增量为 final，发事件并把整句 PCM 移交复核。
    fn cut_final(&mut self) {
        let Some(stream) = self.stream.as_ref() else {
            return;
        };
        let text = stream.get_result();
        let total = text.chars().count();
        // 贪心解码对已切前缀的罕见回退，防负切片（沿用旧实现）。
        if total < self.cut_chars {
            self.cut_chars = total;
        }
        let delta: String = text.chars().skip(self.cut_chars).collect();
        let delta = delta.trim().to_string();
        self.cut_chars = total;
        self.last_partial.clear();
        let start_pts = self.start_pts;
        let end_pts = self.end_pts;
        let pcm = std::mem::take(&mut self.pcm);
        self.start_pts = None;

        if !has_text_content(&delta) {
            if !delta.is_empty() {
                eprintln!("mozart-asr: 跳过纯标点 final：{delta:?}");
            }
            return;
        }
        self.finals += 1;
        self.shared.add_final(1);
        let review_enabled = self.review.is_some();
        let event = Event::final_utterance(
            &self.shared.startup_id,
            self.finals,
            delta,
            start_pts,
            Some(end_pts),
            review_enabled,
        );
        let _ = self.events.send(event.clone());

        let Some(tx) = self.review.as_ref() else {
            return;
        };
        if pcm.is_empty() {
            return;
        }
        let job = ReviewJob {
            final_event: event,
            pcm,
        };
        match tx.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                self.shared.add_review_skipped(1);
                let skipped = job.final_event.into_refined(
                    REVIEW_SKIPPED,
                    None,
                    Some("复核队列已满，保留在线文本".to_string()),
                    0,
                );
                let _ = self.events.send(skipped);
            }
            Err(TrySendError::Disconnected(job)) => {
                self.shared.add_review_skipped(1);
                let skipped = job.final_event.into_refined(
                    REVIEW_SKIPPED,
                    None,
                    Some("复核线程已退出".to_string()),
                    0,
                );
                let _ = self.events.send(skipped);
            }
        }
    }
}

/// 文本内容判定：含字母/数字/CJK 即视为有内容。
/// 近似旧 unicodedata「非空白且非标点」判定，用于跳过纯标点 final。
fn has_text_content(text: &str) -> bool {
    text.chars().any(|c| c.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_text_content_skips_punctuation_only() {
        assert!(!has_text_content(""));
        assert!(!has_text_content("。，！？……"));
        assert!(!has_text_content(".,!? "));
        assert!(has_text_content("你好"));
        assert!(has_text_content("hello"));
        assert!(has_text_content("123"));
        assert!(has_text_content("你好。"));
    }
}