//! 音频进程协议桩：只实现 SPEC-02 控制面，不采集、不播放。
//! 测试旋钮 `MOZART_PRE_STUB_FLOOD=1`：模拟 pre 恒发支路，向 rvc-in.sock 持续
//! 发送实时 InputFrame（惰性建链、跟随对端重绑），受 PRE_SET_FILE_MODE 门控——
//! 文件数据面复用回归用：daemon 暂停 pre 后实时帧不得混入整段攒帧。
//! 测试旋钮 `MOZART_PRE_STUB_FILE_MODE_DELAY_MS`：PRE_SET_FILE_MODE 先执行
//! 门控再延迟应答，模拟「命令已生效而应答丢失/超时」（daemon rpc_timeout_ms
//! 小于该延迟）；缺省 0 不延迟，同 rvc 桩 delay/hang 旋钮性质。

#[allow(dead_code)]
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
use stub_common::{body, expect_known, fail, ok, serve, StubArgs};

struct PreOverrides {
    enhance: Option<Option<mozart_core::config::EnhanceConfig>>,
    dsp: Option<mozart_core::config::PreDspConfig>,
    capture_device: Option<String>,
    playback_device: Option<String>,
}

fn parse_pre_args() -> (StubArgs, PreOverrides) {
    let mut args = StubArgs {
        name: "mozart-pre-stub",
        socket: PathBuf::new(),
        config: None,
        exit_after: None,
        reply_delay: Duration::ZERO,
    };
    let mut overrides = PreOverrides {
        enhance: None,
        dsp: None,
        capture_device: None,
        playback_device: None,
    };
    let mut input = std::env::args().skip(1);
    while let Some(flag) = input.next() {
        let value = input.next().expect("missing argument value");
        match flag.as_str() {
            "--socket" => args.socket = value.into(),
            "--config" => args.config = Some(value.into()),
            "--exit-after-ms" => {
                args.exit_after = Some(Duration::from_millis(value.parse().unwrap()))
            }
            "--delay-ms" => args.reply_delay = Duration::from_millis(value.parse().unwrap()),
            "--effective-enhance" => overrides.enhance = Some(serde_json::from_str(&value).unwrap()),
            "--effective-dsp" => overrides.dsp = Some(serde_json::from_str(&value).unwrap()),
            "--effective-capture-device" => overrides.capture_device = Some(value),
            "--effective-playback-device" => overrides.playback_device = Some(value),
            _ => panic!("unknown argument: {flag}"),
        }
    }
    assert!(!args.socket.as_os_str().is_empty(), "--socket is required");
    (args, overrides)
}

struct State {
    route: &'static str,
    mute: bool,
    file_mode: Arc<AtomicBool>,
    /// daemon 权威断言标志（SPEC-04 单一权威）：启动 false，恒发支路静默，
    /// 首个 PRE_SET_FILE_MODE 置 true。重启同理，不单边 reclaim。
    mode_asserted: Arc<AtomicBool>,
    startup_id: String,
    file_mode_delay: Duration,
    dsp: mozart_core::config::PreDspConfig,
    capture_device: String,
    playback_device: String,
}

