//! 翻译调度线程（SPEC-05）：消费待翻译队列的 final 句，
//! 数字归一化与术语匹配 → 目标语直通或 llama-server 推理 → 质检（满足变化条件时轻量重试 1 次）→
//! 落字幕（译文修订）→ 入待播报队列。
//! 传输失败重试与质检轻量重试共享 `retry_max` 预算（配置上限 2）；传输重试耗尽计失败并记降级原因。
//! 质检拒绝仅在尚有重试预算且重试有实质变化（去掉已注入的非空上下文，或模型 `temperature > 0.0`）时
//! 执行 1 次轻量重试；否则或重试仍不合格时计 `qc_rejected` 并落 failed，不投递播报。
//! translate off 时跳过在队句（原文已发布）。
//! 写字幕失败与字幕线程同纪律：致命退出，由 daemon 重启。

use crate::checks::ContextItem;
use crate::http::{LlamaClient, TranslateRequest};
use crate::preprocess::{self, Glossary};
use crate::quality;
use crate::state::{Queues, Shared, SpeechJob, TranslateJob};
use crate::subtitles::SubtitleStore;
use mozart_core::config::LLM_CONTEXT_WINDOW_MAX;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 取件轮询步长：让停止标志在宽限期内生效。
const RECV_POLL: Duration = Duration::from_millis(100);

/// 降级原因：翻译重试耗尽（整句成功时解除；具体原因见最近失败原因字段）。
pub const DEGRADED_TRANSLATE_FAILED: &str = "translation failed";

/// 目标语（纯英文）判定：不含任何非 ASCII 字母（如中文/日文假名/韩文谚文/西里尔等）
/// 且包含 ASCII 字母时直接放行，不经中译英 LLM。
fn is_target_language(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_alphabetic() && !c.is_ascii_alphabetic())
        && text.chars().any(|c| c.is_ascii_alphabetic())
}

/// 翻译线程主循环：停止标志置位后退出，在途请求由客户端按停止标志放弃。
pub fn run(
    queues: Arc<Queues>,
    shared: Arc<Shared>,
    store: Arc<SubtitleStore>,
    client: LlamaClient,
    glossary: Glossary,
    retry_max: u32,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut context: VecDeque<ContextItem> = VecDeque::new();
    while !stop.load(Relaxed) {
        let job = match queues.recv_translate(RECV_POLL) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => continue,
            // 字幕线程已退出（队列随进程销毁）。
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // 入队后关闭翻译：只保留已发布的原文，标 skipped，不调 llama。
        if !shared.translate_enabled() {
            store.publish_translation_skipped(&job.utterance_id, "translate disabled", &shared)?;
            continue;
        }
        process_job(
            &job,
            &queues,
            &shared,
            &store,
            &client,
            retry_max,
            &glossary,
            &mut context,
            stop,
        )?;
    }
    Ok(())
}

/// 译文落盘（先于播报）、记入上下文历史并按代次入待播报队列。
fn publish_and_enqueue(
    job: &TranslateJob,
    translation: &str,
    translate_ms: u64,
    queues: &Queues,
    shared: &Shared,
    store: &SubtitleStore,
    context: &mut VecDeque<ContextItem>,
) -> Result<(), String> {
    // 先落字幕再投递（SPEC-05：译文存档先于播报）。
    store.publish_translation(&job.utterance_id, translation, translate_ms, shared)?;
    shared.translate_stats().add_translated();
    // 记入上下文供后续句的提示注入与重复检查；容量复用配置上限（单一定义源）。
    context.push_back(ContextItem {
        source: job.text.clone(),
        translation: translation.to_string(),
    });
    while context.len() > LLM_CONTEXT_WINDOW_MAX {
        context.pop_front();
    }
    if let Some(generation) = job
        .speech_generation
        .filter(|&value| Some(value) == shared.speech_generation())
    {
        queues.push_speech(SpeechJob {
            utterance_id: job.utterance_id.clone(),
            text: translation.to_string(),
            generation,
        });
    }
    Ok(())
}

