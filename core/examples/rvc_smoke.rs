//! rvc 冒烟探针（Jetson 专用）：扮演 daemon 侧控制面 + pre 侧数据面，
//! 对拍 C++ 推理进程六命令语义并驱动真模型全链：
//! Hello →（NotLoaded / 坏模型 InvalidArgument 负例）→ LoadModel →
//! StartStream →（Busy 负例）→ ResetStream → DGRAM 帧收发 → GetStatus →
//! StopStream（幂等）→ 停流后 GetStatus，记录激活耗时、帧数守恒与非零率。
//!
//! 用法：rvc-smoke --control <rvc.sock> [--model qiqi-zh] [--frames 150]
//!                [--in <rvc-in.sock>] [--out <rvc-out.sock>]
//! 默认数据面 socket 相对控制 socket 所在目录解析（与 mozart-rvc 同一约定）。

use mozart_core::ctl::{self, schema::Response, Seqpacket};
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, InputFrame, OutputFrame, INPUT_SAMPLES};
use serde_json::{json, Value};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// pre 侧接收缓冲帧数（SPEC-02：4–8 帧取上沿）。
const QUEUE_FRAMES: usize = 8;
/// 控制往返超时：LoadModel 冷加载含多引擎 TRT 反序列化与预热（Nano 实测
/// 可到数十秒），放宽到 60 s；其余命令远快于此。
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
/// 输出帧逐帧超时：正常节拍 20 ms；欠载补零不超时，留裕量。
const RECV_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
struct ProbeError(String);

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn fail(message: impl Into<String>) -> ProbeError {
    ProbeError(message.into())
}

struct Client {
    conn: Seqpacket,
    next_id: u32,
}

impl Client {
    fn connect(path: &Path) -> io::Result<Self> {
        Ok(Self {
            conn: Seqpacket::connect_timeout(path, Some(RPC_TIMEOUT))?,
            next_id: 0,
        })
    }

    fn call(&mut self, type_: u16, body: &Value) -> Result<Response, ProbeError> {
        self.next_id = self.next_id.wrapping_add(1);
        let bytes = body.to_string();
        self.conn
            .send_timeout(type_, self.next_id, bytes.as_bytes(), Some(RPC_TIMEOUT))
            .map_err(|err| fail(format!("send: {err}")))?;
        let message = self
            .conn
            .recv(Some(RPC_TIMEOUT))
            .map_err(|err| fail(format!("recv: {err}")))?;
        // packed 头字段按值取出再比较。
        let reply_type = message.header.type_;
        let reply_id = message.header.request_id;
        if reply_type != type_ {
            return Err(fail(format!("reply type {reply_type:#06x} != {type_:#06x}")));
        }
        if reply_id != self.next_id {
            return Err(fail(format!("reply request_id {reply_id} != {}", self.next_id)));
        }
        serde_json::from_slice(&message.body).map_err(|err| fail(format!("parse reply: {err}")))
    }

    fn call_ok(&mut self, type_: u16, body: &Value) -> Result<Value, ProbeError> {
        let response = self.call(type_, body)?;
        if response.status != "ok" {
            return Err(fail(format!(
                "command {type_:#06x} failed: {} ({})",
                response.code, response.reason
            )));
        }
        Ok(response.data)
    }

