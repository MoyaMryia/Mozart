//! 播报投递线程（SPEC-05）：消费待播报队列，经 tts-jobs.sock（SEQPACKET + core 信封）
//! 把合格译文 + utterance_id 发给 TTS，按受理应答计 submitted/skipped/failed。
//! 断连自动重连，在途句重连后重发；speech off 停新投递并静默清空在队句
//! （已送达 TTS 的在途任务自然播完）；speech stop 的清队列由控制面执行。
//! 队满丢最旧在入队侧计数（Queues::push_speech）。

use crate::state::{Queues, Shared, SpeechJob};
use mozart_core::ctl::{self, Seqpacket};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

/// 降级原因：TTS 任务面断连（含连接失败），重连成功后移除。
pub const DEGRADED_TTS_JOBS: &str = "tts jobs disconnected";

/// 取件轮询步长：让停止标志与开关在宽限期内生效。
const QUEUE_POLL: Duration = Duration::from_millis(100);
/// 断连后的重连重试间隔。
const CONNECT_RETRY: Duration = Duration::from_millis(500);
/// 单次 connect 等待上限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// 发送与受理应答的等待上限：TTS 受理只做入队，正常即时返回；
/// 两段相加须留在 daemon 2 s 宽限内。
const IO_TIMEOUT: Duration = Duration::from_millis(500);
/// 重连重试的停止检查步长。
const STOP_CHECK: Duration = Duration::from_millis(50);

/// TTS 受理状态（沿用旧语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Submitted,
    Skipped,
    Failed,
}

/// TTS_JOB_REPLY 应答体：status 必填，其余字段忽略。
#[derive(Debug, Deserialize)]
struct ReplyBody {
    status: String,
    utterance_id: String,
    #[serde(default)]
    reason: Option<String>,
}

