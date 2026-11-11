//! 控制面服务端（SPEC-02）：Unix SEQPACKET + core 信封，复用 schema 的应答封装。
//! Hello/SetRoute/SetMute/GetStatus。命令解析与执行只读写原子，不阻塞音频线程；
//! GetStatus 只读原子快照与队列读数（AGENTS「GetStatus 只读原子快照」精神）。
//! 连接逐个服务：daemon 持单连接每秒轮询，断开后回到 accept。

use crate::state::{QueueReadout, Shared, VERSION};
use mozart_core::ctl::schema::{error_body, ok_body, MuteSetRequest, RouteSetRequest};
use mozart_core::ctl::{self, Message, Seqpacket};
use mozart_core::ring::Ring;
use mozart_core::RawFrame;
use serde_json::{json, Value};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 控制连接空闲断开，对齐 daemon 侧同一纪律（SPEC-04）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// recv 轮询步长：让停止标志在秒级内生效，再按累计空闲判断断开。
const RECV_POLL: Duration = Duration::from_secs(1);
/// accept 轮询间隔：无连接时周期检查停止标志，保证 SIGTERM 可退出。
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// 服务循环：绑定与监听由调用方完成；收到停止标志后返回。
pub fn serve(
    listener: Seqpacket,
    shared: Arc<Shared>,
    capture_ring: Arc<Ring<RawFrame>>,
    trunk: Arc<Ring<RawFrame>>,
    stop: &'static AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        if let Err(err) = listener.wait_readable(Some(ACCEPT_POLL)) {
            if err.kind() == io::ErrorKind::TimedOut {
                continue;
            }
            eprintln!("mozart-pre: control poll: {err}");
            continue;
        }
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("mozart-pre: control accept: {err}");
                continue;
            }
        };
        serve_connection(&conn, &shared, &capture_ring, &trunk, stop);
    }
}

fn serve_connection(
    conn: &Seqpacket,
    shared: &Shared,
    capture_ring: &Ring<RawFrame>,
    trunk: &Ring<RawFrame>,
    stop: &AtomicBool,
) {
    let mut idle = Duration::ZERO;
    while !stop.load(Ordering::Relaxed) {
        let msg = match conn.recv(Some(RECV_POLL)) {
            Ok(msg) => msg,
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                // 空闲累计到上限即断开；期间仍响应停止标志。
                idle += RECV_POLL;
                if idle >= IDLE_TIMEOUT {
                    return;
                }
                continue;
            }
            // 对端关闭是正常轮转；其余错误也结束本连接。
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(err) => {
                eprintln!("mozart-pre: control recv: {err}");
                return;
            }
        };
        idle = Duration::ZERO;
        let type_ = msg.header.type_;
        let request_id = msg.header.request_id;
        let reply = dispatch(&msg, shared, capture_ring, trunk);
        if conn.send(type_, request_id, &reply).is_err() {
            return;
        }
    }
}

/// 解析请求体；空体按空 JSON 处理（与桩一致）。解析失败直接给出应答。
fn parse_body(body: &[u8]) -> Result<Value, Vec<u8>> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body)
        .map_err(|err| error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()))
}

