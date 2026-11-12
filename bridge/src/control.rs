//! 控制面服务端（SPEC-05）：bridge.sock SEQPACKET + core 信封。
//! Hello/GetStatus 加字幕开关命令；开关改桥内活状态并存档状态文件。
//! 连接逐个服务，断开回到 accept；只读原子计数与队列深度，不碰落盘路径。

use crate::state::{Queues, Shared, Switches, VERSION};
use mozart_core::ctl::schema::{error_body, ok_body};
use mozart_core::ctl::{self, Message, Seqpacket};
use serde::Deserialize;
use serde_json::json;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// 控制连接空闲断开，对齐 daemon 侧同一纪律（SPEC-04）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// recv 轮询步长：让停止标志在宽限期内生效，再按累计空闲判断断开。
const RECV_POLL: Duration = Duration::from_secs(1);
/// accept 轮询间隔：无连接时周期检查停止标志。
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// 开关请求体：{"enabled": bool}。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledRequest {
    enabled: bool,
}

/// speech stop 请求体：空对象（兼容空 body）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopRequest {}

/// 服务循环：绑定与监听由调用方完成；停止标志置位后返回。
pub fn serve(
    listener: Seqpacket,
    shared: Arc<Shared>,
    queues: Arc<Queues>,
    stop: &'static AtomicBool,
) {
    while !stop.load(Relaxed) {
        if let Err(err) = listener.wait_readable(Some(ACCEPT_POLL)) {
            if err.kind() == io::ErrorKind::TimedOut {
                continue;
            }
            eprintln!("mozart-bridge: control poll: {err}");
            continue;
        }
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("mozart-bridge: control accept: {err}");
                continue;
            }
        };
        serve_connection(&conn, &shared, &queues, stop);
    }
}

fn serve_connection(conn: &Seqpacket, shared: &Shared, queues: &Queues, stop: &AtomicBool) {
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
                eprintln!("mozart-bridge: control recv: {err}");
                return;
            }
        };
        idle = Duration::ZERO;
        let type_ = msg.header.type_;
        let request_id = msg.header.request_id;
        let reply = dispatch(&msg, shared, queues);
        if conn.send(type_, request_id, &reply).is_err() {
            return;
        }
    }
}

/// 命令分发。请求体在入口校验一次；未知命令拒绝。
pub fn dispatch(msg: &Message, shared: &Shared, queues: &Queues) -> Vec<u8> {
    match msg.header.type_ {
        ctl::BRIDGE_HELLO => ok_body(json!({ "version": VERSION })),
        ctl::BRIDGE_GET_STATUS => {
            let status = shared.status(queues.translate_depth(), queues.speech_depth());
            ok_body(serde_json::to_value(status).expect("bridge status is JSON-serializable"))
        }
        ctl::BRIDGE_TRANSLATE_SET => match parse_enabled(&msg.body) {
            Ok(enabled) => set_switches(
                shared,
                Switches {
                    translate_enabled: enabled,
                    speech_enabled: shared.speech_enabled(),
                },
            ),
            Err(reason) => error_body(ctl::CODE_INVALID_ARGUMENT, &reason),
        },
        ctl::BRIDGE_SPEECH_SET => match parse_enabled(&msg.body) {
            Ok(enabled) => set_switches(
                shared,
                Switches {
                    translate_enabled: shared.translate_enabled(),
                    speech_enabled: enabled,
                },
            ),
            Err(reason) => error_body(ctl::CODE_INVALID_ARGUMENT, &reason),
        },
        ctl::BRIDGE_SPEECH_STOP => {
            if let Err(reason) = parse_stop(&msg.body) {
                return error_body(ctl::CODE_INVALID_ARGUMENT, &reason);
            }
            // 关播报并清空待播报队列（在途丢弃）；先存档再清队列，失败整体被拒。
            let switches = Switches {
                translate_enabled: shared.translate_enabled(),
                speech_enabled: false,
            };
            if let Err(reason) = shared.set_switches(switches) {
                return error_body(ctl::CODE_INTERNAL, &reason);
            }
            let cleared = queues.clear_speech();
            ok_body(json!({ "speech_enabled": false, "queue_cleared": cleared }))
        }
        _ => error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
    }
}

