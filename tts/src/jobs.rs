//! 任务面（SPEC-05 播报投递）：tts-jobs.sock SEQPACKET，桥连接、TTS 绑定。
//! 复用 core 16 字节信封 + JSON 体：TTS_JOB_SUBMIT 收译文任务，TTS_JOB_REPLY 回
//! 受理状态 submitted/skipped/failed；TTS_JOB_STOP 推进任务代际（清队列并中止在途
//! 合成，见 synth）。有界任务队列经 try_send 入队，队满标 skipped 并计数，不阻塞桥。
//! 桥持一条长连接投递，speech stop 另开短连接下发停止，故逐连接并发服务。
//! v1 单目标音色（配置参考 WAV），voice_params 仅校验不生效。

use crate::control::{ACCEPT_POLL, RECV_POLL};
use crate::state::Shared;
use mozart_core::ctl::{self, Message, Seqpacket};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;

/// 并发连接上限：桥一条长连接投递 + speech stop 短连接，留余量即可。
/// 本地 socket 非公网，超出拒接并记录。
const MAX_CONNECTIONS: usize = 8;

/// 一条合成任务（voice_params 校验后丢弃，v1 用配置音色）。
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub text: String,
    pub utterance_id: String,
}

/// TTS_JOB_SUBMIT 请求体。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub text: String,
    pub utterance_id: String,
    #[serde(default)]
    pub voice_params: Option<Value>,
}

/// 受理状态，沿用旧语义：submitted 已入队 / skipped 队满 / failed 请求无效或无法入队。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Submitted,
    Skipped,
    Failed,
}

/// TTS_JOB_REPLY 应答体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobReply {
    pub status: JobStatus,
    pub utterance_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
}

impl JobReply {
    pub fn failed(utterance_id: &str, reason: &str) -> Self {
        Self {
            status: JobStatus::Failed,
            utterance_id: utterance_id.to_string(),
            reason: Some(reason.to_string()),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("job reply is JSON-serializable")
    }
}

/// 入口校验一次：请求体 JSON 结构与必填字段。内部函数信任校验结果。
pub fn parse_request(body: &[u8]) -> Result<Job, String> {
    let request: JobRequest =
        serde_json::from_slice(body).map_err(|err| format!("invalid job body: {err}"))?;
    if request.text.trim().is_empty() {
        return Err("text must not be empty".to_string());
    }
    if request.utterance_id.is_empty() {
        return Err("utterance_id must not be empty".to_string());
    }
    if let Some(params) = &request.voice_params {
        if !params.is_object() {
            return Err("voice_params must be an object".to_string());
        }
    }
    Ok(Job {
        text: request.text,
        utterance_id: request.utterance_id,
    })
}

/// 服务循环：绑定与监听由调用方完成；停止标志置位后回收连接线程并返回。
/// 逐连接并发服务：桥的投递长连接空闲不断开，若单线程串行服务，speech stop
/// 的短连接会排在长连接之后永远得不到 accept，停止命令无法生效。
pub fn serve(
    listener: Seqpacket,
    tasks: SyncSender<Job>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !stop.load(Relaxed) {
        workers.retain(|handle| !handle.is_finished());
        if let Err(err) = listener.wait_readable(Some(ACCEPT_POLL)) {
            if err.kind() == io::ErrorKind::TimedOut {
                continue;
            }
            eprintln!("mozart-tts: jobs poll: {err}");
            continue;
        }
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("mozart-tts: jobs accept: {err}");
                continue;
            }
        };
        if workers.len() >= MAX_CONNECTIONS {
            eprintln!("mozart-tts: jobs 并发连接超 {MAX_CONNECTIONS}，拒接");
            drop(conn);
            continue;
        }
        let tasks = tasks.clone();
        let shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("mozart-tts-jobs-conn".to_string())
            .spawn(move || serve_connection(&conn, &tasks, &shared, stop));
        match spawned {
            Ok(handle) => workers.push(handle),
            Err(err) => eprintln!("mozart-tts: jobs 连接线程启动失败：{err}"),
        }
    }
    for handle in workers {
        let _ = handle.join();
    }
}

/// 桥是长连接，空闲不断开；对端关闭回到 accept。
fn serve_connection(conn: &Seqpacket, tasks: &SyncSender<Job>, shared: &Shared, stop: &AtomicBool) {
    while !stop.load(Relaxed) {
        let msg = match conn.recv(Some(RECV_POLL)) {
            Ok(msg) => msg,
            Err(err) if err.kind() == io::ErrorKind::TimedOut => continue,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(err) => {
                eprintln!("mozart-tts: jobs recv: {err}");
                return;
            }
        };
        let request_id = msg.header.request_id;
        let reply = dispatch(&msg, tasks, shared);
        if conn.send(ctl::TTS_JOB_REPLY, request_id, &reply).is_err() {
            return;
        }
    }
}

