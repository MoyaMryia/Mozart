//! 控制面服务端（SPEC-05）：bridge.sock SEQPACKET + core 信封。
//! Hello/GetStatus 加字幕开关命令；开关改桥内活状态（持久存档归 daemon）。
//! 连接逐个服务，断开回到 accept；只读原子计数与队列深度，不碰落盘路径。

use crate::state::{Queues, Shared, Switches, VERSION};
use mozart_core::ctl::schema::{error_body, ok_body};
use mozart_core::ctl::{self, Message, Seqpacket, ACCEPT_POLL, IDLE_TIMEOUT, RECV_POLL};
use serde::Deserialize;
use serde_json::json;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

/// 开关请求体：{"enabled": bool}。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledRequest {
    enabled: bool,
}

/// 服务循环：绑定与监听由调用方完成；停止标志置位后返回。
pub fn serve(
    listener: Seqpacket,
    shared: Arc<Shared>,
    queues: Arc<Queues>,
    tts_jobs_path: PathBuf,
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
        serve_connection(&conn, &shared, &queues, &tts_jobs_path, stop);
    }
}

fn serve_connection(
    conn: &Seqpacket,
    shared: &Shared,
    queues: &Queues,
    tts_jobs_path: &Path,
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
                eprintln!("mozart-bridge: control recv: {err}");
                return;
            }
        };
        idle = Duration::ZERO;
        let type_ = msg.header.type_;
        let request_id = msg.header.request_id;
        let reply = dispatch(&msg, shared, queues, tts_jobs_path);
        if conn.send(type_, request_id, &reply).is_err() {
            return;
        }
    }
}

/// 命令分发。请求体在入口校验一次；未知命令拒绝。
pub fn dispatch(msg: &Message, shared: &Shared, queues: &Queues, tts_jobs_path: &Path) -> Vec<u8> {
    match msg.header.type_ {
        ctl::BRIDGE_HELLO => ok_body(json!({ "version": VERSION })),
        ctl::BRIDGE_GET_STATUS => {
            let status = shared.status(queues);
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
            let fenced = match parse_stop(&msg.body) {
                Ok(value) => value,
                Err(reason) => return error_body(ctl::CODE_INVALID_ARGUMENT, &reason),
            };
            let cutoff = shared.stop_speech();
            let cleared = queues.clear_speech();
            if !fenced {
                crate::speech::send_stop(tts_jobs_path, shared, cutoff);
            }
            ok_body(
                json!({ "speech_enabled": false, "queue_cleared": cleared, "cutoff_ns": cutoff }),
            )
        }
        ctl::BRIDGE_SPEECH_CLEAR => {
            let fenced = match parse_stop(&msg.body) {
                Ok(value) => value,
                Err(reason) => return error_body(ctl::CODE_INVALID_ARGUMENT, &reason),
            };
            let (speech_enabled, cutoff) = shared.clear_speech();
            let cleared = queues.clear_speech_before(cutoff);
            if !fenced {
                crate::speech::send_stop(tts_jobs_path, shared, cutoff);
            }
            ok_body(
                json!({ "speech_enabled": speech_enabled, "queue_cleared": cleared, "cutoff_ns": cutoff }),
            )
        }
        _ => error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
    }
}

fn set_switches(shared: &Shared, switches: Switches) -> Vec<u8> {
    shared.set_switches(switches);
    ok_body(json!({
        "translate_enabled": switches.translate_enabled,
        "speech_enabled": switches.speech_enabled,
    }))
}

fn parse_enabled(body: &[u8]) -> Result<bool, String> {
    let request: EnabledRequest =
        serde_json::from_slice(body).map_err(|err| format!("invalid switch body: {err}"))?;
    Ok(request.enabled)
}

