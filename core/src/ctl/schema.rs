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
    /// llama-server 无控制协议，只监管进程存活；状态快照不含其业务字段。
    #[serde(default = "ProcessInfo::stopped")]
    pub llama: ProcessInfo,
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
    /// 扬声器分时状态："realtime" | "broadcast"（SPEC-05 播报投递：播报时
    /// 静音实时，播完恢复）。旧快照缺此字段按 realtime 还原。
    #[serde(default = "default_speaker_state")]
    pub speaker_state: String,
    /// 播报帧播放累计。旧快照缺此字段按 0 还原。
    #[serde(default)]
    pub tts_frames_played: u64,
    pub last_error: Option<String>,
    pub version: String,
}

fn default_speaker_state() -> String {
    "realtime".to_string()
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
    /// 解码/复核 → 输出有界通道满丢事件累计。旧快照缺此字段按 0 还原。
    #[serde(default)]
    pub event_channel_dropped: u64,
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
            event_channel_dropped: 0,
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
    /// 队满拒收（受理 skipped）累计。
    pub skipped: u64,
    /// TTS_JOB_STOP 取消的任务累计（在途中止 + 在队清空）。旧快照缺此字段按 0 还原。
    #[serde(default)]
    pub stopped: u64,
    pub queue_depth: u32,
    pub model_loaded: bool,
    /// 输出帧发送尝试累计（含丢弃）。
    pub frames_out: u64,
    /// 输出帧丢弃累计（无接收方或缓冲满）。
    pub output_dropped: u64,
    pub last_error: Option<String>,
    pub version: String,
}

impl Default for TtsStatus {
    fn default() -> Self {
        Self {
            state: "idle".to_string(),
            synthesized: 0,
            failed: 0,
            skipped: 0,
            stopped: 0,
            queue_depth: 0,
            model_loaded: false,
            frames_out: 0,
            output_dropped: 0,
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
    /// 事件→字幕有界通道满丢计数（partial 可丢，final/refined 阻塞保留）。
    pub events_dropped: u64,
    /// 坏事件（解析失败）丢弃计数。
    pub events_bad: u64,
    /// 跨启动号（旧会话）事件丢弃计数。
    pub events_stale: u64,
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
            events_dropped: 0,
            events_bad: 0,
            events_stale: 0,
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

/// 字幕链路开关请求体（DAEMON_TRANSLATE_SET / DAEMON_SPEECH_SET）：{"enabled": bool}。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchSetRequest {
    pub enabled: bool,
}

/// 文件任务六态（SPEC-04）：queued/processing/cancelling/completed/failed/cancelled。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileJobState {
    Queued,
    Processing,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

/// 文件任务进度：粗阶段 percent 0..=100；infer 段以块计数细化，缺则 None。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileProgress {
    /// decode/feed/infer/publish。
    pub stage: String,
    pub percent: u8,
    pub blocks_done: Option<u64>,
    pub blocks_total: Option<u64>,
}

/// 文件任务快照（DAEMON_FILE_STATUS 的 data、DAEMON_FILE_LIST 的 data 元素）。
/// model_id 在 submit 时定妥（显式指定或当前已加载模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileJob {
    pub job_id: String,
    /// 源文件名（结果缺省名取其主干 + .wav）。
    pub name: String,
    pub model_id: String,
    pub state: FileJobState,
    pub progress: FileProgress,
    pub error: Option<String>,
    /// completed 且结果未被驱逐时为 true，fetch 可取。
    pub result_ready: bool,
    pub created_unix_ms: u64,
}

/// DAEMON_FILE_SUBMIT 请求体；model_id 缺省用当前已加载模型。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSubmitRequest {
    pub path: String,
    pub model_id: Option<String>,
}

/// DAEMON_FILE_CANCEL 请求体。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCancelRequest {
    pub job_id: String,
}

/// DAEMON_FILE_FETCH 请求体。v1 同机：daemon 不复制文件，data 回
/// {"job_id","output_path","name"}（受管结果绝对路径 + 缺省名 = 源文件名主干
/// + .wav）；output 是 CLI 的复制目标，daemon 不使用，缺省名由 CLI 落在当前目录。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileFetchRequest {
    pub job_id: String,
    pub output: Option<String>,
}

/// DAEMON_FILE_STATUS 请求体。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileStatusRequest {
    pub job_id: String,
}

/// 模型清单条目（DAEMON_MODEL_LIST / RVC_LIST_MODELS 的 data.models 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    /// 模型文件在 models_dir 下存在。
    pub exists: bool,
    /// 推理进程当前已加载。
    pub loaded: bool,
}