fn set_switches(shared: &Shared, switches: Switches) -> Vec<u8> {
    match shared.set_switches(switches) {
        Ok(()) => ok_body(json!({
            "translate_enabled": switches.translate_enabled,
            "speech_enabled": switches.speech_enabled,
        })),
        Err(reason) => error_body(ctl::CODE_INTERNAL, &reason),
    }
}

fn parse_enabled(body: &[u8]) -> Result<bool, String> {
    let request: EnabledRequest =
        serde_json::from_slice(body).map_err(|err| format!("invalid switch body: {err}"))?;
    Ok(request.enabled)
}

fn parse_stop(body: &[u8]) -> Result<(), String> {
    if body.is_empty() {
        return Ok(());
    }
    serde_json::from_slice::<StopRequest>(body)
        .map(|_| ())
        .map_err(|err| format!("invalid stop body: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SpeechJob, STATE_FILE_NAME};
    use mozart_core::ctl::Header;
    use serde_json::Value;
    use std::path::PathBuf;

    struct Fixture {
        dir: PathBuf,
        shared: Shared,
        queues: Queues,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mozart-bridge-ctl-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let shared = Shared::new(
                Switches {
                    translate_enabled: true,
                    speech_enabled: true,
                },
                dir.join("subtitles.jsonl"),
                dir.join(STATE_FILE_NAME),
            );
            Self {
                dir,
                shared,
                queues: Queues::new(4, 4),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn message(type_: u16, body: &[u8]) -> Message {
        Message {
            header: Header::new(type_, 7, body.len() as u32),
            body: body.to_vec(),
        }
    }

    fn data(reply: &[u8]) -> Value {
        let envelope: Value = serde_json::from_slice(reply).unwrap();
        assert_eq!(envelope["status"], "ok", "应答：{envelope}");
        envelope["data"].clone()
    }

    fn error_code(reply: &[u8]) -> String {
        let envelope: Value = serde_json::from_slice(reply).unwrap();
        assert_eq!(envelope["status"], "error");
        envelope["code"].as_str().unwrap().to_string()
    }

    #[test]
    fn hello_reports_version() {
        let fixture = Fixture::new("hello");
        let reply = dispatch(&message(ctl::BRIDGE_HELLO, b""), &fixture.shared, &fixture.queues);
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn get_status_covers_four_groups() {
        let fixture = Fixture::new("status");
        fixture.shared.add_published();
        fixture.shared.set_records_in_memory(1);
        fixture.shared.set_last_write_unix_ms(123);
        fixture
            .queues
            .push_translate(crate::state::TranslateJob {
                utterance_id: "s:1".to_string(),
                text: "你好".to_string(),
            });
        let reply = dispatch(
            &message(ctl::BRIDGE_GET_STATUS, b""),
            &fixture.shared,
            &fixture.queues,
        );
        let status = data(&reply);
        assert_eq!(status["translate_enabled"], true);
        assert_eq!(status["speech_enabled"], true);
        assert_eq!(status["translate_queue_depth"], 1);
        assert_eq!(status["speech_queue_depth"], 0);
        assert_eq!(status["subtitles_published"], 1);
        assert_eq!(status["records_in_memory"], 1);
        assert_eq!(status["last_write_unix_ms"], 123);
        assert_eq!(
            status["subtitles_path"],
            fixture.dir.join("subtitles.jsonl").display().to_string()
        );
        assert!(status["degraded"].is_array());
        assert_eq!(status["version"], VERSION);
        // Task 5 字段保持零值。
        assert_eq!(status["translated"], 0);
        assert_eq!(status["llama_connected"], false);
        assert_eq!(status["tts_connected"], false);
    }

    #[test]
    fn translate_switch_toggles_and_persists() {
        let fixture = Fixture::new("translate");
        let reply = dispatch(
            &message(ctl::BRIDGE_TRANSLATE_SET, br#"{"enabled":false}"#),
            &fixture.shared,
            &fixture.queues,
        );
        assert_eq!(data(&reply)["translate_enabled"], false);
        assert!(!fixture.shared.translate_enabled());
        assert!(fixture.shared.speech_enabled());
        let saved = crate::state::load_switches(
            &fixture.dir.join(STATE_FILE_NAME),
            Switches { translate_enabled: true, speech_enabled: true },
        )
        .unwrap();
        assert_eq!(
            saved,
            Switches { translate_enabled: false, speech_enabled: true }
        );

        let reply = dispatch(
            &message(ctl::BRIDGE_TRANSLATE_SET, br#"{"enabled":true}"#),
            &fixture.shared,
            &fixture.queues,
        );
        assert_eq!(data(&reply)["translate_enabled"], true);
        assert!(fixture.shared.translate_enabled());
    }

    #[test]
    fn speech_switch_toggles_and_persists() {
        let fixture = Fixture::new("speech");
        let reply = dispatch(
            &message(ctl::BRIDGE_SPEECH_SET, br#"{"enabled":false}"#),
            &fixture.shared,
            &fixture.queues,
        );
        assert_eq!(data(&reply)["speech_enabled"], false);
        assert!(!fixture.shared.speech_enabled());
        assert!(fixture.shared.translate_enabled());
        let saved = crate::state::load_switches(
            &fixture.dir.join(STATE_FILE_NAME),
            Switches { translate_enabled: true, speech_enabled: true },
        )
        .unwrap();
        assert_eq!(
            saved,
            Switches { translate_enabled: true, speech_enabled: false }
        );
    }

    #[test]
    fn speech_stop_turns_off_and_clears_queue() {
        let fixture = Fixture::new("stop");
        fixture.queues.push_speech(SpeechJob {
            utterance_id: "s:1".to_string(),
            text: "hello".to_string(),
        });
        fixture.queues.push_speech(SpeechJob {
            utterance_id: "s:2".to_string(),
            text: "world".to_string(),
        });
        // 空 body 与 {} 都接受。
        for body in [b"".as_slice(), b"{}"] {
            let reply = dispatch(
                &message(ctl::BRIDGE_SPEECH_STOP, body),
                &fixture.shared,
                &fixture.queues,
            );
            let result = data(&reply);
            assert_eq!(result["speech_enabled"], false);
            assert_eq!(result["queue_cleared"], if body.is_empty() { 2 } else { 0 });
        }
        assert!(!fixture.shared.speech_enabled());
        assert_eq!(fixture.queues.speech_depth(), 0);
        let saved = crate::state::load_switches(
            &fixture.dir.join(STATE_FILE_NAME),
            Switches { translate_enabled: true, speech_enabled: true },
        )
        .unwrap();
        assert!(!saved.speech_enabled);
    }

    #[test]
    fn invalid_bodies_are_rejected() {
        let fixture = Fixture::new("invalid");
        for type_ in [ctl::BRIDGE_TRANSLATE_SET, ctl::BRIDGE_SPEECH_SET] {
            for body in [
                b"".as_slice(),
                b"{}",
                br#"{"enabled":"yes"}"#,
                br#"{"enabled":true,"x":1}"#,
                b"not json",
            ] {
                let reply = dispatch(&message(type_, body), &fixture.shared, &fixture.queues);
                assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
            }
        }
        for body in [b"not json".as_slice(), br#"{"x":1}"#] {
            let reply = dispatch(
                &message(ctl::BRIDGE_SPEECH_STOP, body),
                &fixture.shared,
                &fixture.queues,
            );
            assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
        }
        // 拒绝后开关不变。
        assert!(fixture.shared.translate_enabled());
        assert!(fixture.shared.speech_enabled());
    }

    #[test]
    fn unknown_command_is_rejected() {
        let fixture = Fixture::new("unknown");
        let reply = dispatch(&message(ctl::ASR_HELLO, b""), &fixture.shared, &fixture.queues);
        assert_eq!(error_code(&reply), ctl::CODE_UNKNOWN_COMMAND);
    }
}