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
    pub last_error: Option<String>,
}

impl ProcessInfo {
    pub fn stopped() -> Self {
        Self {
            state: ProcessState::Stopped,
            pid: None,
            restarts: 0,
            last_exit: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Processes {
    pub pre: ProcessInfo,
    pub rvc: ProcessInfo,
    #[serde(default = "ProcessInfo::stopped")]
    pub asr: ProcessInfo,
    #[serde(default = "ProcessInfo::stopped")]
    pub tts: ProcessInfo,
    #[serde(default = "ProcessInfo::stopped")]
    pub bridge: ProcessInfo,
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
    #[serde(default)]
    pub asr: Option<AsrStatus>,
    #[serde(default)]
    pub tts: Option<TtsStatus>,
    #[serde(default)]
    pub bridge: Option<BridgeStatus>,
    pub hw: Option<HardwareStatus>,
    pub degraded: Vec<String>,
}

/// ASR 进程状态快照（ASR_GET_STATUS 的 data）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AsrStatus {
    /// "idle" | "decoding" | "reviewing"。
    pub state: String,
    /// 每次启动随机生成，供 bridge 识别重启。
    pub startup_id: String,
    pub frames_received: u64,
    pub frames_rejected: u64,
    pub ring_dropped: u64,
    pub event_buffer_dropped: u64,
    pub finals: u64,
    pub review_skipped: u64,
    pub review_failed: u64,
    pub last_packet_unix_ms: u64,
    pub bridge_connected: bool,
    pub last_error: Option<String>,
    pub version: String,
}

impl Default for AsrStatus {
    fn default() -> Self {
        Self {
            state: "idle".to_string(),
            startup_id: String::new(),
            frames_received: 0,
            frames_rejected: 0,
            ring_dropped: 0,
            event_buffer_dropped: 0,
            finals: 0,
            review_skipped: 0,
            review_failed: 0,
            last_packet_unix_ms: 0,
            bridge_connected: false,
            last_error: None,
            version: String::new(),
        }
    }
}

/// TTS 进程状态快照（TTS_GET_STATUS 的 data）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsStatus {
    /// "idle" | "synthesizing"。
    pub state: String,
    pub synthesized: u64,
    pub failed: u64,
    pub queue_depth: u32,
    pub model_loaded: bool,
    pub last_error: Option<String>,
    pub version: String,
}

impl Default for TtsStatus {
    fn default() -> Self {
        Self {
            state: "idle".to_string(),
            synthesized: 0,
            failed: 0,
            queue_depth: 0,
            model_loaded: false,
            last_error: None,
            version: String::new(),
        }
    }
}

/// 字幕桥状态快照（BRIDGE_GET_STATUS 的 data）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeStatus {
    pub translate_enabled: bool,
    pub speech_enabled: bool,
    pub translate_queue_depth: u32,
    pub translated: u64,
    pub translate_failed: u64,
    pub translate_retried: u64,
    pub qc_rejected: u64,
    pub llama_connected: bool,
    pub last_translate_ok_unix_ms: u64,
    pub last_translate_fail_unix_ms: u64,
    pub last_translate_fail_reason: Option<String>,
    pub speech_queue_depth: u32,
    pub speech_delivered: u64,
    pub speech_dropped: u64,
    pub speech_submitted: u64,
    pub speech_skipped: u64,
    pub speech_failed: u64,
    pub tts_connected: bool,
    pub subtitles_published: u64,
    pub records_in_memory: u32,
    pub subtitles_path: String,
    pub last_write_unix_ms: u64,
    pub degraded: Vec<String>,
    pub version: String,
}

