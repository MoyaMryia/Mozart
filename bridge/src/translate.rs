//! 翻译调度线程（SPEC-05）：消费待翻译队列的 final 句，
//! 预处理 → llama-server 推理 → 质检 → 落字幕（译文修订）→ 入待播报队列。
//! 失败重试最多 retry_max 次（配置上限 2）；重试耗尽计失败并记降级原因。
//! 质检拒绝只计数，不投递播报。translate off 时跳过在队句（原文已发布）。
//! 写字幕失败与字幕线程同纪律：致命退出，由 daemon 重启。

use crate::checks::ContextItem;
use crate::http::LlamaClient;
use crate::preprocess::{self, Glossary};
use crate::quality;
use crate::state::{Queues, Shared, SpeechJob, TranslateJob};
use crate::subtitles::SubtitleStore;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

/// 取件轮询步长：让停止标志在宽限期内生效。
const RECV_POLL: Duration = Duration::from_millis(100);

/// 上下文重复检查保留的先前句对数（桥侧维护，ASR 事件暂不携带 translation_context）。
const CONTEXT_HISTORY: usize = 8;

/// 降级原因：翻译重试耗尽（整句成功时解除；具体原因见最近失败原因字段）。
pub const DEGRADED_TRANSLATE_FAILED: &str = "translation failed";

/// 翻译线程主循环：停止标志置位后退出，在途请求由客户端按停止标志放弃。
pub fn run(
    queues: Arc<Queues>,
    shared: Arc<Shared>,
    store: Arc<SubtitleStore>,
    client: LlamaClient,
    retry_max: u32,
    stop: &AtomicBool,
) -> Result<(), String> {
    let glossary = Glossary::empty();
    let mut context: VecDeque<ContextItem> = VecDeque::new();
    while !stop.load(Relaxed) {
        let job = match queues.recv_translate(RECV_POLL) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => continue,
            // 字幕线程已退出（队列随进程销毁）。
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // 入队后关闭翻译：只保留已发布的原文，不调 llama。
        if !shared.translate_enabled() {
            continue;
        }
        process_job(&job, &queues, &shared, &store, &client, retry_max, &glossary, &mut context, stop)?;
    }
    Ok(())
}

