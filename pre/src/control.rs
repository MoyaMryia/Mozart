//! 控制面服务端（SPEC-02）：Unix SEQPACKET + core 信封，复用 schema 的应答封装。
//! Hello/SetRoute/SetMute/SetFileMode/SetDsp/GetStatus。命令解析与执行只读写
//! 原子，不阻塞音频线程；GetStatus 只读原子快照与队列读数（AGENTS「GetStatus
//! 只读原子快照」精神），并限频上报环丢帧增量（SPEC-01：热路径只计数，日志在
//! 控制线程）。连接逐个服务：daemon 持单连接每秒轮询，断开后回到 accept。

use crate::state::{DspSwitches, QueueReadout, Shared, VERSION};
use mozart_core::ctl::schema::{
    error_body, ok_body, DspSetRequest, FileModeSetRequest, MuteSetRequest, RouteSetRequest,
};
use mozart_core::ctl::{self, Message, Seqpacket, ACCEPT_POLL, IDLE_TIMEOUT, RECV_POLL};
use mozart_core::dgram::FRAME_SIZE_MISMATCH_REASON;
use mozart_core::ring::Ring;
use mozart_core::{OutputFrame, RawFrame};
use serde_json::{json, Value};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 文件接管握手等待上限：接收线程按 RECV_POLL（100 ms）节拍释放绑定，
/// 正常百毫秒内完成；取 1 s 兜底，小于 daemon 缺省 rpc_timeout_ms（2 s）。
const FILE_HANDOFF_TIMEOUT: Duration = Duration::from_secs(1);
/// 握手等待的轮询步长。
const FILE_HANDOFF_POLL: Duration = Duration::from_millis(5);
/// 环丢帧日志的最小间隔（SPEC-01 限频）：窗口内的增量并入下一条。
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// 环丢帧限频上报（SPEC-01）：实时热路径只计数不打日志，控制线程在
/// GetStatus 发现增量时记录。距上次日志不足 DROP_LOG_INTERVAL 时不推进
/// 基线，增量并入下一条日志。另承载报文尺寸不符的一次性诊断（混跑属部署态，
/// 不随帧刷屏）。
struct DropLog {
    /// 上次写入日志的各环累计值，顺序与 report 的环列表一致。
    last: [u64; 4],
    last_log: Option<Instant>,
    /// 尺寸不符原因已记录：只记首次，之后计数仍经 consumer_drops 可查。
    frame_size_mismatch_logged: bool,
}

impl DropLog {
    fn new() -> Self {
        Self {
            last: [0; 4],
            last_log: None,
            frame_size_mismatch_logged: false,
        }
    }

