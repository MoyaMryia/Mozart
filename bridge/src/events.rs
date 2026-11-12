//! 事件面（SPEC-05）：连接 asr-events.sock SEQPACKET，一条消息一个 JSON 事件，
//! 裸 JSON 不带控制信封（收包用 core Seqpacket::recv_raw）。
//! 读循环只入队不做慢活：解析与启动号校验在此完成，落盘归字幕线程。
//! 断连自动重连；重连后首条事件确立新会话启动号，旧会话事件丢弃。

use crate::state::Shared;
use mozart_core::ctl::Seqpacket;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// 断连后的重连重试间隔。
const CONNECT_RETRY: Duration = Duration::from_millis(500);
/// 单次 connect 等待上限（监听积压满时不无限阻塞）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// 读循环轮询步长：让停止标志在宽限期内生效。
const RECV_POLL: Duration = Duration::from_millis(200);
/// 重连重试的停止检查步长。
const STOP_CHECK: Duration = Duration::from_millis(50);

/// 降级原因：事件面断连（含连接失败），重连成功后移除。
pub const DEGRADED_ASR_EVENTS: &str = "asr events disconnected";

pub const PARTIAL: &str = "partial";
pub const FINAL: &str = "final";
pub const REFINED: &str = "refined";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Partial,
    Final,
    Refined,
}

/// 事件 JSON 线上契约：必填四字段 + refined 可选字段，其余字段忽略。
#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    kind: String,
    startup_id: String,
    utterance_id: String,
    text: String,
    #[serde(default)]
    refined_text: Option<String>,
}

/// 解析后的事件：类型已校验。
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: EventKind,
    pub startup_id: String,
    pub utterance_id: String,
    pub text: String,
    pub refined_text: Option<String>,
}

/// 入口校验一次：JSON 结构与必填字段；未知类型返回 Ok(None) 忽略。
pub fn parse(bytes: &[u8]) -> Result<Option<Event>, String> {
    let raw: RawEvent =
        serde_json::from_slice(bytes).map_err(|err| format!("invalid event JSON: {err}"))?;
    let kind = match raw.kind.as_str() {
        PARTIAL => EventKind::Partial,
        FINAL => EventKind::Final,
        REFINED => EventKind::Refined,
        _ => return Ok(None),
    };
    if raw.startup_id.is_empty() {
        return Err("startup_id must not be empty".to_string());
    }
    if raw.utterance_id.is_empty() {
        return Err("utterance_id must not be empty".to_string());
    }
    Ok(Some(Event {
        kind,
        startup_id: raw.startup_id,
        utterance_id: raw.utterance_id,
        text: raw.text,
        refined_text: raw.refined_text,
    }))
}

/// 启动号校验：连接建立后首条事件确立当前会话，不匹配的事件丢弃。
/// 断连重置，重连后接纳新启动号（识别服务可能已重启）。
#[derive(Default)]
pub struct Session {
    startup_id: Option<String>,
}

impl Session {
    pub fn accept(&mut self, event: &Event) -> bool {
        match &self.startup_id {
            None => {
                self.startup_id = Some(event.startup_id.clone());
                true
            }
            Some(current) => current == &event.startup_id,
        }
    }

    pub fn reset(&mut self) {
        self.startup_id = None;
    }
}

/// 事件线程主循环：连接 → 读包入队 → 断连重连，直到停止标志置位。
pub fn run(
    path: PathBuf,
    tx: Sender<Event>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let mut session = Session::default();
    while !stop.load(Relaxed) {
        let conn = match Seqpacket::connect_timeout(&path, Some(CONNECT_TIMEOUT)) {
            Ok(conn) => conn,
            Err(err) => {
                shared.degrade(DEGRADED_ASR_EVENTS);
                eprintln!("mozart-bridge: connect {}：{err}", path.display());
                sleep_checked(CONNECT_RETRY, stop);
                continue;
            }
        };
        shared.resolve(DEGRADED_ASR_EVENTS);
        // 重连后重新确立会话：识别服务可能已重启换启动号。
        session.reset();
        read_loop(&conn, &tx, &mut session, stop);
        if stop.load(Relaxed) {
            break;
        }
        shared.degrade(DEGRADED_ASR_EVENTS);
    }
    Ok(())
}