/// 单句管线。Err 仅来自字幕写盘失败（致命）；推理失败与质检拒绝在此计数消化。
fn process_job(
    job: &TranslateJob,
    queues: &Queues,
    shared: &Shared,
    store: &SubtitleStore,
    client: &LlamaClient,
    retry_max: u32,
    glossary: &Glossary,
    context: &mut VecDeque<ContextItem>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let text = preprocess::process(&job.text, glossary);
    let stats = shared.translate_stats();
    let mut retries = 0u32;
    // 重试仅针对传输/解析失败（连接、超时、HTTP 状态、坏响应）；
    // 质检拒绝不重试（旧带增强提示词的重试留 FUTURE）。
    let completion = loop {
        match client.translate(&text, stop) {
            Ok(completion) => {
                stats.attempt_ok();
                break Some(completion);
            }
            Err(reason) => {
                stats.attempt_fail(&reason);
                if stop.load(Relaxed) {
                    break None;
                }
                if retries < retry_max {
                    retries += 1;
                    stats.add_retry();
                    continue;
                }
                stats.add_failed();
                shared.degrade(DEGRADED_TRANSLATE_FAILED);
                eprintln!("mozart-bridge: translate {}：{reason}", job.utterance_id);
                break None;
            }
        }
    };
    let Some(completion) = completion else {
        return Ok(());
    };
    if stop.load(Relaxed) {
        return Ok(());
    }
    let content = completion.content.trim();
    if let Err(reason) = quality::check(&job.text, content, completion.finish_reason.as_deref(), context.make_contiguous()) {
        stats.add_qc_rejected();
        eprintln!("mozart-bridge: qc reject {}：{reason}", job.utterance_id);
        return Ok(());
    }
    // 先落字幕再投递（SPEC-05：译文存档先于播报）。
    store.publish_translation(&job.utterance_id, content, shared)?;
    stats.add_translated();
    shared.resolve(DEGRADED_TRANSLATE_FAILED);
    // 记入上下文供后续句的重复检查；只保留最近若干句对。
    context.push_back(ContextItem {
        source: job.text.clone(),
        translation: content.to_string(),
    });
    while context.len() > CONTEXT_HISTORY {
        context.pop_front();
    }
    if shared.speech_enabled() {
        queues.push_speech(SpeechJob {
            utterance_id: job.utterance_id.clone(),
            text: content.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Switches, STATE_FILE_NAME};
    use crate::subtitles::JsonlWriter;
    use crate::testutil::{http_response, temp_dir, MockLlama};
    use serde_json::Value;
    use std::path::Path;

    struct Fixture {
        dir: std::path::PathBuf,
        queues: Arc<Queues>,
        shared: Arc<Shared>,
        store: Arc<SubtitleStore>,
        stop: Arc<AtomicBool>,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = temp_dir(tag);
            let path = dir.join("subtitles.jsonl");
            let shared = Arc::new(Shared::new(
                Switches { translate_enabled: true, speech_enabled: true },
                path.clone(),
                dir.join(STATE_FILE_NAME),
            ));
            let store = Arc::new(SubtitleStore::new(
                JsonlWriter::open(&path).unwrap(),
                path,
            ));
            Self {
                dir,
                queues: Arc::new(Queues::new(4, 4)),
                shared,
                store,
                stop: Arc::new(AtomicBool::new(false)),
            }
        }

        /// 落一条 final 记录（模拟字幕线程先落盘再入队的顺序）。
        fn final_record(&self, utterance_id: &str, text: &str) {
            let event = crate::events::parse(
                &serde_json::to_vec(&serde_json::json!({
                    "type": "final",
                    "startup_id": "s",
                    "utterance_id": utterance_id,
                    "text": text,
                }))
                .unwrap(),
            )
            .unwrap()
            .unwrap();
            self.store.apply_event(&event, &self.shared).unwrap();
        }

        fn lines(&self) -> Vec<Value> {
            std::fs::read_to_string(self.dir.join("subtitles.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        /// 拉起工作线程；测试结束前置停止标志并 join。
        fn spawn_worker(&self, socket: &Path, retry_max: u32) -> std::thread::JoinHandle<()> {
            let queues = self.queues.clone();
            let shared = self.shared.clone();
            let store = self.store.clone();
            let stop = self.stop.clone();
            let client = LlamaClient::new(socket.to_path_buf());
            std::thread::spawn(move || run(queues, shared, store, client, retry_max, &stop).unwrap())
        }

        fn wait_for(&self, what: &str, pred: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !pred() {
                assert!(std::time::Instant::now() < deadline, "等待超时：{what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn llama_ok(body: &str) -> String {
        http_response(200, &format!("{{\"choices\":[{{\"message\":{{\"content\":\"{body}\"}}}}]}}"))
    }

    #[test]
    fn success_path_publishes_and_enqueues_speech() {
        let fixture = Fixture::new("tr-ok");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        let server = mock.spawn(vec![llama_ok("Hello.")]);

        fixture.final_record("s:1", "你好");
        fixture.queues.push_translate(TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
        });
        let worker = fixture.spawn_worker(&socket, 2);
        fixture.wait_for("译文入待播报队列", || fixture.queues.speech_depth() == 1);
        fixture.stop.store(true, Relaxed);
        worker.join().unwrap();
        server.join().unwrap();

        let job = fixture.queues.recv_speech(Duration::from_millis(100)).unwrap();
        assert_eq!(job.utterance_id, "s:1");
        assert_eq!(job.text, "Hello.");
        let lines = fixture.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["translation"], "Hello.");
        assert_eq!(lines[1]["revision"], 1);
        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.translated, 1);
        assert_eq!(status.translate_failed, 0);
        assert_eq!(status.translate_retried, 0);
        assert!(status.llama_connected);
        assert!(status.last_translate_ok_unix_ms > 0);
        assert!(status.degraded.is_empty());
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn retry_then_success_counts_retries() {
        let fixture = Fixture::new("tr-retry");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        let server = mock.spawn(vec![
            http_response(500, "{}"),
            http_response(500, "{}"),
            llama_ok("Hi."),
        ]);
        fixture.final_record("s:1", "你好");
        let job = TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
        };
        let client = LlamaClient::new(socket);
        let mut context = VecDeque::new();
        process_job(
            &job,
            &fixture.queues,
            &fixture.shared,
            &fixture.store,
            &client,
            2,
            &Glossary::empty(),
            &mut context,
            &fixture.stop,
        )
        .unwrap();
        server.join().unwrap();

        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.translate_retried, 2);
        assert_eq!(status.translated, 1);
        assert_eq!(status.translate_failed, 0);
        assert!(status.llama_connected);
        assert_eq!(fixture.queues.speech_depth(), 1);
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn failure_after_retries_degrades_and_skips_speech() {
        let fixture = Fixture::new("tr-fail");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        let server = mock.spawn(vec![
            http_response(500, "{}"),
            http_response(500, "{}"),
            http_response(500, "{}"),
        ]);
        fixture.final_record("s:1", "你好");
        let job = TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
        };
        let client = LlamaClient::new(socket);
        let mut context = VecDeque::new();
        process_job(
            &job,
            &fixture.queues,
            &fixture.shared,
            &fixture.store,
            &client,
            2,
            &Glossary::empty(),
            &mut context,
            &fixture.stop,
        )
        .unwrap();
        server.join().unwrap();

        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.translate_failed, 1);
        assert_eq!(status.translate_retried, 2);
        assert_eq!(status.translated, 0);
        assert!(!status.llama_connected);
        assert!(status.last_translate_fail_unix_ms > 0);
        assert!(status
            .last_translate_fail_reason
            .as_deref()
            .unwrap()
            .contains("HTTP 500"));
        assert!(status
            .degraded
            .iter()
            .any(|d| d.starts_with(DEGRADED_TRANSLATE_FAILED)));
        // 失败句不投递、不落译文。
        assert_eq!(fixture.queues.speech_depth(), 0);
        assert_eq!(fixture.lines().len(), 1);
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn qc_rejection_counts_and_blocks_delivery() {
        let fixture = Fixture::new("tr-qc");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        // 译文与原文相同：质检拒绝。
        let server = mock.spawn(vec![llama_ok("你好")]);
        fixture.final_record("s:1", "你好");
        let job = TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
        };
        let client = LlamaClient::new(socket);
        let mut context = VecDeque::new();
        process_job(
            &job,
            &fixture.queues,
            &fixture.shared,
            &fixture.store,
            &client,
            2,
            &Glossary::empty(),
            &mut context,
            &fixture.stop,
        )
        .unwrap();
        server.join().unwrap();

        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.qc_rejected, 1);
        assert_eq!(status.translated, 0);
        assert_eq!(fixture.queues.speech_depth(), 0);
        assert_eq!(fixture.lines().len(), 1);
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn translate_off_skips_queued_jobs() {
        let fixture = Fixture::new("tr-off");
        fixture.shared.set_switches(Switches {
            translate_enabled: false,
            speech_enabled: true,
        })
        .unwrap();
        fixture.queues.push_translate(TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
        });
        // llama socket 不存在：若误调用会立即失败计数，可据此断言未调用。
        let worker = fixture.spawn_worker(&fixture.dir.join("missing.sock"), 2);
        fixture.wait_for("在队句被跳过", || fixture.queues.translate_depth() == 0);
        fixture.stop.store(true, Relaxed);
        worker.join().unwrap();
        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.translated, 0);
        assert_eq!(status.translate_failed, 0);
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }
}