/// 单句管线。Err 仅来自字幕写盘失败（致命）；推理失败与质检拒绝在此计数消化。
#[allow(clippy::too_many_arguments)]
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
    // 字幕线程入队前已完成合并纠错表与术语同音纠错，此处仅做数字归一化与术语提示匹配，
    // 避免重复构建同音索引或以内置表覆盖自定义 corrections_path。
    let text = preprocess::normalize_numbers(&job.text);
    let stats = shared.translate_stats();
    let started = Instant::now();
    // 源文本身已是目标语（如多语种复核还原出的纯英文句）：直接落字幕并播报，不经中译英 LLM。
    let trimmed_source = text.trim();
    if is_target_language(trimmed_source) {
        publish_and_enqueue(
            job,
            trimmed_source,
            started.elapsed().as_millis() as u64,
            queues,
            shared,
            store,
            context,
        )?;
        return Ok(());
    }
    let matched_glossary = glossary.match_terms(&text);
    let mut retries = 0u32;
    let mut last_reason = String::new();
    let completion = loop {
        let request = TranslateRequest {
            text: &text,
            context: context.make_contiguous(),
            glossary: &matched_glossary,
        };
        match client.translate_with(&request, stop) {
            Ok(completion) => {
                stats.attempt_ok();
                break Some(completion);
            }
            Err(reason) => {
                stats.attempt_fail(&reason);
                if stop.load(Relaxed) {
                    break None;
                }
                last_reason = reason.clone();
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
        // 停止放弃不标失败；重试耗尽标 failed 并记原因。
        if !stop.load(Relaxed) {
            store.publish_translation_failed(&job.utterance_id, &last_reason, shared)?;
        }
        return Ok(());
    };
    if stop.load(Relaxed) {
        return Ok(());
    }
    let mut final_content = completion.content.trim().to_string();
    let mut qc_result = quality::check(
        &job.text,
        &final_content,
        completion.finish_reason.as_deref(),
        context.make_contiguous(),
    );
    if let Err(first_reason) = qc_result {
        if retries < retry_max && client.qc_retry_has_effect(context.make_contiguous()) {
            stats.add_retry();
            // 质检拒绝时去掉历史上下文轻量重试 1 次，防止上下文污染造成重复或长度异常。
            let retry_request = TranslateRequest {
                text: &text,
                context: &[],
                glossary: &matched_glossary,
            };
            match client.translate_with(&retry_request, stop) {
                Ok(retry_completion) => {
                    stats.attempt_ok();
                    if stop.load(Relaxed) {
                        return Ok(());
                    }
                    final_content = retry_completion.content.trim().to_string();
                    qc_result = quality::check(
                        &job.text,
                        &final_content,
                        retry_completion.finish_reason.as_deref(),
                        context.make_contiguous(),
                    );
                }
                Err(transport_err) => {
                    stats.attempt_fail(&transport_err);
                    if stop.load(Relaxed) {
                        return Ok(());
                    }
                    qc_result = Err(format!("{first_reason}; retry failed: {transport_err}"));
                }
            }
        } else {
            qc_result = Err(first_reason);
        }
    }
    if let Err(reason) = qc_result {
        stats.add_qc_rejected();
        eprintln!("mozart-bridge: qc reject {}：{reason}", job.utterance_id);
        store.publish_translation_failed(&job.utterance_id, &reason, shared)?;
        return Ok(());
    }
    let translate_ms = started.elapsed().as_millis() as u64;
    publish_and_enqueue(
        job,
        &final_content,
        translate_ms,
        queues,
        shared,
        store,
        context,
    )?;
    shared.resolve(DEGRADED_TRANSLATE_FAILED);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Switches;
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
                Switches {
                    translate_enabled: true,
                    speech_enabled: true,
                },
                path.clone(),
            ));
            let store = Arc::new(SubtitleStore::new(
                JsonlWriter::open(&path).unwrap(),
                path,
                Vec::new(),
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
            std::thread::spawn(move || {
                run(
                    queues,
                    shared,
                    store,
                    client,
                    Glossary::empty(),
                    retry_max,
                    &stop,
                )
                .unwrap()
            })
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
        http_response(
            200,
            &format!("{{\"choices\":[{{\"message\":{{\"content\":\"{body}\"}}}}]}}"),
        )
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
            speech_generation: fixture.shared.speech_generation(),
        });
        let worker = fixture.spawn_worker(&socket, 2);
        fixture.wait_for("译文入待播报队列", || {
            fixture.queues.speech_depth() == 1
        });
        fixture.stop.store(true, Relaxed);
        worker.join().unwrap();
        server.join().unwrap();

        let job = fixture.queues.recv_speech(Duration::from_millis(100)).unwrap();
        assert_eq!(job.utterance_id, "s:1");
        assert_eq!(job.text, "Hello.");
        let lines = fixture.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["en"], "Hello.");
        assert_eq!(lines[1]["translation_status"], "completed");
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
    fn in_flight_translation_finishes_after_off_on_without_old_speech() {
        let fixture = Fixture::new("tr-generation");
        let socket = fixture.dir.join("llama.sock");
        let (seen_tx, seen) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let server =
            MockLlama::bind(&socket).spawn_gated_reply(llama_ok("Hello."), seen_tx, release_rx);
        fixture.final_record("s:1", "你好");
        fixture.queues.push_translate(TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
            speech_generation: fixture.shared.speech_generation(),
        });
        let worker = fixture.spawn_worker(&socket, 0);
        seen.recv_timeout(Duration::from_secs(2)).unwrap();
        for speech_enabled in [false, true] {
            fixture.shared.set_switches(Switches {
                translate_enabled: true,
                speech_enabled,
            });
        }
        release.send(()).unwrap();
        fixture.wait_for("旧句译文仍落盘", || {
            fixture.shared.status(&fixture.queues).translated == 1
        });
        fixture.stop.store(true, Relaxed);
        worker.join().unwrap();
        server.join().unwrap();
        assert_eq!(fixture.lines()[1]["en"], "Hello.");
        assert_eq!(fixture.queues.speech_depth(), 0);
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
            speech_generation: fixture.shared.speech_generation(),
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
            speech_generation: fixture.shared.speech_generation(),
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
        // 失败句不投递；落 failed 修订（en 仍空，原因可查）。
        assert_eq!(fixture.queues.speech_depth(), 0);
        let lines = fixture.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["translation_status"], "failed");
        assert_eq!(lines[1]["en"], "");
        assert!(lines[1]["translation_error"].as_str().unwrap().contains("HTTP 500"));
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn qc_rejection_counts_and_blocks_delivery() {
        let fixture = Fixture::new("tr-qc");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        // 确定性模型（temperature=0）且未带上下文时，重发逐字节相同请求无意义，直接计 qc_rejected。
        let server = mock.spawn(vec![llama_ok("你好")]);
        fixture.final_record("s:1", "你好");
        let job = TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
            speech_generation: fixture.shared.speech_generation(),
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
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);

        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.qc_rejected, 1);
        assert_eq!(status.translate_retried, 0);
        assert_eq!(status.translated, 0);
        assert_eq!(fixture.queues.speech_depth(), 0);
        let lines = fixture.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["translation_status"], "failed");
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn qc_rejection_retries_once_without_context_and_recovers() {
        use mozart_core::config::{LlmModelConfig, PromptProfile};
        let fixture = Fixture::new("tr-qc-retry-ok");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        // 首次译文重复了上文译文触发质检拒绝；重试时去掉上下文后产出合格译文。
        let server = mock.spawn(vec![llama_ok("The result is a loss."), llama_ok("Nobody can escape.")]);
        fixture.final_record("s:2", "没有人能逃掉");
        let job = TranslateJob {
            utterance_id: "s:2".to_string(),
            text: "没有人能逃掉".to_string(),
            speech_generation: fixture.shared.speech_generation(),
        };
        let model = LlmModelConfig {
            id: "hymt".to_string(),
            path: std::path::PathBuf::from("/models/hymt.gguf"),
            prompt_profile: PromptProfile::HyMt,
            context_window: 2,
            ..LlmModelConfig::default()
        };
        let client = LlamaClient::with_model(socket, model);
        let mut context = VecDeque::from([ContextItem {
            source: "最后的结果一定是输".to_string(),
            translation: "The result is a loss.".to_string(),
        }]);
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
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        let first = String::from_utf8(requests[0].clone()).unwrap();
        let second = String::from_utf8(requests[1].clone()).unwrap();
        assert!(first.contains("最后的结果一定是输"), "首次请求应含上文：{first}");
        assert!(!second.contains("最后的结果一定是输"), "质检重试应去掉上文：{second}");

        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.qc_rejected, 0);
        assert_eq!(status.translate_retried, 1);
        assert_eq!(status.translated, 1);
        assert_eq!(fixture.queues.speech_depth(), 1);
        assert_eq!(fixture.lines()[1]["en"], "Nobody can escape.");
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn qc_rejection_after_effective_retry_still_fails_and_counts() {
        use mozart_core::config::{LlmModelConfig, PromptProfile};
        let fixture = Fixture::new("tr-qc-retry-fail");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        let server = mock.spawn(vec![llama_ok("你好"), llama_ok("你好")]);
        fixture.final_record("s:2", "你好");
        let job = TranslateJob {
            utterance_id: "s:2".to_string(),
            text: "你好".to_string(),
            speech_generation: fixture.shared.speech_generation(),
        };
        let model = LlmModelConfig {
            id: "hymt".to_string(),
            path: std::path::PathBuf::from("/models/hymt.gguf"),
            prompt_profile: PromptProfile::HyMt,
            context_window: 2,
            ..LlmModelConfig::default()
        };
        let client = LlamaClient::with_model(socket, model);
        let mut context = VecDeque::from([ContextItem {
            source: "上文".to_string(),
            translation: "Previous.".to_string(),
        }]);
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
        assert_eq!(server.join().unwrap().len(), 2);
        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.qc_rejected, 1);
        assert_eq!(status.translate_retried, 1);
        assert_eq!(status.translated, 0);
        assert_eq!(fixture.queues.speech_depth(), 0);
        assert_eq!(fixture.lines()[1]["translation_status"], "failed");
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn qc_rejection_retries_when_temperature_is_nonzero() {
        use mozart_core::config::{LlmModelConfig, PromptProfile};
        let fixture = Fixture::new("tr-qc-temp");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        let server = mock.spawn(vec![llama_ok("你好"), llama_ok("Hello.")]);
        fixture.final_record("s:1", "你好");
        let job = TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
            speech_generation: fixture.shared.speech_generation(),
        };
        let model = LlmModelConfig {
            id: "hymt".to_string(),
            path: std::path::PathBuf::from("/models/hymt.gguf"),
            prompt_profile: PromptProfile::HyMt,
            temperature: 0.2,
            ..LlmModelConfig::default()
        };
        let client = LlamaClient::with_model(socket, model);
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
        assert_eq!(server.join().unwrap().len(), 2);
        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.qc_rejected, 0);
        assert_eq!(status.translate_retried, 1);
        assert_eq!(status.translated, 1);
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn qc_rejection_skips_retry_when_retry_max_is_zero() {
        use mozart_core::config::{LlmModelConfig, PromptProfile};
        let fixture = Fixture::new("tr-qc-no-retry");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        let server = mock.spawn(vec![llama_ok("你好")]);
        fixture.final_record("s:2", "你好");
        let job = TranslateJob {
            utterance_id: "s:2".to_string(),
            text: "你好".to_string(),
            speech_generation: fixture.shared.speech_generation(),
        };
        // 携带上下文使重试本可生效，验证 retry_max=0 明确阻止重试。
        let model = LlmModelConfig {
            id: "hymt".to_string(),
            path: std::path::PathBuf::from("/models/hymt.gguf"),
            prompt_profile: PromptProfile::HyMt,
            context_window: 2,
            ..LlmModelConfig::default()
        };
        let client = LlamaClient::with_model(socket, model);
        let mut context = VecDeque::from([ContextItem {
            source: "上文".to_string(),
            translation: "Previous.".to_string(),
        }]);
        process_job(
            &job,
            &fixture.queues,
            &fixture.shared,
            &fixture.store,
            &client,
            0,
            &Glossary::empty(),
            &mut context,
            &fixture.stop,
        )
        .unwrap();
        assert_eq!(server.join().unwrap().len(), 1);
        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.qc_rejected, 1);
        assert_eq!(status.translate_retried, 0);
        assert_eq!(status.translated, 0);
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn pure_english_passes_through_without_calling_llm() {
        let fixture = Fixture::new("tr-en-pass");
        fixture.final_record("s:1", "Hello world, see you at 3 pm.");
        let job = TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "Hello world, see you at 3 pm.".to_string(),
            speech_generation: fixture.shared.speech_generation(),
        };
        // socket 不存在：若误调 LLM 会连接失败。
        let client = LlamaClient::new(fixture.dir.join("missing.sock"));
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
        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.translated, 1);
        assert_eq!(status.translate_failed, 0);
        assert_eq!(status.qc_rejected, 0);
        assert_eq!(fixture.lines()[1]["en"], "Hello world, see you at 3 pm.");
        assert_eq!(fixture.lines()[1]["translation_status"], "completed");
        let speech = fixture.queues.recv_speech(Duration::from_millis(100)).unwrap();
        assert_eq!(speech.text, "Hello world, see you at 3 pm.");
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn non_ascii_scripts_mixed_with_english_still_use_llm() {
        // 中文、日文假名、韩文谚文、西里尔字母夹杂 ASCII 均不得被误判为纯英文直通。
        for (idx, (source, reply)) in [
            ("这个叫PF", "This is called PF."),
            ("はい OK", "Yes, OK."),
            ("네 A", "Yes, A."),
            ("Привет OK", "Hello, OK."),
        ]
        .into_iter()
        .enumerate()
        {
            assert!(!is_target_language(source), "不得直通：{source}");
            let fixture = Fixture::new(&format!("tr-mixed-{idx}"));
            let socket = fixture.dir.join("llama.sock");
            let mock = MockLlama::bind(&socket);
            let server = mock.spawn(vec![llama_ok(reply)]);
            fixture.final_record("s:1", source);
            let job = TranslateJob {
                utterance_id: "s:1".to_string(),
                text: source.to_string(),
                speech_generation: fixture.shared.speech_generation(),
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
            assert_eq!(server.join().unwrap().len(), 1, "必须调 LLM：{source}");
            assert_eq!(fixture.lines()[1]["en"], reply);
            let _ = std::fs::remove_dir_all(&fixture.dir);
        }
    }

    #[test]
    fn process_job_does_not_overwrite_custom_corrections_and_normalizes_numbers() {
        let fixture = Fixture::new("tr-no-recorrect");
        let socket = fixture.dir.join("llama.sock");
        let mock = MockLlama::bind(&socket);
        let server = mock.spawn(vec![llama_ok("Spent 27,000 yuan.")]);
        // 模拟上游已按自定义纠错保留「在见」，translate 只归一化数字不重复按内置表改写。
        fixture.final_record("s:1", "在见花了两万七元");
        let job = TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "在见花了两万七元".to_string(),
            speech_generation: fixture.shared.speech_generation(),
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
        let requests = server.join().unwrap();
        let body = String::from_utf8(requests[0].clone()).unwrap();
        assert!(body.contains("在见花了27000元"), "应归一化数字且保留上游文本：{body}");
        assert!(!body.contains("再见"), "不得重复按内置表覆盖：{body}");
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn translate_off_skips_queued_jobs() {
        let fixture = Fixture::new("tr-off");
        fixture.shared.set_switches(Switches {
            translate_enabled: false,
            speech_enabled: true,
        });
        fixture.final_record("s:1", "你好");
        fixture.queues.push_translate(TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
            speech_generation: fixture.shared.speech_generation(),
        });
        // llama socket 不存在：若误调用会立即失败计数，可据此断言未调用。
        let worker = fixture.spawn_worker(&fixture.dir.join("missing.sock"), 2);
        fixture.wait_for("在队句被跳过", || {
            fixture.queues.translate_depth() == 0
        });
        fixture.stop.store(true, Relaxed);
        worker.join().unwrap();
        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.translated, 0);
        assert_eq!(status.translate_failed, 0);
        // 跳过句落 skipped 修订。
        let lines = fixture.lines();
        assert_eq!(lines[1]["translation_status"], "skipped");
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }
}
