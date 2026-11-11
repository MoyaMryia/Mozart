//! 控制面 JSON 体契约：应答信封、请求体与 daemon 状态快照。
//! 字段名是 CLI 与以后网页适配器的稳定机器契约。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: String,
    pub code: String,
    pub reason: String,
    pub data: Value,
}

pub fn ok_body(data: Value) -> Vec<u8> {
    encode(Response {
        status: "ok".to_string(),
        code: super::CODE_OK.to_string(),
        reason: String::new(),
        data,
    })
}

pub fn error_body(code: &str, reason: &str) -> Vec<u8> {
    encode(Response {
        status: "error".to_string(),
        code: code.to_string(),
        reason: reason.to_string(),
        data: json!(null),
    })
}

fn encode(response: Response) -> Vec<u8> {
    serde_json::to_vec(&response).expect("control response is JSON-serializable")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    Direct,
    Rvc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Activity {
    Realtime,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessState {
    Stopped,
    Starting,
    Running,
    Restarting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub version: String,
    pub uptime_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteState {
    pub desired: Route,
    pub active: Route,
    pub since_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuState {
    pub owner: Option<Activity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbiterState {
    pub gpu: GpuState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub state: ProcessState,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub last_exit: Option<ExitInfo>,
}

impl ProcessInfo {
    pub fn stopped() -> Self {
        Self {
            state: ProcessState::Stopped,
            pid: None,
            restarts: 0,
            last_exit: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Processes {
    pub pre: ProcessInfo,
    pub rvc: ProcessInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreStatus {
    pub state: String,
    pub route: Route,
    pub mute: bool,
    pub capture_device: String,
    pub playback_device: String,
    pub frames_captured: u64,
    pub frames_played: u64,
    pub frames_dropped: u64,
    pub queue_depth: u64,
    pub consumer_drops: std::collections::BTreeMap<String, u64>,
    pub last_error: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RvcStatus {
    pub state: String,
    pub model_id: Option<String>,
    pub model_version: Option<String>,
    pub capabilities: Vec<String>,
    pub stream_kind: Option<String>,
    pub frames_in: u64,
    pub frames_out: u64,
    pub frames_dropped: u64,
    pub blocks: u64,
    pub inference_ms_last: Option<f64>,
    pub inference_ms_avg: Option<f64>,
    pub inference_ms_p95: Option<f64>,
    pub queue_depth: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub available: bool,
    pub load_percent: Option<f64>,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareStatus {
    pub cpu_percent: Option<f64>,
    pub memory: Memory,
    pub gpu: GpuInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub daemon: DaemonInfo,
    pub route: RouteState,
    pub mute: bool,
    pub arbiter: ArbiterState,
    pub processes: Processes,
    pub pre: Option<PreStatus>,
    pub rvc: Option<RvcStatus>,
    pub hw: Option<HardwareStatus>,
    pub degraded: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSetRequest {
    pub route: Route,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MuteSetRequest {
    pub mute: bool,
}
