//! 推理进程协议桩：实现 SPEC-03 控制面，不加载模型、不推理。
//! 模型清单固定一个 demo：LoadModel 未知 id 回 InvalidArgument（model not
//! found），与真 rvc 的 NotFound → InvalidArgument 同口径（SPEC-10 报错归
//! 推理层，daemon 入队不预检）。
//! kind=file 时另跑整段批处理数据面（D1）：攒满 total_frames 帧 →
//! 零阶保持 3:1 上采样 → 阻塞回 total_frames 帧到配置默认 output_socket。
//! 数据面复用 [rvc] 默认 socket（SPEC-03 状态门控交接，无独立文件 socket）。
//! kind=realtime 仍只应答控制面（实时数据面回显见 rvc_echo.rs）。
//!
//! 测试旋钮 `MOZART_RVC_STUB_FILE_DELAY_MS`：攒满帧后模拟整段推理的等待，
//! 让取消用例稳定停在 infer 段；等待可被 StopStream 打断。
//! 测试旋钮 `MOZART_RVC_STUB_FILE_HANG=1`：绑定输入 socket 但永不读帧，
//! 模拟推理线程挂死（进程活着），让喂帧方填满缓冲后阻塞。
//! 测试旋钮 `MOZART_RVC_STUB_FAIL_START_PITCH=<i32>`：StartStream 的
//! pitch_shift 等于该值时回 InferenceFailed（仅一次），模拟新参起流失败。
//! 测试旋钮 `MOZART_RVC_STUB_FAIL_START_DELAY_MS=<u32>`：StartStream 的
//! startup_delay_ms 等于该值时回 InferenceFailed（仅一次），模拟旋钮起流失败。
//! 测试旋钮 `MOZART_RVC_STUB_FAIL_SET_PARAMS_PITCH=<i32>`：SetParams 的
//! pitch_shift 等于该值时回 InferenceFailed（仅一次），模拟在线调参失败。
//! 测试旋钮 `MOZART_RVC_STUB_FAIL_START_AFTER=<n>`：前 n 次 StartStream 成功，
//! 之后全部回 InferenceFailed，模拟新参与旧参恢复都失败。
//! 测试旋钮 `MOZART_RVC_STUB_STOP_KEEP_STREAM=1`：StopStream 应答成功但保持
//! 流在跑，模拟停流未实际生效（恢复起流会收到 Busy）。
//! 测试旋钮 `MOZART_RVC_STUB_STATUS_FAIL_FILE=<path>`：该文件存在时 GetStatus
//! 回 NotReady，模拟 rvc 不可达。
//! 测试旋钮 `MOZART_RVC_STUB_MODELS=<a,b>`：可加载模型清单（缺省 demo），
//! 供多模型切换用例使用。
//! 测试旋钮 `MOZART_RVC_STUB_PRELOAD=<id>`：启动即视为已加载该模型（不起流），
//! 模拟 daemon 连上已有模型的 rvc。

mod stub_common;

use mozart_core::config::Config;
use mozart_core::ctl::{self, Message};
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, InputFrame, OutputFrame, FRAME_MS, INPUT_SAMPLES, OUTPUT_SAMPLES};
use serde_json::{json, Value};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stub_common::{body, expect_known, fail, ok, parse_args, serve};

/// 数据面 socket 缓冲帧数：SPEC-02「接收缓冲 4–8 帧」取上沿。
const QUEUE_FRAMES: usize = 8;

/// 桩注册表唯一模型：LoadModel 与 ListModels 共用同一口径。
const STUB_MODEL: &str = "demo";

/// 攒帧与模拟推理期间的取消检查间隔。
const POLL: Duration = Duration::from_millis(20);

/// 48 kHz 输出 = 16 kHz 输入逐样本复制三份（零阶保持上采样，同 rvc_echo）。
const _: () = assert!(OUTPUT_SAMPLES == INPUT_SAMPLES * 3);

/// 批处理计数：停流后保留，供 GetStatus 查最终计数（同 C++ final_stats）。
#[derive(Default)]
struct Counters {
    frames_in: AtomicU64,
    frames_out: AtomicU64,
    blocks: AtomicU64,
}

