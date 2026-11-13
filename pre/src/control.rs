//! 控制面服务端（SPEC-02）：Unix SEQPACKET + core 信封，复用 schema 的应答封装。
//! Hello/SetRoute/SetMute/GetStatus。命令解析与执行只读写原子，不阻塞音频线程；
//! GetStatus 只读原子快照与队列读数（AGENTS「GetStatus 只读原子快照」精神）。
//! 连接逐个服务：daemon 持单连接每秒轮询，断开后回到 accept。

use crate::state::{QueueReadout, Shared, VERSION};
use mozart_core::ctl::schema::{error_body, ok_body, FileModeSetRequest, MuteSetRequest, RouteSetRequest};
use mozart_core::ctl::{self, Message, Seqpacket, ACCEPT_POLL, IDLE_TIMEOUT, RECV_POLL};
use mozart_core::ring::Ring;
use mozart_core::{OutputFrame, RawFrame};
use serde_json::{json, Value};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 文件接管握手等待上限：接收线程按 RECV_POLL（100 ms）节拍释放绑定，
/// 正常百毫秒内完成；取 1 s 兜底，小于 daemon 缺省 rpc_timeout_ms（2 s）。
const FILE_HANDOFF_TIMEOUT: Duration = Duration::from_secs(1);
/// 握手等待的轮询步长。
const FILE_HANDOFF_POLL: Duration = Duration::from_millis(5);

/// 服务循环：绑定与监听由调用方完成；收到停止标志后返回。
pub fn serve(
    listener: Seqpacket,
    shared: Arc<Shared>,
    capture_ring: Arc<Ring<RawFrame>>,
    trunk: Arc<Ring<RawFrame>>,
    rvc_ring: Arc<Ring<OutputFrame>>,
    tts_ring: Arc<Ring<OutputFrame>>,
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
        serve_connection(&conn, &shared, &capture_ring, &trunk, &rvc_ring, &tts_ring, stop);
    }
}

