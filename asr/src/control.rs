//! 控制面服务端（SPEC-05）：asr.sock SEQPACKET + core 信封，Hello/GetStatus。
//! 只读原子计数与环形队列读数，不碰解码热路径。连接逐个服务，断开回到 accept。

use crate::state::{Shared, VERSION};
use mozart_core::ctl::schema::{error_body, ok_body};
use mozart_core::ctl::{self, Message, Seqpacket, ACCEPT_POLL, IDLE_TIMEOUT, RECV_POLL};
use mozart_core::ring::Ring;
use crate::receiver::Inbox;
use serde_json::json;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// 服务循环：绑定与监听由调用方完成；停止标志置位后返回。
pub fn serve(
    listener: Seqpacket,
    shared: Arc<Shared>,
    ring: Arc<Ring<Inbox>>,
    stop: &'static AtomicBool,
) {
    while !stop.load(Relaxed) {
        if let Err(err) = listener.wait_readable(Some(ACCEPT_POLL)) {
            if err.kind() == io::ErrorKind::TimedOut {
                continue;
            }
            eprintln!("mozart-asr: control poll: {err}");
            continue;
        }
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("mozart-asr: control accept: {err}");
                continue;
            }
        };
        serve_connection(&conn, &shared, &ring, stop);
    }
}

fn serve_connection(
    conn: &Seqpacket,
    shared: &Shared,
    ring: &Ring<Inbox>,
    stop: &AtomicBool,
) {
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
                eprintln!("mozart-asr: control recv: {err}");
                return;
            }
        };
        idle = Duration::ZERO;
        let type_ = msg.header.type_;
        let request_id = msg.header.request_id;
        let reply = dispatch(&msg, shared, ring);
        if conn.send(type_, request_id, &reply).is_err() {
            return;
        }
    }
}

/// 命令分发。Hello/GetStatus 无请求体参数；未知命令拒绝。
pub fn dispatch(msg: &Message, shared: &Shared, ring: &Ring<Inbox>) -> Vec<u8> {
    match msg.header.type_ {
        ctl::ASR_HELLO => ok_body(json!({"version": VERSION, "startup_id": shared.startup_id})),
        ctl::ASR_GET_STATUS => match serde_json::to_value(shared.status(ring.dropped())) {
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

    fn context() -> (Arc<Shared>, Arc<Ring<Inbox>>) {
        (
            Arc::new(Shared::new("teststart".to_string())),
            Arc::new(Ring::new(4)),
        )
    }

    fn data(reply: &[u8]) -> Value {
        let envelope: Value = serde_json::from_slice(reply).unwrap();
        assert_eq!(envelope["status"], "ok");
        envelope["data"].clone()
    }

    #[test]
    fn hello_reports_version_and_startup_id() {
        let (shared, ring) = context();
        let reply = dispatch(&message(ctl::ASR_HELLO), &shared, &ring);
        let data = data(&reply);
        assert_eq!(data["version"], VERSION);
        assert_eq!(data["startup_id"], "teststart");
    }

    #[test]
    fn get_status_covers_spec_fields() {
        let (shared, ring) = context();
        shared.add_received(5);
        shared.add_final(2);
        let reply = dispatch(&message(ctl::ASR_GET_STATUS), &shared, &ring);
        let status = data(&reply);
        assert_eq!(status["state"], "idle");
        assert_eq!(status["startup_id"], "teststart");
        assert_eq!(status["frames_received"], 5);
        assert_eq!(status["finals"], 2);
        assert_eq!(status["ring_dropped"], 0);
        assert_eq!(status["bridge_connected"], false);
        assert_eq!(status["version"], VERSION);
    }

    #[test]
    fn get_status_reads_ring_dropped() {
        let (shared, ring) = context();
        for _ in 0..6 {
            ring.push(Inbox::EndOfInput);
        }
        let reply = dispatch(&message(ctl::ASR_GET_STATUS), &shared, &ring);
        assert_eq!(data(&reply)["ring_dropped"], 2);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let (shared, ring) = context();
        let reply = dispatch(&message(ctl::PRE_HELLO), &shared, &ring);
        let envelope: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(envelope["status"], "error");
        assert_eq!(envelope["code"], ctl::CODE_UNKNOWN_COMMAND);
    }
}