fn parse_stop(body: &[u8]) -> Result<bool, String> {
    if body.is_empty() {
        return Ok(false);
    }
    let request: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(body).map_err(|err| format!("invalid stop body: {err}"))?;
    if request.is_empty() {
        Ok(false)
    } else if request.len() == 1 && request.get("fenced") == Some(&serde_json::Value::Bool(true)) {
        Ok(true)
    } else {
        Err("invalid stop body: expected {} or {\"fenced\":true}".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SpeechJob;
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
            );
            Self {
                dir,
                shared,
                queues: Queues::new(4, 4),
            }
        }

        fn tts_path(&self) -> PathBuf {
            self.dir.join("tts-jobs.sock")
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
        let reply = dispatch(
            &message(ctl::BRIDGE_HELLO, b""),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn get_status_covers_four_groups() {
        let fixture = Fixture::new("status");
        fixture.shared.add_published();
        fixture.shared.set_records_in_memory(1);
        fixture.shared.set_last_write_unix_ms(123);
        fixture.queues.push_translate(crate::state::TranslateJob {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
            speech_generation: None,
        });
        let reply = dispatch(
            &message(ctl::BRIDGE_GET_STATUS, b""),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
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
        // 翻译/播报组计数字段随共享状态上报。
        assert_eq!(status["translated"], 0);
        assert_eq!(status["llama_connected"], false);
        assert_eq!(status["tts_connected"], false);
        fixture.shared.translate_stats().add_translated();
        fixture.shared.translate_stats().attempt_fail("HTTP 500");
        fixture.shared.speech_stats().add_submitted();
        fixture.shared.speech_stats().set_connected(true);
        let reply = dispatch(
            &message(ctl::BRIDGE_GET_STATUS, b""),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        let status = data(&reply);
        assert_eq!(status["translated"], 1);
        assert_eq!(status["llama_connected"], false);
        assert_eq!(status["last_translate_fail_reason"], "HTTP 500");
        assert_eq!(status["speech_submitted"], 1);
        assert_eq!(status["tts_connected"], true);
    }

    #[test]
    fn translate_switch_toggles_live_state() {
        let fixture = Fixture::new("translate");
        let reply = dispatch(
            &message(ctl::BRIDGE_TRANSLATE_SET, br#"{"enabled":false}"#),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        assert_eq!(data(&reply)["translate_enabled"], false);
        assert!(!fixture.shared.translate_enabled());
        assert!(fixture.shared.speech_enabled());

        let reply = dispatch(
            &message(ctl::BRIDGE_TRANSLATE_SET, br#"{"enabled":true}"#),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        assert_eq!(data(&reply)["translate_enabled"], true);
        assert!(fixture.shared.translate_enabled());
    }

    #[test]
    fn speech_switch_toggles_live_state() {
        let fixture = Fixture::new("speech");
        let reply = dispatch(
            &message(ctl::BRIDGE_SPEECH_SET, br#"{"enabled":false}"#),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        assert_eq!(data(&reply)["speech_enabled"], false);
        assert!(!fixture.shared.speech_enabled());
        assert!(fixture.shared.translate_enabled());
    }

    #[test]
    fn speech_stop_turns_off_and_clears_queue() {
        let fixture = Fixture::new("stop");
        fixture.queues.push_speech(SpeechJob {
            utterance_id: "s:1".to_string(),
            text: "hello".to_string(),
            generation: crate::state::SpeechGeneration::FIRST,
        });
        fixture.queues.push_speech(SpeechJob {
            utterance_id: "s:2".to_string(),
            text: "world".to_string(),
            generation: crate::state::SpeechGeneration::FIRST,
        });
        // 空 body 与 {} 都接受。
        for body in [b"".as_slice(), b"{}"] {
            let reply = dispatch(
                &message(ctl::BRIDGE_SPEECH_STOP, body),
                &fixture.shared,
                &fixture.queues,
                &fixture.tts_path(),
            );
            let result = data(&reply);
            assert_eq!(result["speech_enabled"], false);
            assert_eq!(result["queue_cleared"], if body.is_empty() { 2 } else { 0 });
        }
        assert!(!fixture.shared.speech_enabled());
        assert_eq!(fixture.queues.speech_depth(), 0);
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
                let reply = dispatch(
                    &message(type_, body),
                    &fixture.shared,
                    &fixture.queues,
                    &fixture.tts_path(),
                );
                assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
            }
        }
        for body in [b"not json".as_slice(), br#"{"x":1}"#] {
            let reply = dispatch(
                &message(ctl::BRIDGE_SPEECH_STOP, body),
                &fixture.shared,
                &fixture.queues,
                &fixture.tts_path(),
            );
            assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
            let reply = dispatch(
                &message(ctl::BRIDGE_SPEECH_CLEAR, body),
                &fixture.shared,
                &fixture.queues,
                &fixture.tts_path(),
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
        let reply = dispatch(
            &message(ctl::ASR_HELLO, b""),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        assert_eq!(error_code(&reply), ctl::CODE_UNKNOWN_COMMAND);
    }

    #[test]
    fn fenced_stop_returns_cutoff_and_reopen_advances_token() {
        let fixture = Fixture::new("fenced-stop");
        let before = fixture.shared.speech_generation().unwrap().issued_ns();
        let reply = dispatch(&message(ctl::BRIDGE_SPEECH_STOP, br#"{"fenced":true}"#),
            &fixture.shared, &fixture.queues, &fixture.tts_path());
        let result = data(&reply);
        let cutoff = result["cutoff_ns"].as_u64().unwrap();
        assert!(cutoff >= before);
        assert_eq!(result["speech_enabled"], false);
        fixture.shared.set_switches(Switches { translate_enabled: true, speech_enabled: true });
        assert!(fixture.shared.speech_generation().unwrap().issued_ns() > cutoff);
    }

    #[test]
    fn speech_stop_sends_tts_job_stop() {
        let fixture = Fixture::new("stop-tts");
        let path = fixture.tts_path();
        let listener = Seqpacket::bind(&path).unwrap();
        listener.listen(1).unwrap();
        let server = std::thread::spawn(move || {
            let conn = listener.accept().unwrap();
            let msg = conn.recv(Some(Duration::from_secs(2))).unwrap();
            msg.header.type_
        });
        let reply = dispatch(
            &message(ctl::BRIDGE_SPEECH_STOP, b"{}"),
            &fixture.shared,
            &fixture.queues,
            &path,
        );
        assert_eq!(data(&reply)["speech_enabled"], false);
        // 桥侧 speech stop 向 TTS 下发 TTS_JOB_STOP。
        assert_eq!(server.join().unwrap(), ctl::TTS_JOB_STOP);
    }

    #[test]
    fn speech_stop_succeeds_when_tts_absent() {
        let fixture = Fixture::new("stop-absent");
        fixture.queues.push_speech(SpeechJob {
            utterance_id: "s:1".to_string(),
            text: "hello".to_string(),
            generation: crate::state::SpeechGeneration::FIRST,
        });
        // TTS 缺席：停止下发失败只记降级，桥侧仍关开关并清队列。
        let reply = dispatch(
            &message(ctl::BRIDGE_SPEECH_STOP, b"{}"),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        let result = data(&reply);
        assert_eq!(result["speech_enabled"], false);
        assert_eq!(result["queue_cleared"], 1);
        assert!(!fixture.shared.speech_enabled());
        assert_eq!(fixture.queues.speech_depth(), 0);
    }

    #[test]
    fn speech_clear_keeps_enabled_and_rotates_generation() {
        let fixture = Fixture::new("clear");
        let before = fixture.shared.speech_generation().unwrap();
        fixture.queues.push_speech(SpeechJob {
            utterance_id: "s:1".to_string(),
            text: "hello".to_string(),
            generation: before,
        });
        let reply = dispatch(
            &message(ctl::BRIDGE_SPEECH_CLEAR, br#"{"fenced":true}"#),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        let result = data(&reply);
        let cutoff = result["cutoff_ns"].as_u64().unwrap();
        assert_eq!(result["speech_enabled"], true);
        assert_eq!(result["queue_cleared"], 1);
        assert!(cutoff >= before.issued_ns());
        assert!(fixture.shared.speech_enabled());
        assert!(fixture.shared.speech_generation().unwrap().issued_ns() > cutoff);
        assert_eq!(fixture.queues.speech_depth(), 0);

        fixture.shared.set_switches(Switches {
            translate_enabled: true,
            speech_enabled: false,
        });
        let reply = dispatch(
            &message(ctl::BRIDGE_SPEECH_CLEAR, b"{}"),
            &fixture.shared,
            &fixture.queues,
            &fixture.tts_path(),
        );
        let result = data(&reply);
        assert_eq!(result["speech_enabled"], false);
        assert_eq!(result["queue_cleared"], 0);
        assert!(!fixture.shared.speech_enabled());
    }
}
