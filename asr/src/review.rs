//! 复核线程（SPEC-05）：独立队列容量 2，整句音频所有权随提交移交。
//! SenseVoice 离线复核整句，发 refined 事件。模型加载失败不影响在线识别，每句报 failed。
//! 队满由解码侧标 skipped（见 decoder）。SIGTERM 限时 1 s 排空，剩余标 skipped。

use crate::event::{Event, REVIEW_COMPLETED, REVIEW_FAILED, REVIEW_SKIPPED};
use crate::ffi::{self, OfflineRecognizer};
use crate::state::Shared;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 复核队列容量（SPEC-05：2）。
pub const QUEUE_CAPACITY: usize = 2;
/// SIGTERM 复核排空时限（SPEC-05：1 s）。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// 收任务轮询步长：超时后检查停止标志。
const POLL: Duration = Duration::from_millis(100);

/// 一次复核任务：final 事件（派生 refined 用）+ 整句 PCM（所有权移交）。
pub struct ReviewJob {
    pub final_event: Event,
    pub pcm: Vec<f32>,
}

/// 复核线程入口：加载离线识别器后进入循环。
pub fn run(
    rx: Receiver<ReviewJob>,
    model_dir: PathBuf,
    threads: u32,
    events: Sender<Event>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) {
    let recognizer = ffi::load_offline(&model_dir, threads);
    if let Err(err) = &recognizer {
        shared.set_last_error(err);
    }
    run_with(rx, recognizer, events, shared, stop);
}

/// 以给定识别器（或加载错误）运行循环（测试注入）。
pub fn run_with(
    rx: Receiver<ReviewJob>,
    recognizer: Result<Box<dyn OfflineRecognizer>, String>,
    events: Sender<Event>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) {
    let mut deadline: Option<Instant> = None;
    loop {
        if stop.load(Relaxed) && deadline.is_none() {
            deadline = Some(Instant::now() + DRAIN_TIMEOUT);
        }
        let timeout = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => POLL,
        };
        match rx.recv_timeout(timeout) {
            Ok(job) => {
                // 时限已过：本句与队列剩余都标 skipped，不再解码。
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    skip(job, &events, &shared);
                    break;
                }
                process(job, &recognizer, &events, &shared);
            }
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // 限时排空后剩余任务标 skipped。
    while let Ok(job) = rx.try_recv() {
        skip(job, &events, &shared);
    }
}

/// 复核跳过：计数并发 refined skipped 事件（队满/退出时限共用）。
fn skip(job: ReviewJob, events: &Sender<Event>, shared: &Shared) {
    shared.add_review_skipped(1);
    let event = job.final_event.into_refined(
        REVIEW_SKIPPED,
        None,
        Some("复核在退出时限内未排空".to_string()),
        0,
    );
    let _ = events.send(event);
}

/// 复核一句：成功发 completed，空文本或错误发 failed。
fn process(
    job: ReviewJob,
    recognizer: &Result<Box<dyn OfflineRecognizer>, String>,
    events: &Sender<Event>,
    shared: &Shared,
) {
    shared.set_reviewing(true);
    let began = Instant::now();
    let result = match recognizer {
        Ok(r) => r.decode(&job.pcm),
        Err(err) => Err(err.clone()),
    };
    let event = match result {
        Ok(text) if !text.trim().is_empty() => job.final_event.into_refined(
            REVIEW_COMPLETED,
            Some(text),
            None,
            began.elapsed().as_millis() as u64,
        ),
        Ok(_) => {
            shared.add_review_failed(1);
            job.final_event.into_refined(
                REVIEW_FAILED,
                None,
                Some("复核未返回文本".to_string()),
                0,
            )
        }
        Err(err) => {
            shared.add_review_failed(1);
            shared.set_last_error(&err);
            job.final_event
                .into_refined(REVIEW_FAILED, None, Some(err), 0)
        }
    };
    let _ = events.send(event);
    shared.set_reviewing(false);
}