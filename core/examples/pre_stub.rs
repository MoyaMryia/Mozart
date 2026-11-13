//! 音频进程协议桩：只实现 SPEC-02 控制面，不采集、不播放。
//! 测试旋钮 `MOZART_PRE_STUB_FLOOD=1`：模拟 pre 恒发支路，向 rvc-in.sock 持续
//! 发送实时 InputFrame（惰性建链、跟随对端重绑），受 PRE_SET_FILE_MODE 门控——
//! 文件数据面复用回归用：daemon 暂停 pre 后实时帧不得混入整段攒帧。
//! 测试旋钮 `MOZART_PRE_STUB_FILE_MODE_DELAY_MS`：PRE_SET_FILE_MODE 先执行
//! 门控再延迟应答，模拟「命令已生效而应答丢失/超时」（daemon rpc_timeout_ms
//! 小于该延迟）；缺省 0 不延迟，同 rvc 桩 delay/hang 旋钮性质。

mod stub_common;

use mozart_core::config::Config;
use mozart_core::ctl::{self, Message};
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, InputFrame, INPUT_SAMPLES};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use stub_common::{body, expect_known, fail, ok, parse_args, serve};

struct State {
    route: &'static str,
    mute: bool,
    file_mode: Arc<AtomicBool>,
    file_mode_delay: Duration,
}

fn main() {
    let args = parse_args("mozart-pre-stub");
    let file_mode = Arc::new(AtomicBool::new(false));
    let file_mode_delay = std::env::var("MOZART_PRE_STUB_FILE_MODE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::ZERO);
    if std::env::var("MOZART_PRE_STUB_FLOOD").is_ok_and(|value| value == "1") {
        let config = Config::load_from(args.config.as_deref()).unwrap_or_else(|err| {
            eprintln!("mozart-pre-stub: config: {err}");
            std::process::exit(1);
        });
        let path = config.rvc_input_socket();
        let gate = file_mode.clone();
        std::thread::spawn(move || flood(path, gate));
    }
    let mut state = State {
        route: "direct",
        mute: false,
        file_mode,
        file_mode_delay,
    };
    serve(args, move |msg| handle(msg, &mut state));
}

/// 模拟 pre 惰性恒发（ai.rs LazySender）：非阻塞发送，满则丢弃；发送失败弃用
/// 通道下一帧重建链（跟随 rvc 重绑）。file_mode=true 时停发（状态门控交接）。
fn flood(path: PathBuf, gate: Arc<AtomicBool>) {
    let mut socket: Option<FrameSocket<InputFrame>> = None;
    let mut idx = 0u32;
    loop {
        if !gate.load(Ordering::Relaxed) {
            if socket.is_none() {
                socket = FrameSocket::<InputFrame>::connect(&path, 8).ok();
            }
            if let Some(tx) = socket.as_ref() {
                let frame = InputFrame {
                    meta: FrameMeta {
                        pts_ns: u64::from(idx) * 20_000_000,
                        frame_idx: idx,
                        vad_flag: 1,
                        energy_db: 0,
                        conf: 0,
                        segment_id: 0,
                    },
                    pcm: [0.25; INPUT_SAMPLES],
                };
                if tx.send(&frame).is_err() {
                    socket = None;
                }
            }
            idx = idx.wrapping_add(1);
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}

fn handle(msg: &Message, state: &mut State) -> Vec<u8> {
    let known = [
        ctl::PRE_HELLO,
        ctl::PRE_SET_ROUTE,
        ctl::PRE_SET_MUTE,
        ctl::PRE_GET_STATUS,
        ctl::PRE_SPEECH_STOP,
        ctl::PRE_SET_FILE_MODE,
    ];
    if let Err(reply) = expect_known(msg, &known) {
        return reply;
    }
    let body = match body(msg) {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let type_ = msg.header.type_;
    match type_ {
        ctl::PRE_HELLO => ok(json!({"version": "0.1.0", "stub": true})),
        ctl::PRE_SET_ROUTE => {
            let route = body.get("route").and_then(|value| value.as_str());
            match route {
                Some("direct") => state.route = "direct",
                Some("rvc") => state.route = "rvc",
                _ => {
                    return fail(
                        ctl::CODE_INVALID_ARGUMENT,
                        "route must be \"direct\" or \"rvc\"",
                    )
                }
            }
            ok(json!({"route": state.route}))
        }
        ctl::PRE_SET_MUTE => {
            let Some(mute) = body.get("mute").and_then(|value| value.as_bool()) else {
                return fail(ctl::CODE_INVALID_ARGUMENT, "mute must be a boolean");
            };
            state.mute = mute;
            ok(json!({"mute": state.mute}))
        }
        ctl::PRE_SET_FILE_MODE => {
            let Some(active) = body.get("active").and_then(|value| value.as_bool()) else {
                return fail(ctl::CODE_INVALID_ARGUMENT, "active must be a boolean");
            };
            // 桩无数据面绑定，交接握手平凡成立：置门控即应答（恒发支路随即可见）。
            state.file_mode.store(active, Ordering::Relaxed);
            // 测试旋钮：先执行门控再延迟应答，模拟「命令已生效而应答丢失/超时」。
            if !state.file_mode_delay.is_zero() {
                std::thread::sleep(state.file_mode_delay);
            }
            ok(json!({"active": active}))
        }
        ctl::PRE_SPEECH_STOP => ok(json!({"stopped": true, "frames_cleared": 0})),
        ctl::PRE_GET_STATUS => ok(json!({
            "state": "ok",
            "route": state.route,
            "mute": state.mute,
            "capture_device": "",
            "playback_device": "",
            "frames_captured": 0,
            "frames_played": 0,
            "frames_dropped": 0,
            "queue_depth": 0,
            "consumer_drops": {},
            "speaker_state": "realtime",
            "tts_frames_played": 0,
            "file_mode": state.file_mode.load(Ordering::Relaxed),
            "last_error": null,
            "version": "0.1.0"
        })),
        _ => unreachable!("expect_known covers all commands"),
    }
}