fn main() {
    let (args, overrides) = parse_pre_args();
    let mut config = Config::load_from(args.config.as_deref()).expect("pre stub config");
    if let Some(effective) = overrides.enhance {
        config
            .apply_effective_enhance(effective)
            .expect("effective enhancement");
    }
    if let Some(dsp) = overrides.dsp {
        config.pre.dsp = dsp;
    }
    if let Some(capture) = overrides.capture_device {
        config.pre.capture_device = capture;
    }
    if let Some(playback) = overrides.playback_device {
        config.pre.playback_device = playback;
    }
    let file_mode = Arc::new(AtomicBool::new(false));
    let mode_asserted = Arc::new(AtomicBool::new(false));
    let startup_id = format!("stub-{}", std::process::id());
    let file_mode_delay = std::env::var("MOZART_PRE_STUB_FILE_MODE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::ZERO);
    if std::env::var("MOZART_PRE_STUB_FLOOD").is_ok_and(|value| value == "1") {
        let path = config.rvc_input_socket();
        let gate = file_mode.clone();
        let asserted = mode_asserted.clone();
        std::thread::spawn(move || flood(path, gate, asserted));
    }
    let mut state = State {
        route: "direct",
        mute: false,
        file_mode,
        mode_asserted,
        startup_id,
        file_mode_delay,
        dsp: config.pre.dsp,
        capture_device: config.pre.capture_device,
        playback_device: config.pre.playback_device,
    };
    record_dsp(&state.dsp);
    serve(args, move |msg| handle(msg, &mut state));
}

/// 模拟 pre 惰性恒发（ai.rs LazySender）：非阻塞发送，满则丢弃；发送失败弃用
/// 通道下一帧重建链（跟随 rvc 重绑）。单一权威门控（SPEC-04）：仅当 daemon 已
/// 断言实时态（mode_asserted && !file_mode）才发；启动/重启未断言则静默，
/// 文件接管（file_mode=true）停发。与真 pre rvc_ai_active 同口径。
fn flood(path: PathBuf, gate: Arc<AtomicBool>, asserted: Arc<AtomicBool>) {
    let mut socket: Option<FrameSocket<InputFrame>> = None;
    let mut idx = 0u32;
    loop {
        if asserted.load(Ordering::Relaxed) && !gate.load(Ordering::Relaxed) {
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

fn record_dsp(dsp: &mozart_core::config::PreDspConfig) {
    if let Ok(path) = std::env::var("MOZART_PRE_STUB_DSP_STATE_FILE") {
        let temp = format!("{path}.tmp");
        std::fs::write(&temp, serde_json::to_vec(dsp).unwrap()).unwrap();
        std::fs::rename(temp, path).unwrap();
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
        ctl::PRE_SET_DSP,
        ctl::PRE_SET_DEVICE,
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
        ctl::PRE_SET_DSP => {
            if std::env::var("MOZART_PRE_STUB_DSP_REJECT_FILE")
                .is_ok_and(|path| std::path::Path::new(&path).exists())
            {
                return fail(ctl::CODE_INVALID_ARGUMENT, "DSP rejected by stub");
            }
            let request: mozart_core::ctl::schema::DspSetRequest =
                match serde_json::from_value(body.clone()) {
                    Ok(request) => request,
                    Err(err) => return fail(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
                };
            state.dsp = mozart_core::config::PreDspConfig {
                denoise: request.denoise,
                gate: request.gate,
                agc: request.agc,
                limiter: request.limiter,
            };
            record_dsp(&state.dsp);
            ok(serde_json::to_value(&state.dsp).unwrap())
        }
        ctl::PRE_SET_DEVICE => {
            if std::env::var("MOZART_PRE_STUB_DEVICE_REJECT_FILE")
                .is_ok_and(|path| std::path::Path::new(&path).exists())
            {
                return fail(ctl::CODE_INVALID_ARGUMENT, "device rejected by stub");
            }
            let request: mozart_core::ctl::schema::DeviceUseRequest =
                match serde_json::from_value(body.clone()) {
                    Ok(request) => request,
                    Err(err) => return fail(ctl::CODE_INVALID_ARGUMENT, &err.to_string()),
                };
            let target = match request.kind {
                mozart_core::ctl::schema::DeviceKind::Capture => &mut state.capture_device,
                mozart_core::ctl::schema::DeviceKind::Playback => &mut state.playback_device,
            };
            let previous = std::mem::replace(target, request.device.clone());
            ok(json!({
                "kind": request.kind.as_str(),
                "device": request.device,
                "previous_device": previous,
            }))
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
            // daemon 权威断言（SPEC-04）：先置 file_mode 再置 mode_asserted，
            // 杜绝「mode_asserted 先 true、file_mode 尚 false」窗口抢发实时帧。
            state.file_mode.store(active, Ordering::Relaxed);
            state.mode_asserted.store(true, Ordering::Relaxed);
            // 测试旋钮：先执行门控再延迟应答，模拟「命令已生效而应答丢失/超时」。
            if !state.file_mode_delay.is_zero() {
                std::thread::sleep(state.file_mode_delay);
            }
            ok(json!({"active": active}))
        }
        ctl::PRE_SPEECH_STOP => ok(json!({"stopped": true, "frames_cleared": 0})),
        ctl::PRE_GET_STATUS => ok(json!({
            "state": "ok",
            "dsp": state.dsp,
            "enhancement": std::env::var("MOZART_PRE_STUB_ENHANCEMENT_FILE").ok().map(|path| {
                serde_json::from_slice::<mozart_core::ctl::schema::PreEnhanceStatus>(
                    &std::fs::read(path).unwrap()
                ).unwrap()
            }),
            "route": state.route,
            "mute": state.mute,
            "capture_device": state.capture_device,
            "playback_device": state.playback_device,
            "frames_captured": 0,
            "frames_played": 0,
            "frames_dropped": 0,
            "queue_depth": 0,
            "consumer_drops": {},
            "speaker_state": "realtime",
            "tts_frames_played": 0,
            "file_mode": state.file_mode.load(Ordering::Relaxed),
            "mode_asserted": state.mode_asserted.load(Ordering::Relaxed),
            "startup_id": state.startup_id,
            "last_error": null,
            "version": "0.1.0"
        })),
        _ => unreachable!("expect_known covers all commands"),
    }
}
