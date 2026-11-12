//! 事件面（SPEC-05）：连接 asr-events.sock SEQPACKET，一条消息一个 JSON 事件，
//! 裸 JSON 不带控制信封（收包用 core Seqpacket::recv_raw）。
//! 读循环只入队不做慢活：解析与启动号校验在此完成，落盘归字幕线程。
//! 断连自动重连；重连后首条事件确立新会话启动号，旧会话事件丢弃。

use crate::state::Shared;
use mozart_core::ctl::Seqpacket;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{SyncSender, TrySendError};
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

/// 事件 JSON 线上契约：必填四字段 + 旧字幕 schema 所需的可选字段，其余忽略。
/// 审计字段（asr_numeric_audit 等）当前 ASR 未携带，解析为可空以备后续。
#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    kind: String,
    startup_id: String,
    utterance_id: String,
    text: String,
    #[serde(default)]
    refined_text: Option<String>,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    emitted_at: Option<f64>,
    #[serde(default)]
    audio_start_pts_ns: Option<u64>,
    #[serde(default)]
    audio_end_pts_ns: Option<u64>,
    #[serde(default)]
    text_raw: Option<String>,
    #[serde(default)]
    online_text: Option<String>,
    #[serde(default)]
    final_engine: Option<String>,
    #[serde(default)]
    final_decode_ms: Option<u64>,
    #[serde(default)]
    refinement_status: Option<String>,
    #[serde(default)]
    refinement_error: Option<String>,
    #[serde(default)]
    homophone_fixes: Option<Value>,
    #[serde(default)]
    asr_numeric_audit: Option<Value>,
    #[serde(default)]
    asr_polarity_audit: Option<Value>,
    #[serde(default)]
    translation_source_audit: Option<Value>,
    #[serde(default)]
    source_audio: Option<String>,
    #[serde(default)]
    source_audio_sha256: Option<String>,
    #[serde(default)]
    source_audio_seconds: Option<f64>,
}

/// 解析后的事件：类型已校验，可选字段透传给字幕记录。
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: EventKind,
    pub startup_id: String,
    pub utterance_id: String,
    pub text: String,
    pub refined_text: Option<String>,
    pub seq: Option<u64>,
    pub emitted_at: Option<f64>,
    pub audio_start_pts_ns: Option<u64>,
    pub audio_end_pts_ns: Option<u64>,
    pub text_raw: Option<String>,
    pub online_text: Option<String>,
    pub final_engine: Option<String>,
    pub final_decode_ms: Option<u64>,
    pub refinement_status: Option<String>,
    pub refinement_error: Option<String>,
    pub homophone_fixes: Option<Value>,
    pub asr_numeric_audit: Option<Value>,
    pub asr_polarity_audit: Option<Value>,
    pub translation_source_audit: Option<Value>,
    pub source_audio: Option<String>,
    pub source_audio_sha256: Option<String>,
    pub source_audio_seconds: Option<f64>,
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
        seq: raw.seq,
        emitted_at: raw.emitted_at,
        audio_start_pts_ns: raw.audio_start_pts_ns,
        audio_end_pts_ns: raw.audio_end_pts_ns,
        text_raw: raw.text_raw,
        online_text: raw.online_text,
        final_engine: raw.final_engine,
        final_decode_ms: raw.final_decode_ms,
        refinement_status: raw.refinement_status,
        refinement_error: raw.refinement_error,
        homophone_fixes: raw.homophone_fixes,
        asr_numeric_audit: raw.asr_numeric_audit,
        asr_polarity_audit: raw.asr_polarity_audit,
        translation_source_audit: raw.translation_source_audit,
        source_audio: raw.source_audio,
        source_audio_sha256: raw.source_audio_sha256,
        source_audio_seconds: raw.source_audio_seconds,
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
    tx: SyncSender<Event>,
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
        read_loop(&conn, &tx, &mut session, &shared, stop);
        if stop.load(Relaxed) {
            break;
        }
        shared.degrade(DEGRADED_ASR_EVENTS);
    }
    Ok(())
}