/// 命令分发。请求体是外部输入，在此校验一次；内部执行信任已校验的值。
pub fn dispatch(
    msg: &Message,
    shared: &Shared,
    capture_ring: &Ring<RawFrame>,
    trunk: &Ring<RawFrame>,
) -> Vec<u8> {
    let body = match parse_body(&msg.body) {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    match msg.header.type_ {
        ctl::PRE_HELLO => ok_body(json!({"version": VERSION})),
        ctl::PRE_SET_ROUTE => match serde_json::from_value::<RouteSetRequest>(body) {
            Ok(request) => {
                shared.set_route_target(request.route);
                ok_body(json!({"route": request.route}))
            }
            Err(err) => error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
        },
        ctl::PRE_SET_MUTE => match serde_json::from_value::<MuteSetRequest>(body) {
            Ok(request) => {
                shared.set_mute_target(request.mute);
                ok_body(json!({"mute": request.mute}))
            }
            Err(err) => error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
        },
        ctl::PRE_GET_STATUS => {
            let readout = QueueReadout {
                depth: capture_ring.depth() as u64,
                dropped: capture_ring.dropped() + trunk.dropped(),
            };
            match serde_json::to_value(shared.status(readout)) {
                Ok(value) => ok_body(value),
                Err(err) => error_body(ctl::CODE_INTERNAL, &err.to_string()),
            }
        }
        _ => error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mozart_core::ctl::Header;
    use std::path::PathBuf;

    fn message(type_: u16, body: &[u8]) -> Message {
        Message {
            header: Header::new(type_, 7, body.len() as u32),
            body: body.to_vec(),
        }
    }

    fn context() -> (Arc<Shared>, Arc<Ring<RawFrame>>, Arc<Ring<RawFrame>>) {
        (
            Arc::new(Shared::new("cap-dev".to_string(), "play-dev".to_string())),
            Arc::new(Ring::new(2)),
            Arc::new(Ring::new(2)),
        )
    }

    fn data(reply: &[u8]) -> Value {
        let envelope: Value = serde_json::from_slice(reply).unwrap();
        assert_eq!(envelope["status"], "ok", "应答必须是 ok 信封");
        envelope["data"].clone()
    }

    fn error_code(reply: &[u8]) -> String {
        let envelope: Value = serde_json::from_slice(reply).unwrap();
        assert_eq!(envelope["status"], "error");
        envelope["code"].as_str().unwrap().to_string()
    }

    #[test]
    fn hello_reports_version() {
        let (shared, cap, trunk) = context();
        let reply = dispatch(&message(ctl::PRE_HELLO, b"{}"), &shared, &cap, &trunk);
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn hello_accepts_empty_body() {
        let (shared, cap, trunk) = context();
        let reply = dispatch(&message(ctl::PRE_HELLO, b""), &shared, &cap, &trunk);
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn set_route_and_status_roundtrip() {
        let (shared, cap, trunk) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":"rvc"}"#),
            &shared,
            &cap,
            &trunk,
        );
        assert_eq!(data(&reply)["route"], "rvc");

        let reply = dispatch(&message(ctl::PRE_GET_STATUS, b"{}"), &shared, &cap, &trunk);
        let status = data(&reply);
        assert_eq!(status["route"], "rvc");
        assert_eq!(status["state"], "running");
        assert_eq!(status["capture_device"], "cap-dev");
        assert_eq!(status["playback_device"], "play-dev");
        assert_eq!(status["frames_captured"], 0);
        assert_eq!(status["frames_dropped"], 0);
        assert_eq!(status["queue_depth"], 0);
        assert_eq!(status["consumer_drops"], json!({}));
        assert_eq!(status["last_error"], Value::Null);
        assert_eq!(status["version"], VERSION);
    }

    #[test]
    fn set_route_rejects_unknown_value() {
        let (shared, cap, trunk) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":"bogus"}"#),
            &shared,
            &cap,
            &trunk,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);

        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":42}"#),
            &shared,
            &cap,
            &trunk,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);

        let reply = dispatch(&message(ctl::PRE_SET_ROUTE, b"not json"), &shared, &cap, &trunk);
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);

        // 未知键拒绝（deny_unknown_fields）。
        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":"direct","x":1}"#),
            &shared,
            &cap,
            &trunk,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
    }

    #[test]
    fn set_mute_roundtrip_and_rejects_bad_body() {
        let (shared, cap, trunk) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":true}"#),
            &shared,
            &cap,
            &trunk,
        );
        assert_eq!(data(&reply)["mute"], true);
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":false}"#),
            &shared,
            &cap,
            &trunk,
        );
        assert_eq!(data(&reply)["mute"], false);
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":"yes"}"#),
            &shared,
            &cap,
            &trunk,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let (shared, cap, trunk) = context();
        let reply = dispatch(
            &message(ctl::DAEMON_HELLO, b"{}"),
            &shared,
            &cap,
            &trunk,
        );
        assert_eq!(error_code(&reply), ctl::CODE_UNKNOWN_COMMAND);
    }

    #[test]
    fn serve_loop_answers_over_wire_and_stops() {
        let dir = std::env::temp_dir().join(format!("mozart-pre-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("pre.sock");
        let _ = std::fs::remove_file(&path);

        let (shared, cap, trunk) = context();
        let listener = Seqpacket::bind(&path).unwrap();
        listener.listen(4).unwrap();
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let server = std::thread::spawn({
            let shared = shared.clone();
            let cap = cap.clone();
            let trunk = trunk.clone();
            move || serve(listener, shared, cap, trunk, stop)
        });

        let conn = Seqpacket::connect(&path).unwrap();
        conn.send(ctl::PRE_HELLO, 1, b"{}").unwrap();
        let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
        // packed 头字段先按值复制再比较。
        let type_ = reply.header.type_;
        let request_id = reply.header.request_id;
        assert_eq!(type_, ctl::PRE_HELLO);
        assert_eq!(request_id, 1);
        assert_eq!(data(&reply.body)["version"], VERSION);

        conn.send(ctl::PRE_SET_ROUTE, 2, br#"{"route":"rvc"}"#)
            .unwrap();
        let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
        let request_id = reply.header.request_id;
        assert_eq!(request_id, 2);
        assert_eq!(data(&reply.body)["route"], "rvc");

        conn.send(ctl::PRE_GET_STATUS, 3, b"{}").unwrap();
        let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(data(&reply.body)["route"], "rvc");

        // 停止标志结束服务循环。
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }
}
