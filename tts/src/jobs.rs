//! 任务面（SPEC-05 播报投递）：tts-jobs.sock SEQPACKET，桥连接、TTS 绑定。
//! 标准信封（同控制面）：TTS_JOB_SUBMIT 收译文任务，响应 type 回显请求号、体固定
//! status/code/reason/data，受理结果 submitted/skipped/failed 映射 ok/Ok、error/Busy、
//! error，utterance_id 放 data；TTS_JOB_STOP 推进任务代际（清队列并中止在途合成，见
//! synth）。有界任务队列经 try_send 入队，队满标 skipped 并计数，不阻塞桥。
//! 桥持一条长连接投递，speech stop 另开短连接下发停止，故逐连接并发服务。
//! v1 单目标音色（配置参考 WAV），voice_params 仅校验不生效。

use crate::control::{ACCEPT_POLL, RECV_POLL};
use crate::state::Shared;
use mozart_core::ctl::schema::{error_body, ok_body, Response};
use mozart_core::ctl::{self, Message, Seqpacket};
use serde::Deserialize;
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
    pub issued_ns: u64,
    pub text: String,
    pub utterance_id: String,
}

/// TTS_JOB_SUBMIT 请求体。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub issued_ns: Option<u64>,
    pub text: String,
    pub utterance_id: String,
    #[serde(default)]
    pub voice_params: Option<Value>,
}