struct Batch {
    cancel: Arc<AtomicBool>,
    counters: Arc<Counters>,
}

struct State {
    models: Vec<String>,
    model: Option<String>,
    streaming: bool,
    kind: Option<String>,
    batch: Option<Batch>,
    input_socket: PathBuf,
    output_socket: PathBuf,
    delay: Duration,
    hang: bool,
    feed_pace: Duration,
    /// 一次性失败：pitch_shift 命中后清空，供失败恢复用例重试成功。
    fail_start_pitch: Option<i32>,
    /// 一次性失败：startup_delay_ms 命中后清空，供 Stop/Start 失败恢复用例重试成功。
    fail_start_delay_ms: Option<u32>,
    /// 一次性失败：SetParams 的 pitch_shift 命中后清空，供 SetParams 失败用例重试成功。
    fail_set_params_pitch: Option<i32>,
    /// 前 n 次 StartStream 成功，之后全部失败。
    fail_start_after: Option<u32>,
    /// 置位时 StopStream 应答成功但不结束流，模拟停流未生效。
    stop_keep_stream: bool,
    /// 文件存在时 GetStatus 回错，模拟 rvc 不可达。
    status_fail_file: Option<PathBuf>,
    starts: u32,
}

fn main() {
    let args = parse_args("mozart-rvc-stub");
    // 数据面 socket 路径与 daemon 读同一 TOML（含 MOZART_RUN_DIR 覆盖）。
    let config = Config::load_from(args.config.as_deref()).unwrap_or_else(|err| {
        eprintln!("mozart-rvc-stub: config: {err}");
        std::process::exit(1);
    });
    let delay = std::env::var("MOZART_RVC_STUB_FILE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::ZERO);
    let hang = std::env::var("MOZART_RVC_STUB_FILE_HANG").is_ok_and(|value| value == "1");
    // 攒帧逐帧节流：拉宽攒帧窗，让共享输入 socket 上的实时帧污染可复现（回归用）。
    let feed_pace = std::env::var("MOZART_RVC_STUB_FILE_FEED_PACE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::ZERO);
    let fail_start_pitch = std::env::var("MOZART_RVC_STUB_FAIL_START_PITCH")
        .ok()
        .and_then(|value| value.parse::<i32>().ok());
    let fail_start_delay_ms = std::env::var("MOZART_RVC_STUB_FAIL_START_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let fail_set_params_pitch = std::env::var("MOZART_RVC_STUB_FAIL_SET_PARAMS_PITCH")
        .ok()
        .and_then(|value| value.parse::<i32>().ok());
    let fail_start_after = std::env::var("MOZART_RVC_STUB_FAIL_START_AFTER")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let stop_keep_stream =
        std::env::var("MOZART_RVC_STUB_STOP_KEEP_STREAM").is_ok_and(|value| value == "1");
    let status_fail_file = std::env::var_os("MOZART_RVC_STUB_STATUS_FAIL_FILE").map(PathBuf::from);
    let models = std::env::var("MOZART_RVC_STUB_MODELS")
        .map(|text| {
            text.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .ok()
        .filter(|models| !models.is_empty())
        .unwrap_or_else(|| vec![STUB_MODEL.to_string()]);
    let preload = std::env::var("MOZART_RVC_STUB_PRELOAD")
        .ok()
        .filter(|id| models.iter().any(|known| known == id));
    let mut state = State {
        models,
        model: preload,
        streaming: false,
        kind: None,
        batch: None,
        input_socket: config.rvc_input_socket(),
        output_socket: config.rvc_output_socket(),
        delay,
        hang,
        feed_pace,
        fail_start_pitch,
        fail_start_delay_ms,
        fail_set_params_pitch,
        fail_start_after,
        stop_keep_stream,
        status_fail_file,
        starts: 0,
    };
    serve(args, move |msg| handle(msg, &mut state));
}

fn handle(msg: &Message, state: &mut State) -> Vec<u8> {
    let known = [
        ctl::RVC_HELLO,
        ctl::RVC_LOAD_MODEL,
        ctl::RVC_START_STREAM,
        ctl::RVC_STOP_STREAM,
        ctl::RVC_RESET_STREAM,
        ctl::RVC_GET_STATUS,
        ctl::RVC_LIST_MODELS,
        ctl::RVC_SET_PARAMS,
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
        ctl::RVC_HELLO => ok(json!({"version": "0.1.0", "stub": true})),
        ctl::RVC_LOAD_MODEL => {
            let model_id = body.get("model_id").and_then(|value| value.as_str());
            let Some(model_id) = model_id.filter(|id| !id.is_empty()) else {
                return fail(ctl::CODE_INVALID_ARGUMENT, "model_id is required");
            };
            // 桩注册表默认固定 demo；未知 id 与真 rvc 同口径回 InvalidArgument。
            if !state.models.iter().any(|known| known == model_id) {
                return fail(
                    ctl::CODE_INVALID_ARGUMENT,
                    &format!("model not found: {model_id}"),
                );
            }
            state.model = Some(model_id.to_string());
            ok(json!({
                "model_id": model_id,
                "model_version": "stub",
                "model_handle": 1,
                "capabilities": ["realtime", "file"]
            }))
        }
        ctl::RVC_START_STREAM => {
            if state.model.is_none() {
                return fail(ctl::CODE_NOT_LOADED, "load a model first");
            }
            if state.streaming {
                return fail(ctl::CODE_BUSY, "stream already active");
            }
            state.starts += 1;
            let pitch = body
                .get("pitch_shift")
                .and_then(Value::as_i64)
                .and_then(|value| i32::try_from(value).ok());
            let startup_delay = body
                .get("startup_delay_ms")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            let fail_by_count = state
                .fail_start_after
                .is_some_and(|limit| state.starts > limit);
            let fail_by_pitch = state.fail_start_pitch.is_some() && pitch == state.fail_start_pitch;
            let fail_by_delay =
                state.fail_start_delay_ms.is_some() && startup_delay == state.fail_start_delay_ms;
            if fail_by_count || fail_by_pitch || fail_by_delay {
                if fail_by_pitch {
                    state.fail_start_pitch = None;
                }
                if fail_by_delay {
                    state.fail_start_delay_ms = None;
                }
                return fail(
                    ctl::CODE_INFERENCE_FAILED,
                    "stub rejects StartStream (test knob)",
                );
            }
            let kind = body
                .get("kind")
                .and_then(|value| value.as_str())
                .unwrap_or("realtime");
            if kind != "realtime" && kind != "file" {
                return fail(ctl::CODE_INVALID_ARGUMENT, "kind must be realtime or file");
            }
            if kind == "file" {
                if let Err(reply) = start_file_batch(state, &body) {
                    return reply;
                }
            }
            state.streaming = true;
            state.kind = Some(kind.to_string());
            ok(json!({"stream_kind": kind}))
        }
        ctl::RVC_STOP_STREAM | ctl::RVC_RESET_STREAM => {
            // 测试旋钮：应答成功但保持流在跑，模拟停流未实际生效。
            if state.stop_keep_stream && state.kind.as_deref() == Some("realtime") {
                return ok(json!({}));
            }
            // 只置取消：批处理线程在下一个检查点退出，控制面不等它
            // （回帧可能正阻塞在对端缓冲上，join 会与对端互等）。
            if let Some(batch) = state.batch.as_ref() {
                batch.cancel.store(true, Ordering::Release);
            }
            state.streaming = false;
            state.kind = None;
            ok(json!({}))
        }
        ctl::RVC_GET_STATUS => {
            // 测试旋钮：标记文件存在时模拟 rvc 状态不可达。
            if state
                .status_fail_file
                .as_ref()
                .is_some_and(|path| path.exists())
            {
                return fail(ctl::CODE_NOT_READY, "stub status unavailable (test knob)");
            }
            let state_name = if state.streaming {
                "streaming"
            } else if state.model.is_some() {
                "loaded"
            } else {
                "idle"
            };
            let counters = state.batch.as_ref().map(|batch| batch.counters.clone());
            let count = |get: fn(&Counters) -> &AtomicU64| {
                counters
                    .as_ref()
                    .map(|counters| get(counters).load(Ordering::Relaxed))
                    .unwrap_or(0)
            };
            ok(json!({
                "state": state_name,
                "model_id": state.model.as_ref(),
                "model_version": state.model.as_ref().map(|_| "stub"),
                "capabilities": ["realtime", "file"],
                "stream_kind": state.kind.as_ref(),
                "frames_in": count(|c| &c.frames_in),
                "frames_out": count(|c| &c.frames_out),
                "frames_dropped": 0,
                "blocks": count(|c| &c.blocks),
                "inference_ms_last": null,
                "inference_ms_avg": null,
                "inference_ms_p95": null,
                "queue_depth": 0,
                "last_error": null
            }))
        }
        ctl::RVC_LIST_MODELS => {
            // 与 LoadModel 同一注册表口径。
            let models: Vec<Value> = state
                .models
                .iter()
                .map(|id| {
                    json!({
                        "id": id,
                        "exists": true,
                        "loaded": state.model.as_deref() == Some(id.as_str()),
                    })
                })
                .collect();
            ok(json!({"models": models}))
        }
        ctl::RVC_SET_PARAMS => {
            let pitch = match validate_rvc_params(&body) {
                Ok(pitch) => pitch,
                Err(reply) => return reply,
            };
            if state.model.is_none() {
                return fail(ctl::CODE_NOT_LOADED, "load a model first");
            }
            if !state.streaming {
                return fail(ctl::CODE_NOT_READY, "no active stream");
            }
            if state.kind.as_deref() != Some("realtime") {
                return fail(
                    ctl::CODE_BUSY,
                    "cannot change parameters during an active file stream",
                );
            }
            if state.fail_set_params_pitch == Some(pitch) {
                state.fail_set_params_pitch = None;
                return fail(
                    ctl::CODE_INFERENCE_FAILED,
                    "stub rejects SetParams (test knob)",
                );
            }
            ok(json!({}))
        }
        _ => unreachable!("expect_known covers all commands"),
    }
}

/// SetParams 五参数与 skip_silence 校验（与 C++ StreamSession::handle_set_params 同口径）。
fn validate_rvc_params(body: &Value) -> Result<i32, Vec<u8>> {
    let invalid = |reason: &str| Err(fail(ctl::CODE_INVALID_ARGUMENT, reason));
    let Some(pitch_shift) = body.get("pitch_shift").and_then(Value::as_i64) else {
        return invalid("missing or malformed field: pitch_shift");
    };
    let Some(index_rate) = body.get("index_rate").and_then(Value::as_f64) else {
        return invalid("missing or malformed field: index_rate");
    };
    let Some(rms_mix_rate) = body.get("rms_mix_rate").and_then(Value::as_f64) else {
        return invalid("missing or malformed field: rms_mix_rate");
    };
    let Some(protect) = body.get("protect").and_then(Value::as_f64) else {
        return invalid("missing or malformed field: protect");
    };
    let Some(f0_method) = body.get("f0_method").and_then(Value::as_str) else {
        return invalid("missing or malformed field: f0_method");
    };
    if let Some(skip) = body.get("skip_silence") {
        if !skip.is_boolean() {
            return invalid("skip_silence must be a boolean");
        }
    }
    if f0_method != "rmvpe" {
        return invalid("parameter out of range: f0_method");
    }
    if !(-12..=12).contains(&pitch_shift) {
        return invalid("parameter out of range: pitch_shift");
    }
    if !index_rate.is_finite() || !(0.0..=1.0).contains(&index_rate) {
        return invalid("parameter out of range: index_rate");
    }
    if !rms_mix_rate.is_finite() || !(0.0..=1.0).contains(&rms_mix_rate) {
        return invalid("parameter out of range: rms_mix_rate");
    }
    if !protect.is_finite() || !(0.0..=0.5).contains(&protect) {
        return invalid("parameter out of range: protect");
    }
    Ok(pitch_shift as i32)
}

/// kind=file 入口校验一次（同 C++ StreamSession）：total_frames 正整数（u32）；
/// 随后绑定配置默认输入 socket 并起批处理线程。数据面复用 [rvc] 默认 socket
/// （SPEC-03 状态门控交接）：文件期间 pre 经 PRE_SET_FILE_MODE 暂停发送，
/// 实时帧不会混入攒帧。
fn start_file_batch(state: &mut State, body: &Value) -> Result<(), Vec<u8>> {
    let total = body
        .get("total_frames")
        .and_then(Value::as_u64)
        .filter(|total| *total > 0 && *total <= u64::from(u32::MAX));
    let Some(total) = total else {
        return Err(fail(
            ctl::CODE_INVALID_ARGUMENT,
            "total_frames must be a positive integer (u32)",
        ));
    };
    let rx = FrameSocket::<InputFrame>::bind(&state.input_socket, QUEUE_FRAMES)
        .map_err(|err| fail(ctl::CODE_INTERNAL, &format!("bind input socket: {err}")))?;
    let batch = Batch {
        cancel: Arc::new(AtomicBool::new(false)),
        counters: Arc::new(Counters::default()),
    };
    spawn_batch(
        rx,
        state.output_socket.clone(),
        total as usize,
        batch.cancel.clone(),
        batch.counters.clone(),
        state.delay,
        state.hang,
        state.feed_pace,
    );
    state.batch = Some(batch);
    Ok(())
}

fn spawn_batch(
    rx: FrameSocket<InputFrame>,
    output: PathBuf,
    total: usize,
    cancel: Arc<AtomicBool>,
    counters: Arc<Counters>,
    delay: Duration,
    hang: bool,
    feed_pace: Duration,
) {
    std::thread::spawn(move || {
        // 挂死对端：持有输入 socket 绑定但永不读帧，直到停流置取消。
        if hang {
            while !cancel.load(Ordering::Acquire) {
                std::thread::sleep(POLL);
            }
            return;
        }
        // ---- 攒帧：total_frames 个 320 样本输入帧 ----
        let mut audio: Vec<[f32; INPUT_SAMPLES]> = Vec::with_capacity(total);
        while audio.len() < total {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            match rx.recv(Some(POLL)) {
                Ok(frame) => {
                    audio.push(frame.pcm);
                    counters.frames_in.fetch_add(1, Ordering::Relaxed);
                    // 攒帧节流（回归旋钮）：拉宽攒帧窗，复现共享 socket 实时帧污染。
                    if !feed_pace.is_zero() {
                        std::thread::sleep(feed_pace);
                    }
                }
                Err(err) if err.kind() == ErrorKind::TimedOut => continue,
                Err(_) => return,
            }
        }
        // ---- 模拟整段推理：可取消的等待，整段一次过计 1 窗 ----
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()).min(POLL));
        }
        counters.blocks.store(1, Ordering::Relaxed);
        // ---- 回帧：48 kHz 整段切 960 样本，阻塞逐帧发，不丢 ----
        let tx = match FrameSocket::<OutputFrame>::connect(&output, QUEUE_FRAMES) {
            Ok(tx) => tx,
            Err(_) => return,
        };
        for (idx, pcm) in audio.iter().enumerate() {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let mut upsampled = [0f32; OUTPUT_SAMPLES];
            for (i, slot) in upsampled.iter_mut().enumerate() {
                *slot = pcm[i / 3];
            }
            let frame = OutputFrame::new_48k(
                FrameMeta {
                    pts_ns: idx as u64 * u64::from(FRAME_MS) * 1_000_000,
                    frame_idx: idx as u32,
                    vad_flag: 1,
                    energy_db: 0,
                    conf: 0,
                    segment_id: 0,
                },
                upsampled,
            );
            if tx.send_blocking(&frame).is_err() {
                return;
            }
            counters.frames_out.fetch_add(1, Ordering::Relaxed);
        }
    });
}