/// 读循环：只解析、校验、入队；任何收包错误转断连重连。
fn read_loop(conn: &Seqpacket, tx: &Sender<Event>, session: &mut Session, stop: &AtomicBool) {
    while !stop.load(Relaxed) {
        let bytes = match conn.recv_raw(Some(RECV_POLL)) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(err) => {
                eprintln!("mozart-bridge: events recv：{err}");
                return;
            }
        };
        let event = match parse(&bytes) {
            Ok(Some(event)) => event,
            Ok(None) => continue,
            Err(reason) => {
                eprintln!("mozart-bridge: drop event: {reason}");
                continue;
            }
        };
        if !session.accept(&event) {
            continue;
        }
        if tx.send(event).is_err() {
            // 字幕线程已退出，停止标志由其致命错误路径置位。
            return;
        }
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
    use serde_json::json;

    fn event_json(kind: &str, startup: &str, seq: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "type": kind,
            "startup_id": startup,
            "utterance_id": format!("{startup}:{seq}"),
            "seq": seq,
            "text": "你好",
            "emitted_at": 1731234567.89,
        }))
        .unwrap()
    }

    #[test]
    fn parse_accepts_three_types_and_ignores_extra_fields() {
        for (kind, expected) in [
            (PARTIAL, EventKind::Partial),
            (FINAL, EventKind::Final),
            (REFINED, EventKind::Refined),
        ] {
            let event = parse(&event_json(kind, "a1b2c3d4", 1)).unwrap().unwrap();
            assert_eq!(event.kind, expected);
            assert_eq!(event.startup_id, "a1b2c3d4");
            assert_eq!(event.utterance_id, "a1b2c3d4:1");
            assert_eq!(event.text, "你好");
            assert_eq!(event.refined_text, None);
        }
    }

    #[test]
    fn parse_reads_refined_text() {
        let bytes = serde_json::to_vec(&json!({
            "type": "refined",
            "startup_id": "s",
            "utterance_id": "s:1",
            "text": "你好",
            "refined_text": "你好。",
            "refinement_status": "completed",
        }))
        .unwrap();
        let event = parse(&bytes).unwrap().unwrap();
        assert_eq!(event.kind, EventKind::Refined);
        assert_eq!(event.refined_text.as_deref(), Some("你好。"));
    }

    #[test]
    fn parse_rejects_invalid_json_and_missing_fields() {
        assert!(parse(b"{not json").is_err());
        assert!(parse(b"").is_err());
        // 缺必填字段。
        for body in [
            json!({"type": "final", "startup_id": "s", "text": "x"}),
            json!({"type": "final", "utterance_id": "s:1", "text": "x"}),
            json!({"startup_id": "s", "utterance_id": "s:1", "text": "x"}),
            json!({"type": "final", "startup_id": "s", "utterance_id": "s:1"}),
        ] {
            let bytes = serde_json::to_vec(&body).unwrap();
            assert!(parse(&bytes).is_err(), "应拒绝：{body}");
        }
        // 空启动号/句编号。
        for body in [
            json!({"type": "final", "startup_id": "", "utterance_id": "s:1", "text": "x"}),
            json!({"type": "final", "startup_id": "s", "utterance_id": "", "text": "x"}),
        ] {
            let bytes = serde_json::to_vec(&body).unwrap();
            assert!(parse(&bytes).is_err(), "应拒绝：{body}");
        }
    }

    #[test]
    fn parse_ignores_unknown_type() {
        assert_eq!(parse(&event_json("mystery", "s", 1)).unwrap(), None);
    }

    fn event(startup: &str) -> Event {
        parse(&event_json(FINAL, startup, 1)).unwrap().unwrap()
    }

    #[test]
    fn session_adopts_first_and_filters_mismatch() {
        let mut session = Session::default();
        assert!(session.accept(&event("aaaa")));
        assert!(session.accept(&event("aaaa")));
        assert!(!session.accept(&event("bbbb")));
        // 断连重置后接纳新启动号。
        session.reset();
        assert!(session.accept(&event("bbbb")));
        assert!(!session.accept(&event("aaaa")));
    }
}