impl Default for BridgeStatus {
    fn default() -> Self {
        Self {
            translate_enabled: false,
            speech_enabled: false,
            translate_queue_depth: 0,
            translated: 0,
            translate_failed: 0,
            translate_retried: 0,
            qc_rejected: 0,
            llama_connected: false,
            last_translate_ok_unix_ms: 0,
            last_translate_fail_unix_ms: 0,
            last_translate_fail_reason: None,
            speech_queue_depth: 0,
            speech_delivered: 0,
            speech_dropped: 0,
            speech_submitted: 0,
            speech_skipped: 0,
            speech_failed: 0,
            tts_connected: false,
            subtitles_published: 0,
            records_in_memory: 0,
            subtitles_path: String::new(),
            last_write_unix_ms: 0,
            degraded: Vec::new(),
            version: String::new(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 序列化 → 反序列化 → 再序列化稳定，且与默认值一致。
    fn round_trip<T: Serialize + serde::de::DeserializeOwned>(value: &T) -> Value {
        let json = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), json);
        json
    }

    #[test]
    fn asr_status_covers_spec_fields() {
        let json = round_trip(&AsrStatus::default());
        for key in [
            "state",
            "startup_id",
            "frames_received",
            "frames_rejected",
            "ring_dropped",
            "event_buffer_dropped",
            "finals",
            "review_skipped",
            "review_failed",
            "last_packet_unix_ms",
            "bridge_connected",
            "last_error",
            "version",
        ] {
            assert!(json.get(key).is_some(), "AsrStatus 缺字段 {key}");
        }
        assert_eq!(json["state"], "idle");
        assert_eq!(json["bridge_connected"], false);
        assert_eq!(json["frames_received"], 0);
    }

    #[test]
    fn tts_status_covers_spec_fields() {
        let json = round_trip(&TtsStatus::default());
        for key in [
            "state",
            "synthesized",
            "failed",
            "queue_depth",
            "model_loaded",
            "last_error",
            "version",
        ] {
            assert!(json.get(key).is_some(), "TtsStatus 缺字段 {key}");
        }
        assert_eq!(json["state"], "idle");
        assert_eq!(json["model_loaded"], false);
    }

    #[test]
    fn bridge_status_covers_four_groups() {
        let json = round_trip(&BridgeStatus::default());
        // 翻译组、播报组、字幕组、降级原因各取代表字段。
        for key in [
            "translate_enabled",
            "translate_queue_depth",
            "translated",
            "translate_failed",
            "translate_retried",
            "qc_rejected",
            "llama_connected",
            "speech_enabled",
            "speech_queue_depth",
            "speech_delivered",
            "speech_dropped",
            "speech_submitted",
            "speech_skipped",
            "speech_failed",
            "tts_connected",
            "subtitles_published",
            "records_in_memory",
            "subtitles_path",
            "last_write_unix_ms",
            "degraded",
            "version",
        ] {
            assert!(json.get(key).is_some(), "BridgeStatus 缺字段 {key}");
        }
        assert_eq!(json["translate_enabled"], false);
        assert_eq!(json["speech_enabled"], false);
        assert!(json["degraded"].is_array());
    }

    #[test]
    fn language_status_defaults_from_empty_object() {
        // 容器级 #[serde(default)]：空对象即可还原默认快照。
        let asr: AsrStatus = serde_json::from_str("{}").unwrap();
        let tts: TtsStatus = serde_json::from_str("{}").unwrap();
        let bridge: BridgeStatus = serde_json::from_str("{}").unwrap();
        assert_eq!(
            serde_json::to_value(&asr).unwrap(),
            serde_json::to_value(&AsrStatus::default()).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&tts).unwrap(),
            serde_json::to_value(&TtsStatus::default()).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&bridge).unwrap(),
            serde_json::to_value(&BridgeStatus::default()).unwrap()
        );
    }

    fn stopped_json() -> Value {
        json!({
            "state": "stopped",
            "pid": null,
            "restarts": 0,
            "last_exit": null,
            "last_error": null
        })
    }

    #[test]
    fn processes_defaults_language_roles_to_stopped() {
        // 旧 daemon 快照只含 pre/rvc：新角色补默认 stopped。
        let json = json!({ "pre": stopped_json(), "rvc": stopped_json() });
        let processes: Processes = serde_json::from_value(json).unwrap();
        assert_eq!(processes.asr.state, ProcessState::Stopped);
        assert_eq!(processes.tts.state, ProcessState::Stopped);
        assert_eq!(processes.bridge.state, ProcessState::Stopped);
    }

    fn status_json() -> Value {
        json!({
            "daemon": { "pid": 1, "version": "0.1.0", "uptime_s": 10 },
            "route": { "desired": "direct", "active": "direct", "since_unix_ms": 0 },
            "mute": false,
            "arbiter": { "gpu": { "owner": null } },
            "processes": { "pre": stopped_json(), "rvc": stopped_json() },
            "pre": null,
            "rvc": null,
            "hw": null,
            "degraded": []
        })
    }

    #[test]
    fn status_language_snapshots_default_to_none() {
        let status: Status = serde_json::from_value(status_json()).unwrap();
        assert!(status.asr.is_none());
        assert!(status.tts.is_none());
        assert!(status.bridge.is_none());
    }

    #[test]
    fn status_round_trips_populated_language_snapshots() {
        let mut status: Status = serde_json::from_value(status_json()).unwrap();
        status.asr = Some(AsrStatus::default());
        status.tts = Some(TtsStatus::default());
        status.bridge = Some(BridgeStatus::default());
        let json = serde_json::to_value(&status).unwrap();
        let back: Status = serde_json::from_value(json).unwrap();
        assert!(back.asr.is_some());
        assert!(back.tts.is_some());
        assert!(back.bridge.is_some());
    }
}