/// 读循环：只解析、校验、入队；任何收包错误转断连重连。
/// 有界通道：partial 满丢并计数，final/refined 阻塞保留；坏事件与跨启动号丢弃计数。
fn read_loop(
    conn: &Seqpacket,
    tx: &SyncSender<Event>,
    session: &mut Session,
    shared: &Shared,
    stop: &AtomicBool,
) {
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
                shared.add_event_bad();
                eprintln!("mozart-bridge: drop event: {reason}");
                continue;
            }
        };
        if !session.accept(&event) {
            shared.add_event_stale();
            continue;
        }
        match event.kind {
            EventKind::Partial => match tx.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => shared.add_event_dropped(),
                // 字幕线程已退出，停止标志由其致命错误路径置位。
                Err(TrySendError::Disconnected(_)) => return,
            },
            // final/refined 不可丢：阻塞等到通道有空位。
            _ => {
                if tx.send(event).is_err() {
                    return;
                }
            }
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

    /// 有界通道语义：partial 满丢并计数，final 阻塞保留（C2）。
    #[test]
    fn bounded_channel_drops_partial_and_keeps_final() {
        use crate::state::{Queues, Switches, STATE_FILE_NAME};
        use crate::testutil::temp_dir;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        use std::sync::{mpsc, Arc};

        let dir = temp_dir("evt-bound");
        let path = dir.join("asr-events.sock");
        let listener = Seqpacket::bind(&path).unwrap();
        listener.listen(1).unwrap();
        // 裸 SEQPACKET 客户端：事件面无控制信封，直接发 JSON。
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0, "socket: {}", std::io::Error::last_os_error());
        let client = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: sockaddr_un 可由全零位模式表示。
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_os_str().as_bytes();
        for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
        // SAFETY: client 有效；addr 指向已初始化的 sockaddr_un。
        let rc = unsafe {
            libc::connect(client.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len)
        };
        assert_eq!(rc, 0, "connect: {}", std::io::Error::last_os_error());
        let conn = listener.accept().unwrap();
        let send_json = |value: &serde_json::Value| {
            let bytes = serde_json::to_vec(value).unwrap();
            // SAFETY: client 有效；bytes 指向已初始化的缓冲。
            let sent = unsafe {
                libc::send(client.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL)
            };
            assert_eq!(sent as usize, bytes.len(), "send: {}", std::io::Error::last_os_error());
        };

        let shared = Arc::new(Shared::new(
            Switches { translate_enabled: false, speech_enabled: false },
            dir.join("subtitles.jsonl"),
            dir.join(STATE_FILE_NAME),
        ));
        let queues = Queues::new(1, 1);
        let stop = Arc::new(AtomicBool::new(false));
        // 容量 1：首条 partial 占满，第二条满丢，final 阻塞等待空位。
        let (tx, rx) = mpsc::sync_channel::<Event>(1);
        let reader = {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                read_loop(&conn, &tx, &mut Session::default(), &shared, &stop);
            })
        };

        send_json(&json!({"type":"partial","startup_id":"s","utterance_id":"s:1","text":"甲"}));
        send_json(&json!({"type":"partial","startup_id":"s","utterance_id":"s:1","text":"乙"}));
        send_json(&json!({"type":"final","startup_id":"s","utterance_id":"s:1","text":"丙"}));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while shared.status(&queues).events_dropped < 1 {
            assert!(std::time::Instant::now() < deadline, "等待满丢计数超时");
            std::thread::sleep(Duration::from_millis(10));
        }
        // 取走首条 partial 腾位，阻塞的 final 随即送达（不可丢）。
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.kind, EventKind::Partial);
        assert_eq!(first.text, "甲");
        let last = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(last.kind, EventKind::Final);
        assert_eq!(last.text, "丙");

        let status = shared.status(&queues);
        assert_eq!(status.events_dropped, 1);
        assert_eq!(status.events_bad, 0);
        assert_eq!(status.events_stale, 0);

        stop.store(true, Relaxed);
        reader.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}