    /// `rings` 为（环名, 当前累计）。只在控制线程调用，允许分配与阻塞。
    fn report(&mut self, rings: [(&'static str, u64); 4]) {
        let now = Instant::now();
        if let Some(last) = self.last_log {
            if now.duration_since(last) < DROP_LOG_INTERVAL {
                return;
            }
        }
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut logged = false;
        for (slot, (name, total)) in rings.iter().enumerate() {
            // 累计计数单调递增，差值即上次日志以来的增量。
            let delta = total - self.last[slot];
            if delta > 0 {
                eprintln!(
                    "mozart-pre: ring {name} dropped +{delta} (total {total}) unix_ms={at_ms}"
                );
                logged = true;
            }
        }
        if logged {
            self.last = rings.map(|(_, total)| total);
            self.last_log = Some(now);
        }
    }

    /// 报文尺寸不符（新旧进程混跑，帧契约扩字段）首次出现时记录可查询原因。
    /// 接收线程只发布原子计数，分配与加锁留在控制线程（AGENTS 实时线程约束）。
    fn report_frame_size_mismatch(&mut self, shared: &Shared) {
        if self.frame_size_mismatch_logged {
            return;
        }
        for (label, total) in [
            ("rvc output socket", shared.ai_recv_size_mismatch()),
            ("tts output socket", shared.tts_recv_size_mismatch()),
        ] {
            if total > 0 {
                let message = format!("{label}: {FRAME_SIZE_MISMATCH_REASON} (packets {total})");
                shared.set_last_error(&message);
                eprintln!("mozart-pre: {message}");
                self.frame_size_mismatch_logged = true;
                return;
            }
        }
    }
}

/// 服务循环：绑定与监听由调用方完成；收到停止标志后返回。
pub fn serve(
    listener: Seqpacket,
    shared: Arc<Shared>,
    capture_ring: Arc<Ring<crate::capture::CapturedFrame>>,
    trunk: Arc<Ring<RawFrame>>,
    rvc_ring: Arc<Ring<OutputFrame>>,
    tts_ring: Arc<Ring<OutputFrame>>,
    stop: &'static AtomicBool,
) {
    let mut drop_log = DropLog::new();
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
        serve_connection(
            &conn,
            &shared,
            &capture_ring,
            &trunk,
            &rvc_ring,
            &tts_ring,
            &mut drop_log,
            stop,
        );
    }
}

fn serve_connection(
    conn: &Seqpacket,
    shared: &Shared,
    capture_ring: &Ring<crate::capture::CapturedFrame>,
    trunk: &Ring<RawFrame>,
    rvc_ring: &Ring<OutputFrame>,
    tts_ring: &Ring<OutputFrame>,
    drop_log: &mut DropLog,
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
        let reply = dispatch(
            &msg,
            shared,
            capture_ring,
            trunk,
            rvc_ring,
            tts_ring,
            drop_log,
        );
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
fn dispatch(
    msg: &Message,
    shared: &Shared,
    capture_ring: &Ring<crate::capture::CapturedFrame>,
    trunk: &Ring<RawFrame>,
    rvc_ring: &Ring<OutputFrame>,
    tts_ring: &Ring<OutputFrame>,
    drop_log: &mut DropLog,
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
                    // 文件接管：先置 file_mode 停发（rvc_ai_active 立即为 false），
                    // 再置 mode_asserted——次序保证断言激活前恒发已停，杜绝
                    // 「mode_asserted 先 true、file_mode 尚 false」窗口抢发实时帧
                    // 污染整段攒帧。随后等接收线程释放输出 socket 的握手完成才
                    // 应答——daemon 收到 ok 后才绑定同一 socket，次序保证不撞绑。
                    // 本等待在控制线程，不阻塞音频线程。
                    shared.set_file_mode(true);
                    shared.set_mode_asserted(true);
                    let deadline = Instant::now() + FILE_HANDOFF_TIMEOUT;
                    while !shared.sends_idle() || !shared.output_released() {
                        if Instant::now() >= deadline {
                            // 接收线程未在时限内释放（异常态）：回滚门控，
                            // 不留「已暂停但无文件任务」的悬空态；原因经应答可查。
                            shared.set_file_mode(false);
                            return error_body(
                                ctl::CODE_NOT_READY,
                                "AI send/output socket handoff timed out",
                            );
                        }
                        std::thread::sleep(FILE_HANDOFF_POLL);
                    }
                    ok_body(json!({"active": true}))
                } else {
                    // 交还：daemon 已先解绑输出 socket；接收线程在下一节拍
                    // 重绑并恢复接收，处理线程恢复 rvc 发送。先置 file_mode 再
                    // 断言，实时恒发随断言激活（此支路实时帧属预期，非污染）。
                    shared.set_file_mode(false);
                    shared.set_mode_asserted(true);
                    ok_body(json!({"active": false}))
                }
            }
            Err(err) => error_body(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
        },
        ctl::PRE_SET_DSP => match serde_json::from_value::<DspSetRequest>(body) {
            Ok(request) => {
                // 入口校验由 serde 完成（四键必需、bool 类型、拒未知键）；
                // 执行只写目标原子，处理线程在一帧内应用（denoise 重新启用
                // 在 Dsp 内重置 WOLA 状态），与 SetRoute/SetMute 同纪律。
                let switches = DspSwitches {
                    denoise: request.denoise,
                    gate: request.gate,
                    agc: request.agc,
                    limiter: request.limiter,
                };
                shared.set_dsp_target(switches);
                ok_body(json!({
                    "denoise": switches.denoise,
                    "gate": switches.gate,
                    "agc": switches.agc,
                    "limiter": switches.limiter,
                }))
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
            let capture_dropped = capture_ring.dropped();
            let trunk_dropped = trunk.dropped();
            let rvc_dropped = rvc_ring.dropped();
            let tts_dropped = tts_ring.dropped();
            drop_log.report([
                ("capture", capture_dropped),
                ("trunk", trunk_dropped),
                ("rvc", rvc_dropped),
                ("tts", tts_dropped),
            ]);
            drop_log.report_frame_size_mismatch(shared);
            let readout = QueueReadout {
                depth: capture_ring.depth() as u64,
                dropped: capture_dropped + trunk_dropped,
                rvc_dropped,
                tts_dropped,
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
        Arc<Ring<crate::capture::CapturedFrame>>,
        Arc<Ring<RawFrame>>,
        Arc<Ring<OutputFrame>>,
        Arc<Ring<OutputFrame>>,
        DropLog,
    ) {
        (
            Arc::new(crate::state::test_shared("cap-dev", "play-dev", "test")),
            Arc::new(Ring::new(2)),
            Arc::new(Ring::new(2)),
            Arc::new(Ring::new(2)),
            Arc::new(Ring::new(2)),
            DropLog::new(),
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
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(ctl::PRE_HELLO, b"{}"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn hello_accepts_empty_body() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(ctl::PRE_HELLO, b""),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["version"], VERSION);
    }

    #[test]
    fn set_route_and_status_roundtrip() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":"rvc"}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["route"], "rvc");

        let reply = dispatch(
            &message(ctl::PRE_GET_STATUS, b"{}"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
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

    /// 混跑诊断：报文尺寸不符首次经 GetStatus 记录可查询原因，之后不重复
    /// 记录（计数继续增长，原因文本保持首次值，不随帧刷屏）。
    #[test]
    fn status_records_frame_size_mismatch_once() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(ctl::PRE_GET_STATUS, b"{}"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["last_error"], Value::Null);

        shared.set_ai_recv_size_mismatch(1);
        let reply = dispatch(
            &message(ctl::PRE_GET_STATUS, b"{}"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        let reason = data(&reply)["last_error"].as_str().unwrap().to_string();
        assert!(
            reason.contains(FRAME_SIZE_MISMATCH_REASON),
            "原因须说明尺寸不符与同代重建：{reason}"
        );
        assert!(reason.contains("rvc output socket"), "须点名支路：{reason}");

        shared.set_ai_recv_size_mismatch(50);
        let reply = dispatch(
            &message(ctl::PRE_GET_STATUS, b"{}"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["last_error"], json!(reason), "只记首次");
    }

    #[test]
    fn set_route_rejects_unknown_value() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":"bogus"}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);

        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, br#"{"route":42}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);

        let reply = dispatch(
            &message(ctl::PRE_SET_ROUTE, b"not json"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
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
            &mut log,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
    }

    #[test]
    fn set_mute_roundtrip_and_rejects_bad_body() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":true}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["mute"], true);
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":false}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["mute"], false);
        let reply = dispatch(
            &message(ctl::PRE_SET_MUTE, br#"{"mute":"yes"}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
    }

    #[test]
    fn set_dsp_roundtrip_and_updates_target() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(
                ctl::PRE_SET_DSP,
                br#"{"denoise":false,"gate":true,"agc":true,"limiter":false}"#,
            ),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        let applied = data(&reply);
        assert_eq!(applied["denoise"], false);
        assert_eq!(applied["gate"], true);
        assert_eq!(applied["agc"], true);
        assert_eq!(applied["limiter"], false);
        assert_eq!(
            shared.dsp_target(),
            DspSwitches {
                denoise: false,
                gate: true,
                agc: true,
                limiter: false,
            },
            "命令目标必须写入共享状态供处理线程应用"
        );

        // 再切回：目标随命令更新。
        let reply = dispatch(
            &message(
                ctl::PRE_SET_DSP,
                br#"{"denoise":true,"gate":false,"agc":false,"limiter":true}"#,
            ),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["denoise"], true);
        assert_eq!(
            shared.dsp_target(),
            DspSwitches {
                denoise: true,
                gate: false,
                agc: false,
                limiter: true,
            }
        );
    }

    #[test]
    fn set_dsp_rejects_bad_body() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let before = shared.dsp_target();
        for body in [
            &b"{}"[..],
            // 缺键。
            br#"{"denoise":true,"gate":false,"agc":false}"#,
            // 类型错误。
            br#"{"denoise":"yes","gate":false,"agc":false,"limiter":false}"#,
            // 未知键。
            br#"{"denoise":true,"gate":false,"agc":false,"limiter":false,"x":1}"#,
            b"not json",
        ] {
            let reply = dispatch(
                &message(ctl::PRE_SET_DSP, body),
                &shared,
                &cap,
                &trunk,
                &rvc,
                &tts,
                &mut log,
            );
            assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
            assert_eq!(shared.dsp_target(), before, "坏体不得改变 DSP 目标");
        }
    }

    #[test]
    fn unknown_command_is_rejected() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(ctl::DAEMON_HELLO, b"{}"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(error_code(&reply), ctl::CODE_UNKNOWN_COMMAND);
    }

    /// 限频状态机（SPEC-01）：无增量不记；首次增量立即记并推进基线；
    /// 窗口内增量被抑制、基线不动；窗口过后合并上报。
    #[test]
    fn drop_log_rate_limits_and_merges_deltas() {
        fn rings(capture: u64, trunk: u64, rvc: u64, tts: u64) -> [(&'static str, u64); 4] {
            [
                ("capture", capture),
                ("trunk", trunk),
                ("rvc", rvc),
                ("tts", tts),
            ]
        }
        let mut log = DropLog::new();
        log.report(rings(0, 0, 0, 0));
        assert!(log.last_log.is_none(), "无增量不得记日志时间");

        log.report(rings(3, 0, 2, 0));
        assert_eq!(log.last, [3, 0, 2, 0]);
        let first = log.last_log.expect("首次增量必须立即上报");

        log.report(rings(5, 1, 2, 0));
        assert_eq!(log.last, [3, 0, 2, 0], "窗口内抑制时基线不动，增量留待合并");
        assert_eq!(log.last_log.unwrap(), first);

        // 回拨上次日志时间模拟窗口已过：合并增量一次上报。
        log.last_log = Some(first - DROP_LOG_INTERVAL);
        log.report(rings(5, 1, 2, 0));
        assert_eq!(log.last, [5, 1, 2, 0]);
        assert!(log.last_log.unwrap() > first);
    }

    /// GetStatus 把环丢帧读数交给限频上报：应答口径不变，基线随读数推进。
    #[test]
    fn get_status_feeds_drop_log() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        // 采集环容量 2，塞 4 帧丢 2。
        for frame_idx in 0..4 {
            crate::capture::enqueue(
                &cap,
                RawFrame {
                    meta: mozart_core::FrameMeta {
                        pts_ns: 0,
                        frame_idx,
                        vad_flag: 1,
                        energy_db: 0,
                        conf: 0,
                        segment_id: 0,
                    },
                    pcm: [0.0; mozart_core::RAW_SAMPLES],
                },
                &shared,
            );
        }
        let reply = dispatch(
            &message(ctl::PRE_GET_STATUS, b"{}"),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
        );
        assert_eq!(data(&reply)["frames_dropped"], 2);
        assert_eq!(log.last, [2, 0, 0, 0]);
    }

    /// 文件接管握手：模拟接收线程按 file_mode 置/清 output_released，
    /// active=true 必须等握手完成才应答 ok；active=false 立即恢复。
    #[test]
    fn set_file_mode_waits_for_handshake_and_roundtrips() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
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
            &mut log,
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
            &mut log,
        );
        assert_eq!(data(&reply)["active"], false);
        assert!(!shared.file_mode());

        stop.store(true, Ordering::Relaxed);
        watcher.join().unwrap();
    }

    #[test]
    fn set_file_mode_waits_for_in_flight_send_permit() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        shared.set_mode_asserted(true);
        let generation = shared.file_generation();
        let permit = shared.begin_ai_send(generation).unwrap();
        shared.set_output_released(true);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let reply = dispatch(
                    &message(ctl::PRE_SET_FILE_MODE, br#"{"active":true}"#),
                    &shared,
                    &cap,
                    &trunk,
                    &rvc,
                    &tts,
                    &mut log,
                );
                tx.send(reply).unwrap();
            })
        };
        let deadline = Instant::now() + FILE_HANDOFF_TIMEOUT;
        while !shared.file_mode() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(30)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(!shared.sends_idle());
        drop(permit);
        let reply = rx.recv_timeout(FILE_HANDOFF_TIMEOUT).unwrap();
        assert_eq!(data(&reply)["active"], true);
        worker.join().unwrap();
        assert!(shared.sends_idle());
        shared.set_file_mode(false);
        assert!(
            shared.begin_ai_send(generation).is_none(),
            "旧帧不能跨文件生命周期"
        );
        let generation = shared.file_generation();
        shared.set_file_mode(false);
        shared.set_mode_asserted(true);
        assert_eq!(
            shared.file_generation(),
            generation,
            "周期相同断言不推进代次"
        );
        assert!(shared.begin_ai_send(generation).is_some());
    }

    #[test]
    fn set_file_mode_rejects_bad_body() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
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
                &mut log,
            );
            assert_eq!(error_code(&reply), ctl::CODE_INVALID_ARGUMENT);
            assert!(!shared.file_mode(), "坏体不得改变门控状态");
        }
    }

    /// 接收线程未释放（无握手）：active=true 在时限内回滚门控并报错，
    /// 不留「已暂停但无文件任务」的悬空态。
    #[test]
    fn set_file_mode_timeout_rolls_back() {
        let (shared, cap, trunk, rvc, tts, mut log) = context();
        let reply = dispatch(
            &message(ctl::PRE_SET_FILE_MODE, br#"{"active":true}"#),
            &shared,
            &cap,
            &trunk,
            &rvc,
            &tts,
            &mut log,
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

        let (shared, cap, trunk, rvc, tts, _log) = context();
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
        tts.push(OutputFrame::new_48k(
            mozart_core::FrameMeta {
                pts_ns: 0,
                frame_idx: 0,
                vad_flag: 1,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            [0.0; mozart_core::OUTPUT_SAMPLES],
        ));
        tts.push(OutputFrame::new_48k(
            mozart_core::FrameMeta {
                pts_ns: 0,
                frame_idx: 1,
                vad_flag: 1,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            [0.0; mozart_core::OUTPUT_SAMPLES],
        ));
        conn.send(ctl::PRE_SPEECH_STOP, 4, b"{}").unwrap();
        let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
        let stopped = data(&reply.body);
        assert_eq!(stopped["stopped"], true);
        assert_eq!(stopped["frames_cleared"], 2);
        assert_eq!(tts.depth(), 0, "停止后播报环必须清空");

        // SetDsp 经线缆往返：应答回显开关，目标写入共享状态。
        conn.send(
            ctl::PRE_SET_DSP,
            5,
            br#"{"denoise":false,"gate":false,"agc":false,"limiter":true}"#,
        )
        .unwrap();
        let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
        let reply_type = reply.header.type_;
        assert_eq!(reply_type, ctl::PRE_SET_DSP);
        let dsp = data(&reply.body);
        assert_eq!(dsp["denoise"], false);
        assert_eq!(dsp["limiter"], true);
        assert_eq!(
            shared.dsp_target(),
            DspSwitches {
                denoise: false,
                gate: false,
                agc: false,
                limiter: true,
            }
        );

        // 停止标志结束服务循环。
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }
}