/// 命令分发：TTS_JOB_SUBMIT 入队（受理状态决定应答）；TTS_JOB_STOP 推进任务代际
/// （在途合成中止、在队任务由 synth 线程清空），应答 {"stopped":true}。
pub fn dispatch(msg: &Message, tasks: &SyncSender<Job>, shared: &Shared) -> Vec<u8> {
    match msg.header.type_ {
        ctl::TTS_JOB_SUBMIT => dispatch_submit(msg, tasks, shared),
        ctl::TTS_JOB_STOP => {
            // 入口校验一次：停止体为空或 {}。
            if !msg.body.is_empty() && msg.body != b"{}" {
                return JobReply::failed("", "invalid stop body").to_bytes();
            }
            shared.stop_jobs();
            serde_json::to_vec(&json!({ "stopped": true })).expect("stop reply is JSON-serializable")
        }
        _ => JobReply::failed("", "unknown command").to_bytes(),
    }
}

/// TTS_JOB_SUBMIT：入队结果决定受理状态。
fn dispatch_submit(msg: &Message, tasks: &SyncSender<Job>, shared: &Shared) -> Vec<u8> {
    let job = match parse_request(&msg.body) {
        Ok(job) => job,
        Err(reason) => return JobReply::failed("", &reason).to_bytes(),
    };
    let utterance_id = job.utterance_id.clone();
    shared.queue_push();
    match tasks.try_send(job) {
        Ok(()) => JobReply {
            status: JobStatus::Submitted,
            utterance_id,
            reason: None,
        }
        .to_bytes(),
        Err(TrySendError::Full(_)) => {
            shared.queue_pop();
            shared.add_skipped(1);
            JobReply {
                status: JobStatus::Skipped,
                utterance_id,
                reason: Some("queue full".to_string()),
            }
            .to_bytes()
        }
        Err(TrySendError::Disconnected(_)) => {
            shared.queue_pop();
            JobReply::failed(&utterance_id, "synth worker stopped").to_bytes()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mozart_core::ctl::Header;
    use std::sync::mpsc;
    use std::time::Duration;

    fn submit(body: &[u8]) -> Message {
        Message {
            header: Header::new(ctl::TTS_JOB_SUBMIT, 7, body.len() as u32),
            body: body.to_vec(),
        }
    }

    fn stop(body: &[u8]) -> Message {
        Message {
            header: Header::new(ctl::TTS_JOB_STOP, 9, body.len() as u32),
            body: body.to_vec(),
        }
    }

    fn reply_of(bytes: &[u8]) -> JobReply {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn request_roundtrip_with_voice_params() {
        let body = br#"{"text":"Hello world","utterance_id":"ab12:1","voice_params":{"speed":1.0}}"#;
        let job = parse_request(body).unwrap();
        assert_eq!(job.text, "Hello world");
        assert_eq!(job.utterance_id, "ab12:1");
        // voice_params 缺省可解析。
        let minimal = br#"{"text":"Hi","utterance_id":"ab12:2"}"#;
        assert!(parse_request(minimal).is_ok());
    }

    #[test]
    fn request_validation_rejects_bad_bodies() {
        let cases: &[(&[u8], &str)] = &[
            (b"not json", "invalid job body"),
            (br#"{"text":"","utterance_id":"x"}"#, "text must not be empty"),
            (br#"{"text":"  ","utterance_id":"x"}"#, "text must not be empty"),
            (br#"{"text":"hi","utterance_id":""}"#, "utterance_id must not be empty"),
            (br#"{"text":"hi"}"#, "invalid job body"),
            (br#"{"text":"hi","utterance_id":"x","voice_params":42}"#, "voice_params must be an object"),
            (br#"{"text":"hi","utterance_id":"x","bogus":1}"#, "invalid job body"),
        ];
        for (body, reason) in cases {
            let err = parse_request(body).unwrap_err();
            assert!(err.contains(reason), "body {body:?}：{err}");
        }
    }

    #[test]
    fn reply_serialization_omits_absent_reason() {
        let reply = JobReply {
            status: JobStatus::Submitted,
            utterance_id: "ab12:1".to_string(),
            reason: None,
        };
        let value: Value = serde_json::from_slice(&reply.to_bytes()).unwrap();
        assert_eq!(value["status"], "submitted");
        assert_eq!(value["utterance_id"], "ab12:1");
        assert!(value.get("reason").is_none());
        // 三态序列化稳定。
        for (status, text) in [
            (JobStatus::Submitted, "submitted"),
            (JobStatus::Skipped, "skipped"),
            (JobStatus::Failed, "failed"),
        ] {
            let reply = JobReply { status, utterance_id: String::new(), reason: None };
            let back: JobReply = serde_json::from_slice(&reply.to_bytes()).unwrap();
            assert_eq!(back, reply);
            assert_eq!(serde_json::to_value(&back).unwrap()["status"], text);
        }
    }

    #[test]
    fn dispatch_submits_and_echoes_utterance() {
        let (tx, rx) = mpsc::sync_channel(2);
        let shared = Shared::new();
        let body = br#"{"text":"Hello","utterance_id":"ab12:1"}"#;
        let reply = reply_of(&dispatch(&submit(body), &tx, &shared));
        assert_eq!(reply.status, JobStatus::Submitted);
        assert_eq!(reply.utterance_id, "ab12:1");
        assert_eq!(reply.reason, None);
        assert_eq!(shared.status().queue_depth, 1);
        let job = rx.try_recv().unwrap();
        assert_eq!(job.text, "Hello");
    }

    #[test]
    fn dispatch_queue_full_replies_skipped_and_counts() {
        let (tx, rx) = mpsc::sync_channel(1);
        let shared = Shared::new();
        let body = br#"{"text":"first","utterance_id":"ab12:1"}"#;
        assert_eq!(reply_of(&dispatch(&submit(body), &tx, &shared)).status, JobStatus::Submitted);
        let body = br#"{"text":"second","utterance_id":"ab12:2"}"#;
        let reply = reply_of(&dispatch(&submit(body), &tx, &shared));
        assert_eq!(reply.status, JobStatus::Skipped);
        assert_eq!(reply.utterance_id, "ab12:2");
        assert_eq!(reply.reason.as_deref(), Some("queue full"));
        let status = shared.status();
        assert_eq!(status.skipped, 1);
        assert_eq!(status.queue_depth, 1);
        // 出队后可再入队。
        rx.try_recv().unwrap();
        shared.queue_pop();
        assert_eq!(reply_of(&dispatch(&submit(body), &tx, &shared)).status, JobStatus::Submitted);
    }

    #[test]
    fn dispatch_invalid_body_replies_failed_without_queueing() {
        let (tx, rx) = mpsc::sync_channel(1);
        let shared = Shared::new();
        let reply = reply_of(&dispatch(&submit(b"{}"), &tx, &shared));
        assert_eq!(reply.status, JobStatus::Failed);
        assert!(reply.reason.is_some());
        assert_eq!(shared.status().queue_depth, 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_unknown_command_replies_failed() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let shared = Shared::new();
        let msg = Message {
            header: Header::new(ctl::TTS_HELLO, 1, 0),
            body: Vec::new(),
        };
        let reply = reply_of(&dispatch(&msg, &tx, &shared));
        assert_eq!(reply.status, JobStatus::Failed);
        assert_eq!(reply.reason.as_deref(), Some("unknown command"));
    }

    #[test]
    fn dispatch_worker_gone_replies_failed() {
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        drop(rx);
        let shared = Shared::new();
        let body = br#"{"text":"Hello","utterance_id":"ab12:1"}"#;
        let reply = reply_of(&dispatch(&submit(body), &tx, &shared));
        assert_eq!(reply.status, JobStatus::Failed);
        assert_eq!(reply.reason.as_deref(), Some("synth worker stopped"));
        assert_eq!(shared.status().queue_depth, 0);
    }

    #[test]
    fn dispatch_stop_bumps_epoch_and_replies_stopped() {
        let (tx, _rx) = mpsc::sync_channel::<Job>(1);
        let shared = Shared::new();
        assert_eq!(shared.job_epoch(), 0);
        for body in [&b""[..], &b"{}"[..]] {
            let reply = dispatch(&stop(body), &tx, &shared);
            let value: Value = serde_json::from_slice(&reply).unwrap();
            assert_eq!(value["stopped"], true);
        }
        assert_eq!(shared.job_epoch(), 2, "每次停止推进一代");
    }

    #[test]
    fn dispatch_stop_rejects_bad_body() {
        let (tx, _rx) = mpsc::sync_channel::<Job>(1);
        let shared = Shared::new();
        let reply = reply_of(&dispatch(&stop(b"{\"x\":1}"), &tx, &shared));
        assert_eq!(reply.status, JobStatus::Failed);
        assert_eq!(reply.reason.as_deref(), Some("invalid stop body"));
        assert_eq!(shared.job_epoch(), 0, "坏体不得推进代际");
    }

    #[test]
    fn stop_served_on_second_connection_while_first_idle() {
        // 桥投递长连接空闲不断开；停止短连接必须被并发服务，否则停止永远不生效。
        let dir = std::env::temp_dir()
            .join(format!("mozart-tts-jobs-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.sock");
        let _ = std::fs::remove_file(&path);
        let listener = Seqpacket::bind(&path).unwrap();
        listener.listen(4).unwrap();
        let (tx, _rx) = mpsc::sync_channel::<Job>(4);
        let shared = Arc::new(Shared::new());
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let server = {
            let shared = shared.clone();
            std::thread::spawn(move || serve(listener, tx, shared, stop))
        };

        let _idle = Seqpacket::connect(&path).unwrap();
        let stop_conn = Seqpacket::connect(&path).unwrap();
        stop_conn.send(ctl::TTS_JOB_STOP, 1, b"").unwrap();
        let reply = stop_conn.recv(Some(Duration::from_secs(2))).unwrap();
        let reply_type = reply.header.type_;
        assert_eq!(reply_type, ctl::TTS_JOB_REPLY);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body["stopped"], true);
        assert_eq!(shared.job_epoch(), 1);

        stop.store(true, Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}