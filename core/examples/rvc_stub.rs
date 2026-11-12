//! 推理进程协议桩：实现 SPEC-03 控制面，不加载模型、不推理。
//! kind=file 时另跑整段批处理数据面（D1/D2）：攒满 total_frames 帧 →
//! 零阶保持 3:1 上采样 → 阻塞回 total_frames 帧到 output_socket。
//! kind=realtime 仍只应答控制面（实时数据面回显见 rvc_echo.rs）。
//!
//! 测试旋钮 `MOZART_RVC_STUB_FILE_DELAY_MS`：攒满帧后模拟整段推理的等待，
//! 让取消用例稳定停在 infer 段；等待可被 StopStream 打断。
//! 测试旋钮 `MOZART_RVC_STUB_FILE_HANG=1`：绑定输入 socket 但永不读帧，
//! 模拟推理线程挂死（进程活着），让喂帧方填满缓冲后阻塞。

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
    model: Option<String>,
    streaming: bool,
    kind: Option<String>,
    batch: Option<Batch>,
    input_socket: PathBuf,
    delay: Duration,
    hang: bool,
    feed_pace: Duration,
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
    let mut state = State {
        model: None,
        streaming: false,
        kind: None,
        batch: None,
        input_socket: config.rvc_input_socket(),
        delay,
        hang,
        feed_pace,
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
            // 桩模型目录固定一个 demo；已加载的其他模型一并列出。
            let mut ids = vec!["demo".to_string()];
            if let Some(model) = state.model.as_ref() {
                if model != "demo" {
                    ids.push(model.clone());
                }
            }
            let models: Vec<_> = ids
                .into_iter()
                .map(|id| {
                    let loaded = state.model.as_deref() == Some(id.as_str());
                    json!({"id": id, "exists": true, "loaded": loaded})
                })
                .collect();
            ok(json!({"models": models}))
        }
        _ => unreachable!("expect_known covers all commands"),
    }
}

/// kind=file 入口校验一次（同 C++ StreamSession）：total_frames 正整数（u32）、
/// output_socket / input_socket 绝对路径；随后绑定输入 socket 并起批处理线程。
/// input_socket 缺省回退实时 rvc-in.sock；文件链路传独立 socket，隔离 pre 恒发帧。
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
    let output = body
        .get("output_socket")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let Some(output) = output else {
        return Err(fail(
            ctl::CODE_INVALID_ARGUMENT,
            "output_socket must be an absolute path string",
        ));
    };
    let input = body
        .get("input_socket")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| state.input_socket.clone());
    let rx = FrameSocket::<InputFrame>::bind(&input, QUEUE_FRAMES)
        .map_err(|err| fail(ctl::CODE_INTERNAL, &format!("bind input socket: {err}")))?;
    let batch = Batch {
        cancel: Arc::new(AtomicBool::new(false)),
        counters: Arc::new(Counters::default()),
    };
    spawn_batch(
        rx,
        output,
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
            let frame = OutputFrame {
                meta: FrameMeta {
                    pts_ns: idx as u64 * u64::from(FRAME_MS) * 1_000_000,
                    frame_idx: idx as u32,
                    vad_flag: 1,
                    energy_db: 0,
                    conf: 0,
                    segment_id: 0,
                },
                pcm: upsampled,
            };
            if tx.send_blocking(&frame).is_err() {
                return;
            }
            counters.frames_out.fetch_add(1, Ordering::Relaxed);
        }
    });
}