/// 受理拒绝应答（标准 error 信封）：utterance_id 放 data 供桥对账。
/// 受理三态映射：submitted→ok/Ok（ok_body）、队满 skipped→error/Busy、
/// 请求无效或无法入队 failed→error/InvalidArgument 或 Internal。
fn submit_error(code: &str, reason: &str, utterance_id: &str) -> Vec<u8> {
    serde_json::to_vec(&Response {
        status: "error".to_string(),
        code: code.to_string(),
        reason: reason.to_string(),
        data: json!({ "utterance_id": utterance_id }),
    })
    .expect("job reply is JSON-serializable")
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
    let issued_ns = match request.issued_ns {
        Some(value) if mozart_core::speech::valid_timestamp(value) => value,
        Some(_) => return Err("issued_ns must be a valid monotonic timestamp".into()),
        None => mozart_core::speech::monotonic_ns(),
    };
    Ok(Job {
        issued_ns,
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
        let type_ = msg.header.type_;
        let reply = dispatch(&msg, tasks, shared);
        // 标准信封：响应 type 回显请求号。
        if conn.send(type_, request_id, &reply).is_err() {
            return;
        }
    }
}

/// 任务面与控制面共用停止入口。旧任务逐件丢弃，不清除新代队列。
pub fn dispatch(msg: &Message, tasks: &SyncSender<Job>, shared: &Shared) -> Vec<u8> {
    match msg.header.type_ {
        ctl::TTS_JOB_SUBMIT => dispatch_submit(msg, tasks, shared),
        ctl::TTS_JOB_STOP => stop_reply(&msg.body, shared),
        _ => error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
    }
}

pub(crate) fn stop_reply(body: &[u8], shared: &Shared) -> Vec<u8> {
    let cutoff = if body.is_empty() || body == b"{}" {
        mozart_core::speech::monotonic_ns()
    } else {
        match serde_json::from_slice::<mozart_core::speech::StopFence>(body) {
            Ok(fence) if mozart_core::speech::valid_timestamp(fence.cutoff_ns) => fence.cutoff_ns,
            _ => return error_body(ctl::CODE_INVALID_ARGUMENT, "invalid stop body"),
        }
    };
    let cutoff = shared.install_fence(cutoff);
    ok_body(json!({ "stopped": true, "cutoff_ns": cutoff }))
}

/// TTS_JOB_SUBMIT：入队结果决定受理应答（标准信封，utterance_id 放 data）。
fn dispatch_submit(msg: &Message, tasks: &SyncSender<Job>, shared: &Shared) -> Vec<u8> {
    let job = match parse_request(&msg.body) {
        Ok(job) => job,
        Err(reason) => return submit_error(ctl::CODE_INVALID_ARGUMENT, &reason, ""),
    };
    let utterance_id = job.utterance_id.clone();
    if job.issued_ns < shared.cutoff() {
        shared.add_stopped(1);
        return submit_error(ctl::CODE_NOT_READY, "speech generation revoked", &utterance_id);
    }
    shared.queue_push();
    match tasks.try_send(job) {
        Ok(()) => ok_body(json!({ "utterance_id": utterance_id })),
        Err(TrySendError::Full(_)) => {
            shared.queue_pop();
            shared.add_skipped(1);
            submit_error(ctl::CODE_BUSY, "queue full", &utterance_id)
        }
        Err(TrySendError::Disconnected(_)) => {
            shared.queue_pop();
            submit_error(ctl::CODE_INTERNAL, "synth worker stopped", &utterance_id)
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

    fn reply_of(bytes: &[u8]) -> Response {
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
    fn submit_reply_is_standard_envelope() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let shared = Shared::new();
        let body = br#"{"text":"Hi","utterance_id":"ab12:1"}"#;
        let reply = reply_of(&dispatch(&submit(body), &tx, &shared));
        // 体固定 status/code/reason/data，utterance_id 放 data。
        assert_eq!(reply.status, "ok");
        assert_eq!(reply.code, ctl::CODE_OK);
        assert_eq!(reply.reason, "");
        assert_eq!(reply.data["utterance_id"], "ab12:1");
    }

    #[test]
    fn dispatch_submits_and_echoes_utterance() {
        let (tx, rx) = mpsc::sync_channel(2);
        let shared = Shared::new();
        let body = br#"{"text":"Hello","utterance_id":"ab12:1"}"#;
        let reply = reply_of(&dispatch(&submit(body), &tx, &shared));
        assert_eq!(reply.status, "ok");
        assert_eq!(reply.code, ctl::CODE_OK);
        assert_eq!(reply.data["utterance_id"], "ab12:1");
        assert_eq!(reply.reason, "");
        assert_eq!(shared.status().queue_depth, 1);
        let job = rx.try_recv().unwrap();
        assert_eq!(job.text, "Hello");
    }

    #[test]
    fn dispatch_queue_full_replies_skipped_and_counts() {
        let (tx, rx) = mpsc::sync_channel(1);
        let shared = Shared::new();
        let body = br#"{"text":"first","utterance_id":"ab12:1"}"#;
        assert_eq!(reply_of(&dispatch(&submit(body), &tx, &shared)).status, "ok");
        let body = br#"{"text":"second","utterance_id":"ab12:2"}"#;
        let reply = reply_of(&dispatch(&submit(body), &tx, &shared));
        assert_eq!(reply.status, "error");
        assert_eq!(reply.code, ctl::CODE_BUSY);
        assert_eq!(reply.data["utterance_id"], "ab12:2");
        assert_eq!(reply.reason, "queue full");
        let status = shared.status();
        assert_eq!(status.skipped, 1);
        assert_eq!(status.queue_depth, 1);
        // 出队后可再入队。
        rx.try_recv().unwrap();
        shared.queue_pop();
        assert_eq!(reply_of(&dispatch(&submit(body), &tx, &shared)).status, "ok");
    }

    #[test]
    fn dispatch_invalid_body_replies_failed_without_queueing() {
        let (tx, rx) = mpsc::sync_channel(1);
        let shared = Shared::new();
        let reply = reply_of(&dispatch(&submit(b"{}"), &tx, &shared));
        assert_eq!(reply.status, "error");
        assert_eq!(reply.code, ctl::CODE_INVALID_ARGUMENT);
        assert!(!reply.reason.is_empty());
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
        assert_eq!(reply.status, "error");
        assert_eq!(reply.code, ctl::CODE_UNKNOWN_COMMAND);
        assert_eq!(reply.reason, "unknown command");
    }

    #[test]
    fn dispatch_worker_gone_replies_failed() {
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        drop(rx);
        let shared = Shared::new();
        let body = br#"{"text":"Hello","utterance_id":"ab12:1"}"#;
        let reply = reply_of(&dispatch(&submit(body), &tx, &shared));
        assert_eq!(reply.status, "error");
        assert_eq!(reply.code, ctl::CODE_INTERNAL);
        assert_eq!(reply.reason, "synth worker stopped");
        assert_eq!(reply.data["utterance_id"], "ab12:1");
        assert_eq!(shared.status().queue_depth, 0);
    }

    #[test]
    fn dispatch_stop_installs_cutoff_and_replies_stopped() {
        let (tx, _rx) = mpsc::sync_channel::<Job>(1);
        let shared = Shared::new();
        assert_eq!(shared.cutoff(), 0);
        for body in [&b""[..], &b"{}"[..]] {
            let before = mozart_core::speech::monotonic_ns();
            let reply = reply_of(&dispatch(&stop(body), &tx, &shared));
            assert_eq!(reply.status, "ok");
            assert_eq!(reply.data["stopped"], true);
            assert_eq!(reply.data["cutoff_ns"], shared.cutoff());
            assert!(shared.cutoff() >= before);
        }
    }

    #[test]
    fn typed_stop_rejects_late_old_job_and_does_not_revoke_new_job() {
        let (tx, rx) = mpsc::sync_channel::<Job>(2);
        let shared = Shared::new();
        let cutoff = mozart_core::speech::monotonic_ns();
        let stop_body = json!({"cutoff_ns": cutoff});
        assert_eq!(reply_of(&dispatch(&stop(stop_body.to_string().as_bytes()), &tx, &shared)).status, "ok");
        let old_body = json!({"text":"old", "utterance_id":"s:1", "issued_ns":cutoff - 1});
        let old = reply_of(&dispatch(&submit(old_body.to_string().as_bytes()), &tx, &shared));
        assert_eq!(old.code, ctl::CODE_NOT_READY);
        assert_eq!(shared.status().queue_depth, 0);
        assert!(rx.try_recv().is_err());

        let issued_ns = mozart_core::speech::monotonic_ns();
        let new_body = json!({"text":"new", "utterance_id":"s:2", "issued_ns":issued_ns});
        let new = reply_of(&dispatch(&submit(new_body.to_string().as_bytes()), &tx, &shared));
        assert_eq!(new.status, "ok");
        let replay = reply_of(&dispatch(&stop(stop_body.to_string().as_bytes()), &tx, &shared));
        assert_eq!(replay.data["cutoff_ns"], cutoff);
        let job = rx.try_recv().unwrap();
        assert_eq!(job.utterance_id, "s:2");
        assert!(!shared.abort_job(&AtomicBool::new(false), job.issued_ns).aborted());
    }

    #[test]
    fn dispatch_stop_rejects_bad_body() {
        let (tx, _rx) = mpsc::sync_channel::<Job>(1);
        let shared = Shared::new();
        let reply = reply_of(&dispatch(&stop(b"{\"x\":1}"), &tx, &shared));
        assert_eq!(reply.status, "error");
        assert_eq!(reply.code, ctl::CODE_INVALID_ARGUMENT);
        assert_eq!(reply.reason, "invalid stop body");
        assert_eq!(shared.cutoff(), 0, "坏体不得推进代际");
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
        assert_eq!(reply_type, ctl::TTS_JOB_STOP);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["data"]["stopped"], true);
        assert_ne!(shared.cutoff(), 0);
        assert_eq!(body["data"]["cutoff_ns"], shared.cutoff());

        stop.store(true, Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}