    // 六命令负例对拍：期望指定错误码，status=error 且 code 相符。
    fn call_expect_error(
        &mut self,
        type_: u16,
        body: &Value,
        code: &str,
    ) -> Result<(), ProbeError> {
        let response = self.call(type_, body)?;
        if response.status != "error" {
            return Err(fail(format!(
                "command {type_:#06x} expected error {code}, got ok"
            )));
        }
        if response.code != code {
            return Err(fail(format!(
                "command {type_:#06x} expected error {code}, got {} ({})",
                response.code, response.reason
            )));
        }
        Ok(())
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("rvc-smoke: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), ProbeError> {
    let args = parse_args()?;
    eprintln!(
        "rvc-smoke: control={} model={} frames={} in={} out={}",
        args.control.display(),
        args.model,
        args.frames,
        args.input.display(),
        args.output.display()
    );

    // 先扮演 pre 绑定 output_socket，rvc StartStream 才能 connect 成功。
    let output_socket =
        FrameSocket::<OutputFrame>::bind(&args.output, QUEUE_FRAMES).map_err(|err| {
            fail(format!("bind {}: {err}", args.output.display()))
        })?;

    let mut client = Client::connect(&args.control)
        .map_err(|err| fail(format!("connect {}: {err}", args.control.display())))?;

    let hello = client.call_ok(ctl::RVC_HELLO, &json!({}))?;
    eprintln!("hello: version {}", hello["version"]);
    if hello["version"].as_str().map_or(true, str::is_empty) {
        return Err(fail("Hello response missing version"));
    }

    // 负例 1：未加载模型即开流 → NotLoaded（SPEC-03）。
    client.call_expect_error(
        ctl::RVC_START_STREAM,
        &json!({
            "kind": "realtime", "model_handle": 1, "pitch_shift": 0,
            "index_rate": 0.0, "rms_mix_rate": 1.0, "protect": 0.33,
            "f0_method": "rmvpe",
        }),
        "NotLoaded",
    )?;
    eprintln!("start_stream before load: NotLoaded ok");

    // 负例 2：不存在的模型 → InvalidArgument。
    client.call_expect_error(
        ctl::RVC_LOAD_MODEL,
        &json!({"model_id": "no-such-model"}),
        "InvalidArgument",
    )?;
    eprintln!("load_model missing: InvalidArgument ok");

    // 激活耗时：LoadModel 从发起到应答（冷加载含 TRT 反序列化与预热）。
    let load_started = Instant::now();
    let loaded = client.call_ok(
        ctl::RVC_LOAD_MODEL,
        &json!({"model_id": args.model}),
    )?;
    let activation = load_started.elapsed();
    eprintln!(
        "load_model: id={} version={} handle={} capabilities={} activation={:.0} ms",
        loaded["model_id"],
        loaded["model_version"],
        loaded["model_handle"],
        loaded["capabilities"],
        activation.as_millis()
    );
    let handle = loaded["model_handle"]
        .as_u64()
        .ok_or_else(|| fail("LoadModel response missing model_handle"))?;
    let realtime_cap = loaded["capabilities"]
        .as_array()
        .map(|caps| caps.iter().any(|c| c == "realtime"))
        .unwrap_or(false);
    if !realtime_cap {
        return Err(fail(format!(
            "model {} lacks realtime capability: {}",
            args.model, loaded["capabilities"]
        )));
    }

    let start_body = json!({
        "kind": "realtime",
        "model_handle": handle,
        "pitch_shift": 0,
        "index_rate": 0.0,
        "rms_mix_rate": 1.0,
        "protect": 0.33,
        "f0_method": "rmvpe",
    });
    client.call_ok(ctl::RVC_START_STREAM, &start_body)?;
    eprintln!("start_stream: realtime ok");

    // 负例 3：活动流上再开流 → Busy。
    client.call_expect_error(ctl::RVC_START_STREAM, &start_body, "Busy")?;
    eprintln!("start_stream while active: Busy ok");

    // 断续重置：泵数据前清空内部历史，后续计数从零累积。
    client.call_ok(ctl::RVC_RESET_STREAM, &json!({}))?;
    eprintln!("reset_stream: ok");

    // 数据面：rvc 已绑定 input_socket，此处连接发送。
    let input_socket = FrameSocket::<InputFrame>::connect(&args.input, QUEUE_FRAMES)
        .map_err(|err| fail(format!("connect {}: {err}", args.input.display())))?;

    let stop = Arc::new(AtomicBool::new(false));
    let sender = std::thread::spawn({
        let stop = stop.clone();
        let frames = args.frames;
        move || {
            // 16 kHz 正弦（220 Hz，0.25 幅度），20 ms 帧节拍。
            let started = Instant::now();
            for idx in 1..=frames {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut pcm = [0f32; INPUT_SAMPLES];
                for (i, slot) in pcm.iter_mut().enumerate() {
                    let phase = 2.0 * std::f32::consts::PI * 220.0
                        * ((started.elapsed().as_secs_f32() * 16000.0 + i as f32) / 16000.0);
                    *slot = 0.25 * phase.sin();
                }
                let frame = InputFrame {
                    meta: FrameMeta {
                        pts_ns: started.elapsed().as_nanos() as u64,
                        frame_idx: idx,
                        vad_flag: 1,
                        energy_db: 100,
                        conf: 200,
                        segment_id: 1,
                    },
                    pcm,
                };
                if input_socket.send(&frame).is_err() {
                    break;
                }
                // 20 ms 帧节拍；固定起点保证长期发送不超前音频时钟。
                let target = Duration::from_millis(u64::from(idx) * 20);
                let now = started.elapsed();
                if now < target {
                    std::thread::sleep(target - now);
                }
            }
        }
    });

    // 接收 N 帧：帧数守恒（1:1 泵）+ 输出非零率（启动缓冲后应为推理音频）。
    let mut received = 0usize;
    let mut nonzero = 0usize;
    let mut peak: f32 = 0.0;
    let mut first_nonzero_frame = None;
    while received < args.frames as usize {
        match output_socket.recv(Some(RECV_TIMEOUT)) {
            Ok(frame) => {
                received += 1;
                let pcm = frame.pcm;
                let amp = pcm.iter().fold(0.0f32, |acc, v| acc.max(v.abs()));
                peak = peak.max(amp);
                if amp > 1e-4 {
                    if first_nonzero_frame.is_none() {
                        first_nonzero_frame = Some(received);
                    }
                    nonzero += 1;
                }
            }
            Err(err) if err.kind() == ErrorKind::TimedOut => {
                stop.store(true, Ordering::Relaxed);
                return Err(fail(format!(
                    "output timeout after {received}/{} frames",
                    args.frames as usize
                )));
            }
            Err(err) => return Err(fail(format!("recv output: {err}"))),
        }
    }
    stop.store(true, Ordering::Relaxed);
    sender.join().map_err(|_| fail("sender panicked"))?;

    let status = client.call_ok(ctl::RVC_GET_STATUS, &json!({}))?;
    check_status_fields(&status, "streaming")?;
    eprintln!(
        "status: state={} frames_in={} frames_out={} dropped={} blocks={} \
         inference_ms(last/avg/p95)={:?}/{:?}/{:?} queue_depth={} last_error={:?}",
        status["state"],
        status["frames_in"],
        status["frames_out"],
        status["frames_dropped"],
        status["blocks"],
        status["inference_ms_last"],
        status["inference_ms_avg"],
        status["inference_ms_p95"],
        status["queue_depth"],
        status["last_error"]
    );

    client.call_ok(ctl::RVC_STOP_STREAM, &json!({}))?;
    eprintln!("stop_stream: ok");
    // StopStream 幂等：重复停流仍应答 ok。
    client.call_ok(ctl::RVC_STOP_STREAM, &json!({}))?;
    eprintln!("stop_stream again: ok (idempotent)");

    // 停流后 GetStatus：state=loaded、无流、最终计数可查。
    let frames = args.frames as usize;
    let final_status = client.call_ok(ctl::RVC_GET_STATUS, &json!({}))?;
    check_status_fields(&final_status, "loaded")?;
    if final_status["stream_kind"].is_null() {
        eprintln!("final status: stream_kind=null (stopped) ok");
    } else {
        return Err(fail("stream_kind must be null after StopStream"));
    }
    let frames_in = final_status["frames_in"].as_u64();
    let frames_out = final_status["frames_out"].as_u64();
    let dropped = final_status["frames_dropped"].as_u64();
    let expected = frames as u64; // u32 帧数，u64 无损
    if frames_in != Some(expected) || frames_out != Some(expected) || dropped != Some(0) {
        return Err(fail(format!(
            "counter mismatch after stop: frames_in={frames_in:?} \
             frames_out={frames_out:?} dropped={dropped:?} (expect {expected}/{expected}/0)"
        )));
    }
    eprintln!("final counters: frames_in={frames_in:?} frames_out={frames_out:?} dropped=0");

    // 验收：帧数守恒；启动缓冲（400 ms）后非零输出应占绝大多数。
    let silent_allowance = 25; // 400 ms 启动 + 240 ms 首块 → 32 帧，留裕量
    let verdict_ok =
        received == frames && nonzero + silent_allowance >= frames && peak > 1e-3;
    eprintln!(
        "verdict: received={received}/{frames} nonzero={nonzero} first_nonzero={first_nonzero_frame:?} peak={peak:.3} => {}",
        if verdict_ok { "PASS" } else { "FAIL" }
    );
    eprintln!(
        "rvc-smoke: 激活 {:.0} ms；{frames} 帧 {} s 守恒，非零 {nonzero} 帧",
        activation.as_millis(),
        args.frames * 20 / 1000
    );
    if verdict_ok {
        Ok(())
    } else {
        Err(fail("smoke verdict failed"))
    }
}

/// GetStatus 字段全量对拍：RvcStatus 契约（core/src/ctl/schema.rs）的
/// 14 个字段一个不少，state 与期望值相符。
fn check_status_fields(status: &Value, expect_state: &str) -> Result<(), ProbeError> {
    const FIELDS: [&str; 14] = [
        "state",
        "model_id",
        "model_version",
        "capabilities",
        "stream_kind",
        "frames_in",
        "frames_out",
        "frames_dropped",
        "blocks",
        "inference_ms_last",
        "inference_ms_avg",
        "inference_ms_p95",
        "queue_depth",
        "last_error",
    ];
    for field in FIELDS {
        if status.get(field).is_none() {
            return Err(fail(format!("GetStatus missing field: {field}")));
        }
    }
    if status["state"] != expect_state {
        return Err(fail(format!(
            "GetStatus state={} expected {expect_state}",
            status["state"]
        )));
    }
    Ok(())
}

struct Args {
    control: PathBuf,
    model: String,
    frames: u32,
    input: PathBuf,
    output: PathBuf,
}

fn parse_args() -> Result<Args, ProbeError> {
    let mut control = None;
    let mut model = "qiqi-zh".to_string();
    let mut frames = 150u32;
    let mut input = None;
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| fail(format!("missing value for {arg}")))?;
        match arg.as_str() {
            "--control" => control = Some(PathBuf::from(value)),
            "--model" => model = value,
            "--frames" => {
                frames = value
                    .parse()
                    .map_err(|_| fail("--frames must be an integer"))?;
            }
            "--in" => input = Some(PathBuf::from(value)),
            "--out" => output = Some(PathBuf::from(value)),
            _ => return Err(fail(format!("unknown argument: {arg}"))),
        }
    }
    let control = control.ok_or_else(|| {
        fail("usage: rvc-smoke --control <rvc.sock> [--model qiqi-zh] [--frames 150]")
    })?;
    let dir = control.parent().unwrap_or(Path::new("."));
    Ok(Args {
        input: input.unwrap_or_else(|| dir.join("rvc-in.sock")),
        output: output.unwrap_or_else(|| dir.join("rvc-out.sock")),
        control,
        model,
        frames,
    })
}