/// 投递线程主循环：停止标志置位后退出，在途请求随连接丢弃。
pub fn run(
    queues: Arc<Queues>,
    shared: Arc<Shared>,
    path: PathBuf,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut conn: Option<Seqpacket> = None;
    let mut request_id: u32 = 0;
    let mut pending: Option<SpeechJob> = None;
    while !stop.load(Relaxed) {
        if !shared.speech_enabled() {
            // 停新投递：静默清空在队句；已送达的在途任务自然播完。
            queues.clear_speech();
            pending = None;
            sleep_checked(QUEUE_POLL, stop);
            continue;
        }
        let job = match pending.take() {
            Some(job) => job,
            None => match queues.recv_speech(QUEUE_POLL) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => continue,
                // 队列随进程销毁。
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        if conn.is_none() {
            match Seqpacket::connect_timeout(&path, Some(CONNECT_TIMEOUT)) {
                Ok(fresh) => {
                    shared.speech_stats().set_connected(true);
                    shared.resolve(DEGRADED_TTS_JOBS);
                    conn = Some(fresh);
                }
                Err(err) => {
                    shared.speech_stats().set_connected(false);
                    shared.degrade(DEGRADED_TTS_JOBS);
                    eprintln!("mozart-bridge: connect {}：{err}", path.display());
                    pending = Some(job);
                    sleep_checked(CONNECT_RETRY, stop);
                    continue;
                }
            }
        }
        let current = conn.as_ref().expect("connection established above");
        request_id = request_id.wrapping_add(1);
        match submit(current, request_id, &job) {
            Ok(status) => {
                let stats = shared.speech_stats();
                stats.add_delivered();
                match status {
                    JobStatus::Submitted => stats.add_submitted(),
                    JobStatus::Skipped => stats.add_skipped(),
                    JobStatus::Failed => stats.add_failed(),
                }
            }
            Err(reason) => {
                eprintln!("mozart-bridge: tts submit {}：{reason}", job.utterance_id);
                shared.speech_stats().set_connected(false);
                shared.degrade(DEGRADED_TTS_JOBS);
                conn = None;
                pending = Some(job);
            }
        }
    }
    Ok(())
}

/// 发一条任务并等受理应答；入口校验应答类型、句编号与状态取值。
fn submit(conn: &Seqpacket, request_id: u32, job: &SpeechJob) -> Result<JobStatus, String> {
    let body = serde_json::to_vec(&json!({
        "text": job.text,
        "utterance_id": job.utterance_id,
    }))
    .expect("speech job is JSON-serializable");
    conn.send_timeout(ctl::TTS_JOB_SUBMIT, request_id, &body, Some(IO_TIMEOUT))
        .map_err(|err| format!("send：{err}"))?;
    let reply = conn
        .recv(Some(IO_TIMEOUT))
        .map_err(|err| format!("recv：{err}"))?;
    let reply_type = reply.header.type_;
    if reply_type != ctl::TTS_JOB_REPLY {
        return Err(format!("unexpected reply type 0x{reply_type:04x}"));
    }
    let parsed: ReplyBody =
        serde_json::from_slice(&reply.body).map_err(|err| format!("invalid reply JSON: {err}"))?;
    if parsed.utterance_id != job.utterance_id {
        return Err(format!(
            "reply utterance_id mismatch: {}",
            parsed.utterance_id
        ));
    }
    match parsed.status.as_str() {
        "submitted" => Ok(JobStatus::Submitted),
        "skipped" => Ok(JobStatus::Skipped),
        "failed" => {
            if let Some(reason) = &parsed.reason {
                eprintln!("mozart-bridge: tts failed {}：{reason}", job.utterance_id);
            }
            Ok(JobStatus::Failed)
        }
        other => Err(format!("unknown job status: {other}")),
    }
}

/// speech stop：向 TTS 下发 TTS_JOB_STOP（清任务队列并中止在途合成，TTS 侧处理见 Wave 2）。
/// 尽力而为：TTS 缺席或拒收只记降级，不阻断桥侧关开关与清队列。
pub fn send_stop(path: &Path, shared: &Shared) {
    let result = Seqpacket::connect_timeout(path, Some(CONNECT_TIMEOUT))
        .map_err(|err| format!("connect {}：{err}", path.display()))
        .and_then(|conn| {
            conn.send_timeout(ctl::TTS_JOB_STOP, 0, b"", Some(IO_TIMEOUT))
                .map_err(|err| format!("send：{err}"))
        });
    if let Err(reason) = result {
        shared.degrade(DEGRADED_TTS_JOBS);
        eprintln!("mozart-bridge: tts stop：{reason}");
    }
}

fn sleep_checked(total: Duration, stop: &AtomicBool) {
    let mut remaining = total;
    while !remaining.is_zero() && !stop.load(Relaxed) {
        let step = STOP_CHECK.min(remaining);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Switches, STATE_FILE_NAME};
    use crate::testutil::{temp_dir, MockTts};
    use std::path::Path;

    struct Fixture {
        dir: PathBuf,
        queues: Arc<Queues>,
        shared: Arc<Shared>,
        stop: Arc<AtomicBool>,
    }

    impl Fixture {
        fn new(tag: &str, speech_enabled: bool) -> Self {
            let dir = temp_dir(tag);
            let shared = Arc::new(Shared::new(
                Switches { translate_enabled: true, speech_enabled },
                dir.join("subtitles.jsonl"),
                dir.join(STATE_FILE_NAME),
            ));
            Self {
                dir,
                queues: Arc::new(Queues::new(4, 4)),
                shared,
                stop: Arc::new(AtomicBool::new(false)),
            }
        }

        fn push(&self, seq: u64) {
            assert!(self.queues.push_speech(SpeechJob {
                utterance_id: format!("s:{seq}"),
                text: format!("hello {seq}"),
            }));
        }

        fn spawn_worker(&self, path: &Path) -> std::thread::JoinHandle<()> {
            let queues = self.queues.clone();
            let shared = self.shared.clone();
            let stop = self.stop.clone();
            let path = path.to_path_buf();
            std::thread::spawn(move || run(queues, shared, path, &stop).unwrap())
        }

        fn wait_for(&self, what: &str, pred: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !pred() {
                assert!(std::time::Instant::now() < deadline, "等待超时：{what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn finish(&self, worker: std::thread::JoinHandle<()>) {
            self.stop.store(true, Relaxed);
            worker.join().unwrap();
        }
    }

    #[test]
    fn submitted_skipped_failed_are_counted() {
        let fixture = Fixture::new("sp-status", true);
        let path = fixture.dir.join("tts-jobs.sock");
        let mock = MockTts::bind(&path);
        let (jobs, server) = mock.spawn(vec!["submitted", "skipped", "failed"]);

        fixture.push(1);
        fixture.push(2);
        fixture.push(3);
        let worker = fixture.spawn_worker(&path);
        fixture.wait_for("三条受理计数", || {
            let status = fixture.shared.status(&fixture.queues);
            status.speech_delivered == 3
        });
        fixture.finish(worker);
        server.join().unwrap();

        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.speech_submitted, 1);
        assert_eq!(status.speech_skipped, 1);
        assert_eq!(status.speech_failed, 1);
        assert!(status.tts_connected);
        assert!(status.degraded.is_empty());
        let jobs = jobs.lock().unwrap();
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0]["text"], "hello 1");
        assert_eq!(jobs[0]["utterance_id"], "s:1");
        assert_eq!(jobs[2]["utterance_id"], "s:3");
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn reconnect_resends_pending_job() {
        let fixture = Fixture::new("sp-reconnect", true);
        let path = fixture.dir.join("tts-jobs.sock");
        // 第一代 TTS：受理一句后关闭连接（模拟重启）。
        let mock = MockTts::bind(&path);
        let (jobs1, server1) = mock.spawn_closing(vec!["submitted"]);
        fixture.push(1);
        let worker = fixture.spawn_worker(&path);
        fixture.wait_for("第一句送达", || {
            fixture.shared.status(&fixture.queues).speech_delivered == 1
        });
        server1.join().unwrap();

        // 第二句发出时对端已关闭：降级可查，重连后重发。
        fixture.push(2);
        fixture.wait_for("断连降级可查", || {
            fixture
                .shared
                .status(&fixture.queues)
                .degraded
                .iter()
                .any(|d| d.starts_with(DEGRADED_TTS_JOBS))
        });
        let mock = MockTts::bind(&path);
        let (jobs2, server2) = mock.spawn(vec!["submitted"]);
        fixture.wait_for("重连后第二句送达", || {
            fixture.shared.status(&fixture.queues).speech_delivered == 2
        });
        fixture.finish(worker);
        server2.join().unwrap();

        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.speech_submitted, 2);
        assert!(status.tts_connected);
        assert!(status.degraded.is_empty());
        assert_eq!(jobs1.lock().unwrap().len(), 1);
        let jobs2 = jobs2.lock().unwrap();
        assert_eq!(jobs2.len(), 1);
        assert_eq!(jobs2[0]["utterance_id"], "s:2");
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn disabled_drains_queue_without_delivery() {
        let fixture = Fixture::new("sp-off", false);
        let path = fixture.dir.join("tts-jobs.sock");
        // 只绑定不应答：关闭期间工作线程不应发起连接。
        let mock = MockTts::bind(&path);

        fixture.push(1);
        fixture.push(2);
        let worker = fixture.spawn_worker(&path);
        fixture.wait_for("在队句被清空", || fixture.queues.speech_depth() == 0);
        // 关闭期间新句同样被清掉。
        fixture.push(3);
        fixture.wait_for("新句也被清空", || fixture.queues.speech_depth() == 0);
        assert!(mock.wait_connection(Duration::from_millis(300)).is_none());
        fixture.finish(worker);

        let status = fixture.shared.status(&fixture.queues);
        assert_eq!(status.speech_delivered, 0);
        assert_eq!(status.speech_dropped, 0);
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }

    #[test]
    fn tts_absent_degrades_and_holds_job() {
        let fixture = Fixture::new("sp-absent", true);
        let path = fixture.dir.join("tts-jobs.sock");
        fixture.push(1);
        let worker = fixture.spawn_worker(&path);
        fixture.wait_for("断连降级可查", || {
            fixture
                .shared
                .status(&fixture.queues)
                .degraded
                .iter()
                .any(|d| d.starts_with(DEGRADED_TTS_JOBS))
        });
        let status = fixture.shared.status(&fixture.queues);
        assert!(!status.tts_connected);
        assert_eq!(status.speech_delivered, 0);

        // TTS 上线：在途句补发，降级解除。
        let mock = MockTts::bind(&path);
        let (jobs, server) = mock.spawn(vec!["submitted"]);
        fixture.wait_for("上线后送达", || {
            fixture.shared.status(&fixture.queues).speech_delivered == 1
        });
        fixture.finish(worker);
        server.join().unwrap();
        assert_eq!(jobs.lock().unwrap()[0]["utterance_id"], "s:1");
        assert!(fixture.shared.status(&fixture.queues).degraded.is_empty());
        let _ = std::fs::remove_dir_all(&fixture.dir);
    }
}