fn serve_connection(
    conn: &Seqpacket,
    shared: &Shared,
    capture_ring: &Ring<RawFrame>,
    trunk: &Ring<RawFrame>,
    rvc_ring: &Ring<OutputFrame>,
    tts_ring: &Ring<OutputFrame>,
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
        let reply = dispatch(&msg, shared, capture_ring, trunk, rvc_ring, tts_ring);
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
    rvc_ring: &Ring<OutputFrame>,
    tts_ring: &Ring<OutputFrame>,
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
        ctl::PRE_SET_FILE_MODE => match serde_json::from_value::<FileModeSetRequest>(body) {
            Ok(request) => {
                if request.active {
                    // 文件接管：先置门控（处理线程停发 rvc），再等接收线程
                    // 释放输出 socket 的握手完成才应答——daemon 收到 ok 后
                    // 才绑定同一 socket，次序保证不撞绑。本等待在控制线程，
                    // 不阻塞音频线程。
                    shared.set_file_mode(true);
                    let deadline = Instant::now() + FILE_HANDOFF_TIMEOUT;
                    while !shared.output_released() {
                        if Instant::now() >= deadline {
                            // 接收线程未在时限内释放（异常态）：回滚门控，
                            // 不留「已暂停但无文件任务」的悬空态；原因经应答可查。
                            shared.set_file_mode(false);
                            return error_body(
                                ctl::CODE_NOT_READY,
                                "output socket handoff timed out",
                            );
                        }
                        std::thread::sleep(FILE_HANDOFF_POLL);
                    }
                    ok_body(json!({"active": true}))
                } else {
                    // 交还：daemon 已先解绑输出 socket；接收线程在下一节拍
                    // 重绑并恢复接收，处理线程恢复 rvc 发送。
                    shared.set_file_mode(false);
                    ok_body(json!({"active": false}))
                }
            }
            Err(err) => error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
        },
        ctl::PRE_SPEECH_STOP => {
            // speech stop 经 daemon 转发：清播报环，丢弃已送达未播的播报帧。
            // 播放线程下一帧见环空，holdoff 后恢复实时；计数经应答可查。
            let mut frames_cleared = 0u64;
            while tts_ring.pop(Some(Duration::ZERO)).is_some() {
                frames_cleared += 1;
            }
            ok_body(json!({"stopped": true, "frames_cleared": frames_cleared}))
        }
        ctl::PRE_GET_STATUS => {
            let readout = QueueReadout {
                depth: capture_ring.depth() as u64,
                dropped: capture_ring.dropped() + trunk.dropped(),
                rvc_dropped: rvc_ring.dropped(),
                tts_dropped: tts_ring.dropped(),
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

    fn context() -> (
        Arc<Shared>,
        Arc<Ring<RawFrame>>,
        Arc<Ring<RawFrame>>,
        Arc<Ring<OutputFrame>>,
        Arc<Ring<OutputFrame>>,
    ) {
        (
            Arc::new(Shared::new("cap-dev".to_string(), "play-dev".to_string())),
            Arc::new(Ring::new(2)),
            Arc::new(Ring::new(2)),
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
        let (shared, cap, trunk, rvc, tts) = context();
        let reply = dispatch(&message(ctl::PRE_HELLO, b"{}"), &shared, &cap, &trunk, &rvc, &tts);
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn hello_accepts_empty_body() {
        let (shared, cap, trunk, rvc, tts) = context();
        let reply = dispatch(&message(ctl::PRE_HELLO, b""), &shared, &cap, &trunk, &rvc, &tts);
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn set_route_and_status_roundtrip() {
        let (shared, cap, trunk, rvc, tts) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":"rvc"}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(data(&reply)["route"], "rvc");

        let reply = dispatch(&message(ctl::PRE_GET_STATUS, b"{}"), &shared, &cap, &trunk, &rvc, &tts);
        let status = data(&reply);
        assert_eq!(status["route"], "rvc");
        assert_eq!(status["state"], "running");
        assert_eq!(status["capture_device"], "cap-dev");
        assert_eq!(status["playback_device"], "play-dev");
        assert_eq!(status["frames_captured"], 0);
        assert_eq!(status["frames_dropped"], 0);
        assert_eq!(status["queue_depth"], 0);
        // consumer_drops 四键恒存在且为零值（SPEC-02 / SPEC-05 口径）。
        assert_eq!(
            status["consumer_drops"],
            json!({"rvc": 0, "rvc.skipped": 0, "asr": 0, "tts": 0})
        );
        assert_eq!(status["last_error"], Value::Null);
        assert_eq!(status["version"], VERSION);
    }

    #[test]
    fn set_route_rejects_unknown_value() {
        let (shared, cap, trunk, rvc, tts) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":"bogus"}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);

        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":42}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);

        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, b"not json"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);

        // 未知键拒绝（deny_unknown_fields）。
        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":"direct","x":1}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
    }

    #[test]
    fn set_mute_roundtrip_and_rejects_bad_body() {
        let (shared, cap, trunk, rvc, tts) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":true}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(data(&reply)["mute"], true);
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":false}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(data(&reply)["mute"], false);
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":"yes"}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let (shared, cap, trunk, rvc, tts) = context();
        let reply = dispatch(
            &message(ctl::DAEMON_HELLO, b"{}"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(error_code(&reply), ctl::CODE_UNKNOWN_COMMAND);
    }

    /// 文件接管握手：模拟接收线程按 file_mode 置/清 output_released，
    /// active=true 必须等握手完成才应答 ok；active=false 立即恢复。
    #[test]
    fn set_file_mode_waits_for_handshake_and_roundtrips() {
        let (shared, cap, trunk, rvc, tts) = context();
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // 接收线程的握手行为：接管即释放，交还即重绑。
                    shared.set_output_released(shared.file_mode());
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };

        let reply = dispatch(
            &message(ctl::PRE_SET_FILE_MODE, br#"{"active":true}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(data(&reply)["active"], true);
        assert!(shared.file_mode());
        assert!(shared.output_released(), "应答 ok 前握手必须已完成");

        let reply = dispatch(
            &message(ctl::PRE_SET_FILE_MODE, br#"{"active":false}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(data(&reply)["active"], false);
        assert!(!shared.file_mode());

        stop.store(true, Ordering::Relaxed);
        watcher.join().unwrap();
    }

    #[test]
    fn set_file_mode_rejects_bad_body() {
        let (shared, cap, trunk, rvc, tts) = context();
        for body in [
            &b"{}"[..],
            br#"{"active":"yes"}"#,
            br#"{"active":true,"x":1}"#,
            b"not json",
        ] {
            let reply = dispatch(
                &message(ctl::PRE_SET_FILE_MODE, body),
                &shared,
                &cap,
                &trunk,
                &rvc,
                &tts,
            );
            assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
            assert!(!shared.file_mode(), "坏体不得改变门控状态");
        }
    }

    /// 接收线程未释放（无握手）：active=true 在时限内回滚门控并报错，
    /// 不留「已暂停但无文件任务」的悬空态。
    #[test]
    fn set_file_mode_timeout_rolls_back() {
        let (shared, cap, trunk, rvc, tts) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_FILE_MODE, br#"{"active":true}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
        );
        assert_eq!(error_code(&reply), ctl::CODE_NOT_READY);
        assert!(!shared.file_mode(), "超时必须回滚门控");
    }

    #[test]
    fn serve_loop_answers_over_wire_and_stops() {
        let dir = std::env::temp_dir().join(format!("mozart-pre-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("pre.sock");
        let _ = std::fs::remove_file(&path);

        let (shared, cap, trunk, rvc, tts) = context();
        let listener = Seqpacket::bind(&path).unwrap();
        listener.listen(4).unwrap();
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let server = std::thread::spawn({
            let shared = shared.clone();
            let cap = cap.clone();
            let trunk = trunk.clone();
            let rvc = rvc.clone();
            let tts = tts.clone();
            move || serve(listener, shared, cap, trunk, rvc, tts, stop)
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

        // speech stop 清播报环：先塞两帧，停止后环空、应答计 frames_cleared。
        tts.push(OutputFrame {
            meta: mozart_core::FrameMeta {
                pts_ns: 0, frame_idx: 0, vad_flag: 1, energy_db: 0, conf: 0, segment_id: 0,
            },
            pcm: [0.0; mozart_core::OUTPUT_SAMPLES],
        });
        tts.push(OutputFrame {
            meta: mozart_core::FrameMeta {
                pts_ns: 0, frame_idx: 1, vad_flag: 1, energy_db: 0, conf: 0, segment_id: 0,
            },
            pcm: [0.0; mozart_core::OUTPUT_SAMPLES],
        });
        conn.send(ctl::PRE_SPEECH_STOP, 4, b"{}").unwrap();
        let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
        let stopped = data(&reply.body);
        assert_eq!(stopped["stopped"], true);
        assert_eq!(stopped["frames_cleared"], 2);
        assert_eq!(tts.depth(), 0, "停止后播报环必须清空");

        // 停止标志结束服务循环。
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }
}
