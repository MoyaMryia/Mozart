//! 控制面服务端（SPEC-05）：asr.sock SEQPACKET + core 信封，Hello/GetStatus。
//! 只读原子计数与环形队列读数，不碰解码热路径；GetStatus 限频上报环丢帧
//! 增量（SPEC-01：热路径只计数，日志在控制线程）。连接逐个服务，断开回到 accept。

use crate::state::{unix_ms, Shared, VERSION};
use mozart_core::ctl::schema::{error_body, ok_body};
use mozart_core::ctl::{self, Message, Seqpacket, ACCEPT_POLL, IDLE_TIMEOUT, RECV_POLL};
use mozart_core::ring::Ring;
use crate::receiver::Inbox;
use serde_json::json;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 环丢帧日志的最小间隔（SPEC-01 限频）：窗口内的增量并入下一条。
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// 环丢帧限频上报（SPEC-01）：解码热路径只计数不打日志，控制线程在
/// GetStatus 发现增量时记录。距上次日志不足 DROP_LOG_INTERVAL 时不推进
/// 基线，增量并入下一条日志。
struct DropLog {
    /// 上次写入日志的累计值。
    last: u64,
    last_log: Option<Instant>,
}

impl DropLog {
    fn new() -> Self {
        Self { last: 0, last_log: None }
    }

    /// 只在控制线程调用，允许分配与阻塞。
    fn report(&mut self, total: u64) {
        let now = Instant::now();
        if let Some(last) = self.last_log {
            if now.duration_since(last) < DROP_LOG_INTERVAL {
                return;
            }
        }
        // 累计计数单调递增，差值即上次日志以来的增量。
        let delta = total - self.last;
        if delta == 0 {
            return;
        }
        eprintln!("mozart-asr: ring inbox dropped +{delta} (total {total}) unix_ms={}", unix_ms());
        self.last = total;
        self.last_log = Some(now);
    }
}

/// 服务循环：绑定与监听由调用方完成；停止标志置位后返回。
pub fn serve(
    listener: Seqpacket,
    shared: Arc<Shared>,
    ring: Arc<Ring<Inbox>>,
    stop: &'static AtomicBool,
) {
    let mut drop_log = DropLog::new();
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
        serve_connection(&conn, &shared, &ring, &mut drop_log, stop);
    }
}

fn serve_connection(
    conn: &Seqpacket,
    shared: &Shared,
    ring: &Ring<Inbox>,
    drop_log: &mut DropLog,
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
        let reply = dispatch(&msg, shared, ring, drop_log);
        if conn.send(type_, request_id, &reply).is_err() {
            return;
        }
    }
}

/// 命令分发。Hello/GetStatus 无请求体参数；未知命令拒绝。
fn dispatch(msg: &Message, shared: &Shared, ring: &Ring<Inbox>, drop_log: &mut DropLog) -> Vec<u8> {
    match msg.header.type_ {
        ctl::ASR_HELLO => ok_body(json!({"version": VERSION, "startup_id": shared.startup_id})),
        ctl::ASR_GET_STATUS => {
            let dropped = ring.dropped();
            drop_log.report(dropped);
            match serde_json::to_value(shared.status(dropped)) {
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
    use serde_json::Value;

    fn message(type_: u16) -> Message {
        Message {
            header: Header::new(type_, 7, 0),
            body: Vec::new(),
        }
    }

    fn context() -> (Arc<Shared>, Arc<Ring<Inbox>>, DropLog) {
        (
            Arc::new(Shared::new("teststart".to_string())),
            Arc::new(Ring::new(4)),
            DropLog::new(),
        )
    }

    fn data(reply: &[u8]) -> Value {
        let envelope: Value = serde_json::from_slice(reply).unwrap();
        assert_eq!(envelope["status"], "ok");
        envelope["data"].clone()
    }

    #[test]
    fn hello_reports_version_and_startup_id() {
        let (shared, ring, mut log) = context();
        let reply = dispatch(&message(ctl::ASR_HELLO), &shared, &ring, &mut log);
        let data = data(&reply);
        assert_eq!(data["version"], VERSION);
        assert_eq!(data["startup_id"], "teststart");
    }

    #[test]
    fn get_status_covers_spec_fields() {
        let (shared, ring, mut log) = context();
        shared.add_received(5);
        shared.add_final(2);
        let reply = dispatch(&message(ctl::ASR_GET_STATUS), &shared, &ring, &mut log);
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
        let (shared, ring, mut log) = context();
        for _ in 0..6 {
            ring.push(Inbox::EndOfInput);
        }
        let reply = dispatch(&message(ctl::ASR_GET_STATUS), &shared, &ring, &mut log);
        assert_eq!(data(&reply)["ring_dropped"], 2);
        // 丢帧增量同时交给限频上报，基线随读数推进。
        assert_eq!(log.last, 2);
        assert!(log.last_log.is_some());
    }

    /// 限频状态机（SPEC-01）：无增量不记；首次增量立即记并推进基线；
    /// 窗口内增量被抑制、基线不动；窗口过后合并上报。
    #[test]
    fn drop_log_rate_limits_and_merges_deltas() {
        let mut log = DropLog::new();
        log.report(0);
        assert!(log.last_log.is_none(), "无增量不得记日志时间");

        log.report(3);
        assert_eq!(log.last, 3);
        let first = log.last_log.expect("首次增量必须立即上报");

        log.report(5);
        assert_eq!(log.last, 3, "窗口内抑制时基线不动，增量留待合并");
        assert_eq!(log.last_log.unwrap(), first);

        // 回拨上次日志时间模拟窗口已过：合并增量一次上报。
        log.last_log = Some(first - DROP_LOG_INTERVAL);
        log.report(5);
        assert_eq!(log.last, 5);
        assert!(log.last_log.unwrap() > first);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let (shared, ring, mut log) = context();
        let reply = dispatch(&message(ctl::PRE_HELLO), &shared, &ring, &mut log);
        let envelope: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(envelope["status"], "error");
        assert_eq!(envelope["code"], ctl::CODE_UNKNOWN_COMMAND);
    }
}