/// 音频设备条目（DAEMON_DEVICE_LIST 的 data.devices 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub id: String,
    pub label: String,
    /// capture/playback。
    pub kind: String,
}

/// DAEMON_RESTART 请求体；service 取进程名（pre/rvc/asr/tts/bridge/llama）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartRequest {
    pub service: String,
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
            "skipped",
            "queue_depth",
            "model_loaded",
            "frames_out",
            "output_dropped",
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
        assert_eq!(processes.llama.state, ProcessState::Stopped);
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

    #[test]
    fn pre_status_speaker_fields_default_for_old_snapshots() {
        // 旧 pre 快照不含分时字段：按 realtime / 0 还原，daemon 轮询不报错。
        let old = json!({
            "state": "running",
            "route": "direct",
            "mute": false,
            "capture_device": "cap",
            "playback_device": "play",
            "frames_captured": 1,
            "frames_played": 2,
            "frames_dropped": 3,
            "queue_depth": 4,
            "consumer_drops": {"rvc": 0, "rvc.skipped": 0, "asr": 0},
            "last_error": null,
            "version": "0.1.0"
        });
        let status: PreStatus = serde_json::from_value(old).unwrap();
        assert_eq!(status.speaker_state, "realtime");
        assert_eq!(status.tts_frames_played, 0);

        // 新快照往返稳定。
        let mut populated = status;
        populated.speaker_state = "broadcast".to_string();
        populated.tts_frames_played = 7;
        let json = serde_json::to_value(&populated).unwrap();
        assert_eq!(json["speaker_state"], "broadcast");
        assert_eq!(json["tts_frames_played"], 7);
        let back: PreStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back.speaker_state, "broadcast");
        assert_eq!(back.tts_frames_played, 7);
    }

    #[test]
    fn switch_set_request_parses_enabled_and_rejects_extras() {
        let on: SwitchSetRequest = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(on.enabled);
        let off: SwitchSetRequest = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert!(!off.enabled);
        for bad in [
            "{}",
            r#"{"enabled":"yes"}"#,
            r#"{"enabled":true,"x":1}"#,
            "not json",
        ] {
            assert!(serde_json::from_str::<SwitchSetRequest>(bad).is_err(), "应拒绝：{bad}");
        }
    }

    #[test]
    fn file_job_state_serializes_lowercase() {
        let states = [
            (FileJobState::Queued, "queued"),
            (FileJobState::Processing, "processing"),
            (FileJobState::Cancelling, "cancelling"),
            (FileJobState::Completed, "completed"),
            (FileJobState::Failed, "failed"),
            (FileJobState::Cancelled, "cancelled"),
        ];
        for (state, text) in states {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(text));
            let back: FileJobState = serde_json::from_value(json!(text)).unwrap();
            assert_eq!(back, state);
        }
        // 大小写是稳定契约：大写变体名不接受。
        assert!(serde_json::from_str::<FileJobState>("\"Queued\"").is_err());
    }

    fn sample_file_job() -> FileJob {
        FileJob {
            job_id: "job_1700000000000000".to_string(),
            name: "input.wav".to_string(),
            model_id: "model-a".to_string(),
            state: FileJobState::Processing,
            progress: FileProgress {
                stage: "infer".to_string(),
                percent: 60,
                blocks_done: Some(12),
                blocks_total: Some(20),
            },
            error: None,
            result_ready: false,
            created_unix_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn file_job_round_trips_with_stable_fields() {
        let job = sample_file_job();
        let json = round_trip(&job);
        for key in [
            "job_id",
            "name",
            "model_id",
            "state",
            "progress",
            "error",
            "result_ready",
            "created_unix_ms",
        ] {
            assert!(json.get(key).is_some(), "FileJob 缺字段 {key}");
        }
        assert_eq!(json["state"], "processing");
        assert_eq!(json["progress"]["stage"], "infer");
        assert_eq!(json["progress"]["percent"], 60);
        assert_eq!(json["progress"]["blocks_done"], 12);
        assert_eq!(json["progress"]["blocks_total"], 20);

        // 失败态带原因；块计数缺省为 null。
        let mut failed = job.clone();
        failed.state = FileJobState::Failed;
        failed.error = Some("decode failed".to_string());
        failed.progress = FileProgress {
            stage: "decode".to_string(),
            percent: 0,
            blocks_done: None,
            blocks_total: None,
        };
        let json = round_trip(&failed);
        assert_eq!(json["state"], "failed");
        assert_eq!(json["error"], "decode failed");
        assert!(json["progress"]["blocks_done"].is_null());
        assert!(json["progress"]["blocks_total"].is_null());

        // 完成态结果可取。
        let mut completed = job;
        completed.state = FileJobState::Completed;
        completed.result_ready = true;
        completed.progress = FileProgress {
            stage: "publish".to_string(),
            percent: 100,
            blocks_done: None,
            blocks_total: None,
        };
        let json = round_trip(&completed);
        assert_eq!(json["state"], "completed");
        assert_eq!(json["result_ready"], true);
    }

    #[test]
    fn model_and_device_entries_round_trip() {
        let model = ModelEntry {
            id: "model-a".to_string(),
            exists: true,
            loaded: false,
        };
        let json = round_trip(&model);
        assert_eq!(json["id"], "model-a");
        assert_eq!(json["exists"], true);
        assert_eq!(json["loaded"], false);

        let capture = DeviceEntry {
            id: "plughw:0,0".to_string(),
            label: "Loopback".to_string(),
            kind: "capture".to_string(),
        };
        let json = round_trip(&capture);
        assert_eq!(json["id"], "plughw:0,0");
        assert_eq!(json["label"], "Loopback");
        assert_eq!(json["kind"], "capture");
        let playback = DeviceEntry {
            kind: "playback".to_string(),
            ..capture
        };
        assert_eq!(round_trip(&playback)["kind"], "playback");
    }

    #[test]
    fn file_submit_request_parses_and_rejects_extras() {
        let minimal: FileSubmitRequest = serde_json::from_str(r#"{"path":"/tmp/a.wav"}"#).unwrap();
        assert_eq!(minimal.path, "/tmp/a.wav");
        assert_eq!(minimal.model_id, None);
        let full: FileSubmitRequest =
            serde_json::from_str(r#"{"path":"/tmp/a.wav","model_id":"model-a"}"#).unwrap();
        assert_eq!(full.model_id.as_deref(), Some("model-a"));
        for bad in [
            "{}",
            r#"{"path":1}"#,
            r#"{"path":"/tmp/a.wav","model_id":2}"#,
            r#"{"path":"/tmp/a.wav","x":1}"#,
            "not json",
        ] {
            assert!(serde_json::from_str::<FileSubmitRequest>(bad).is_err(), "应拒绝：{bad}");
        }
    }

    #[test]
    fn file_job_id_requests_parse_and_reject_extras() {
        let cancel: FileCancelRequest = serde_json::from_str(r#"{"job_id":"job_1"}"#).unwrap();
        assert_eq!(cancel.job_id, "job_1");
        let status: FileStatusRequest = serde_json::from_str(r#"{"job_id":"job_1"}"#).unwrap();
        assert_eq!(status.job_id, "job_1");
        for bad in ["{}", r#"{"job_id":1}"#, r#"{"job_id":"job_1","x":1}"#] {
            assert!(serde_json::from_str::<FileCancelRequest>(bad).is_err(), "应拒绝：{bad}");
            assert!(serde_json::from_str::<FileStatusRequest>(bad).is_err(), "应拒绝：{bad}");
        }
    }

    #[test]
    fn file_fetch_request_parses_and_rejects_extras() {
        let minimal: FileFetchRequest = serde_json::from_str(r#"{"job_id":"job_1"}"#).unwrap();
        assert_eq!(minimal.job_id, "job_1");
        assert_eq!(minimal.output, None);
        let full: FileFetchRequest =
            serde_json::from_str(r#"{"job_id":"job_1","output":"/tmp/out.wav"}"#).unwrap();
        assert_eq!(full.output.as_deref(), Some("/tmp/out.wav"));
        for bad in [
            "{}",
            r#"{"job_id":"job_1","output":1}"#,
            r#"{"job_id":"job_1","x":1}"#,
        ] {
            assert!(serde_json::from_str::<FileFetchRequest>(bad).is_err(), "应拒绝：{bad}");
        }
    }

    #[test]
    fn restart_request_parses_and_rejects_extras() {
        let request: RestartRequest = serde_json::from_str(r#"{"service":"rvc"}"#).unwrap();
        assert_eq!(request.service, "rvc");
        for bad in ["{}", r#"{"service":1}"#, r#"{"service":"rvc","x":1}"#] {
            assert!(serde_json::from_str::<RestartRequest>(bad).is_err(), "应拒绝：{bad}");
        }
    }
}
