//! 控制面服务端（SPEC-05）：tts.sock SEQPACKET + core 信封，Hello/GetStatus。
//! 只读原子计数，不碰合成热路径。连接逐个服务，断开回到 accept。

use crate::state::{Shared, VERSION};
use mozart_core::ctl::schema::{error_body, ok_body};
use mozart_core::ctl::{self, Message, Seqpacket};
use serde_json::json;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// 控制连接空闲断开，对齐 daemon 侧同一纪律（SPEC-04）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// recv 轮询步长：让停止标志在秒级内生效，再按累计空闲判断断开。
pub(crate) const RECV_POLL: Duration = Duration::from_secs(1);
/// accept 轮询间隔：无连接时周期检查停止标志（任务面复用）。
pub(crate) const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// 服务循环：绑定与监听由调用方完成；停止标志置位后返回。
pub fn serve(listener: Seqpacket, shared: Arc<Shared>, stop: &'static AtomicBool) {
    while !stop.load(Relaxed) {
        if let Err(err) = listener.wait_readable(Some(ACCEPT_POLL)) {
            if err.kind() == io::ErrorKind::TimedOut {
                continue;
            }
            eprintln!("mozart-tts: control poll: {err}");
            continue;
        }
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("mozart-tts: control accept: {err}");
                continue;
            }
        };
        serve_connection(&conn, &shared, stop);
    }
}

fn serve_connection(conn: &Seqpacket, shared: &Shared, stop: &AtomicBool) {
    let mut idle = Duration::ZERO;
    while !stop.load(Relaxed) {
        let msg = match conn.recv(Some(RECV_POLL)) {
            Ok(msg) => msg,
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                idle += RECV_POLL;
                if idle >= IDLE_TIMEOUT {
                    return;
                }
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(err) => {
                eprintln!("mozart-tts: control recv: {err}");
                return;
            }
        };
        idle = Duration::ZERO;
        let type_ = msg.header.type_;
        let request_id = msg.header.request_id;
        let reply = dispatch(&msg, shared);
        if conn.send(type_, request_id, &reply).is_err() {
            return;
        }
    }
}

/// 命令分发。Hello/GetStatus 无请求体参数；未知命令拒绝。
pub fn dispatch(msg: &Message, shared: &Shared) -> Vec<u8> {
    match msg.header.type_ {
        ctl::TTS_HELLO => ok_body(json!({"version": VERSION})),
        ctl::TTS_GET_STATUS => match serde_json::to_value(shared.status()) {
            Ok(value) => ok_body(value),
            Err(err) => error_body(ctl::CODE_INTERNAL, &err.to_string()),
        },
        _ => error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mozart_core::ctl::Header;
    use serde_json::Value;

    fn message(type_: u16) -> Message {
        Message {
            header: Header::new(type_, 7, 0),
            body: Vec::new(),
        }
    }

    fn data(reply: &[u8]) -> Value {
        let envelope: Value = serde_json::from_slice(reply).unwrap();
        assert_eq!(envelope["status"], "ok");
        envelope["data"].clone()
    }

    #[test]
    fn hello_reports_version() {
        let shared = Shared::new();
        let reply = dispatch(&message(ctl::TTS_HELLO), &shared);
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn get_status_covers_spec_fields() {
        let shared = Shared::new();
        shared.add_synthesized(3);
        shared.add_failed(1);
        shared.add_skipped(2);
        shared.set_model_loaded(true);
        shared.set_synthesizing(true);
        let status = data(&dispatch(&message(ctl::TTS_GET_STATUS), &shared));
        assert_eq!(status["state"], "synthesizing");
        assert_eq!(status["synthesized"], 3);
        assert_eq!(status["failed"], 1);
        assert_eq!(status["skipped"], 2);
        assert_eq!(status["model_loaded"], true);
        assert_eq!(status["queue_depth"], 0);
        assert_eq!(status["frames_out"], 0);
        assert_eq!(status["output_dropped"], 0);
        assert_eq!(status["last_error"], Value::Null);
        assert_eq!(status["version"], VERSION);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let shared = Shared::new();
        let reply = dispatch(&message(ctl::ASR_HELLO), &shared);
        let envelope: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(envelope["status"], "error");
        assert_eq!(envelope["code"], ctl::CODE_UNKNOWN_COMMAND);
    }
}