//! 控制面 JSON 体契约：应答信封、请求体与 daemon 状态快照。
//! 字段名是 CLI 与以后网页适配器的稳定机器契约。

use crate::config::{PreDspConfig, RvcSessionValues};
use crate::denoise::DenoiseBackend;
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
pub enum OutputMode {
    Direct,
    Rvc,
    Broadcast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputPhase {
    ClosingSpeech,
    ConfiguringPlayout,
    Routing,
    OpeningSpeech,
    ReattachingPlayout,
    Applied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputModeStatus {
    pub requested: OutputMode,
    pub applied: Option<OutputMode>,
    pub phase: OutputPhase,
    pub last_error: Option<String>,
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

/// Optional additions to GetStatus, absent when read from an older daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusExtensions {
    #[serde(default)]
    pub daemon: DaemonStatusExtensions,
    #[serde(default)]
    pub file_limits: Option<FileLimits>,
    #[serde(default)]
    pub api_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonStatusExtensions {
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLimits {
    pub max_upload_bytes: u64,
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
    #[serde(default)]
    pub file_return_pending: bool,
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
    #[serde(default = "ProcessInfo::stopped")]
    pub enhance: ProcessInfo,
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
    #[serde(default = "ProcessInfo::stopped")]
    pub web: ProcessInfo,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayoutPolicy {
    #[default]
    Legacy,
    RealtimeOnly,
    BroadcastOnly,
    Silent,
}

pub const SUPERVISED_PLAYOUT_ENV: &str = "MOZART_SUPERVISED_PLAYOUT";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayoutSetRequest {
    pub policy: PlayoutPolicy,
    pub route: Route,
    pub mute: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PreMicMixerStatus {
    pub supported: bool,
    #[serde(default)]
    pub card_name: Option<String>,
    #[serde(default)]
    pub volume_percent: Option<u8>,
    #[serde(default)]
    pub gain_db: Option<f32>,
    #[serde(default)]
    pub min_db: Option<f32>,
    #[serde(default)]
    pub max_db: Option<f32>,
    #[serde(default)]
    pub agc_supported: bool,
    #[serde(default)]
    pub agc_enabled: Option<bool>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreStatus {
    #[serde(default)]
    pub playout_policy: PlayoutPolicy,
    #[serde(default)]
    pub enhancement: Option<PreEnhanceStatus>,
    #[serde(default)]
    pub aec: Option<PreAecStatus>,
    #[serde(default)]
    pub dsp: Option<PreDspConfig>,
    #[serde(default)]
    pub capture_gain_db: f32,
    #[serde(default)]
    pub mic_mixer: Option<PreMicMixerStatus>,
    #[serde(default = "default_denoise_backend")]
    pub denoise_backend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denoise_backends: Vec<DenoiseBackendInfo>,
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
    #[serde(default)]
    pub rvc_playout_gap_frames: u64,
    #[serde(default)]
    pub playback_underruns: u64,
    /// 文件接管状态（PRE_SET_FILE_MODE）：true 时 rvc AI 发送暂停、
    /// output_socket 已释放给 daemon。旧快照缺此字段按 false 还原。
    #[serde(default)]
    pub file_mode: bool,
    /// daemon 是否已权威断言模式（SPEC-04 单一权威）：pre 启动为 false，
    /// rvc AI 支路（发 input_socket + 绑 output_socket）静默；收到首个
    /// PRE_SET_FILE_MODE 置 true。daemon 据此与 startup_id 对账、重启重断言。
    /// 旧快照缺此字段按 false 还原。
    #[serde(default)]
    pub mode_asserted: bool,
    /// 每次进程启动随机生成，供 daemon 识别 pre 重启并重新断言模式。
    /// 旧快照缺此字段按空串还原。
    #[serde(default)]
    pub startup_id: String,
    pub last_error: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenoiseBackendInfo {
    pub id: String,
    pub label: String,
    pub stage: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreEnhanceStatus {
    pub enabled: bool,
    pub connected: bool,
    pub frames_in: u64,
    pub frames_out: u64,
    pub dropped_frames: u64,
    pub queue_depth: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreAecStatus {
    pub enabled: bool,
    pub backend: String,
    pub active: bool,
    pub delay_ms: Option<i32>,
    pub erle_db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erl_db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divergent_fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residual_echo_likelihood: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_median_ms: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_std_ms: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_delay_ms: Option<i32>,
    pub ref_dropped: u64,
    pub ref_underruns: u64,
    pub resets: u64,
    pub capture_clips: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnhanceStatus {
    pub version: String,
    pub backend: String,
    pub frames_in: u64,
    pub frames_out: u64,
    pub sessions: u64,
    pub algorithm_delay_samples: u32,
    pub last_error: Option<String>,
}

fn default_denoise_backend() -> String {
    DenoiseBackend::default().as_str().to_string()
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

/// RVC 会话参数当前生效值（status.rvc_params，SPEC-04 config set 编排）。
/// 取自 daemon 生效配置而不是 rvc 进程快照：参数经 StartStream / SetParams 下发，
/// 失败回退后 status 须显示仍在生效的旧值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RvcParams {
    pub pitch_shift: i32,
    pub index_rate: f64,
    pub rms_mix_rate: f64,
    pub protect: f64,
    pub f0_method: String,
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

fn default_services_active() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    #[serde(default = "default_services_active")]
    pub services_active: bool,
    #[serde(default)]
    pub enhance: Option<EnhanceStatus>,
    pub daemon: DaemonInfo,
    pub route: RouteState,
    #[serde(default)]
    pub output_mode: Option<OutputModeStatus>,
    pub mute: bool,
    pub arbiter: ArbiterState,
    pub processes: Processes,
    pub pre: Option<PreStatus>,
    pub rvc: Option<RvcStatus>,
    /// RVC 会话参数当前生效值（SPEC-04）。旧快照缺此字段按 None 还原。
    #[serde(default)]
    pub rvc_params: Option<RvcParams>,
    #[serde(default)]
    pub asr: Option<AsrStatus>,
    #[serde(default)]
    pub tts: Option<TtsStatus>,
    #[serde(default)]
    pub bridge: Option<BridgeStatus>,
    pub hw: Option<HardwareStatus>,
    pub degraded: Vec<String>,
    /// 写了重启键但尚未重启的服务名（SPEC-04 待重启显示）。旧快照缺此字段按空还原。
    #[serde(default)]
    pub pending_restart: Vec<String>,
    /// 尚未重启生效的键名（SPEC-04 键级待重启标记）。旧快照缺此字段按空还原。
    #[serde(default)]
    pub pending_restart_keys: Vec<String>,
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputModeSetRequest {
    pub mode: OutputMode,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MuteSetRequest {
    pub mute: bool,
}

/// 文件接管开关请求体（PRE_SET_FILE_MODE）：{"active": bool}。
/// active=true：pre 暂停 rvc AI 发送并释放 output_socket（交接给 daemon）；
/// active=false：恢复实时。asr 支路不受影响（SPEC-02 / SPEC-03 状态门控交接）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileModeSetRequest {
    pub active: bool,
}

/// DSP 级开关请求体（PRE_SET_DSP）：四个开关全部必需，显式完整状态，
/// 不做部分更新（SPEC-02 运行中切换 DSP 级）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DspSetRequest {
    pub denoise: bool,
    pub gate: bool,
    pub agc: bool,
    pub limiter: bool,
}

/// 降噪启停与后端切换请求体（PRE_SET_DENOISE）：至少提供 enabled 或 backend 之一。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenoiseSetRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<DenoiseBackend>,
}

/// 声学回声消除开关请求体（PRE_SET_AEC）：{"enabled": bool}。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AecSetRequest {
    pub enabled: bool,
}

/// 软件采集前级数字增益请求体（PRE_SET_CAPTURE_GAIN）：{"capture_gain_db": f32}。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureGainSetRequest {
    pub capture_gain_db: f32,
}

pub const MIC_MIXER_VOLUME_MAX: u8 = 100;

/// 采集卡硬件混音器调节请求体（DAEMON_MIC_MIXER_SET / PRE_SET_MIC_MIXER）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MicMixerSetRequest {
    #[serde(default)]
    pub volume_percent: Option<u8>,
    #[serde(default)]
    pub agc_enabled: Option<bool>,
}

impl MicMixerSetRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.volume_percent.is_none() && self.agc_enabled.is_none() {
            return Err("volume_percent or agc_enabled must be provided");
        }
        if self.volume_percent.is_some_and(|v| v > MIC_MIXER_VOLUME_MAX) {
            return Err("volume_percent must be within 0..=100");
        }
        Ok(())
    }
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

/// DAEMON_MODEL_USE 请求体。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUseRequest {
    pub model_id: String,
}

/// 音频设备条目（DAEMON_DEVICE_LIST 的 data.devices 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub id: String,
    pub label: String,
    /// capture/playback。
    pub kind: String,
}

/// DAEMON_DEVICE_LIST 请求体。缺省只列配置首选（plughw 与 default）；
/// all=true 返回完整 ALSA 别名清单。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceListRequest {
    #[serde(default)]
    pub all: bool,
}

/// 音频设备方向（DAEMON_DEVICE_USE / PRE_SET_DEVICE 共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Capture,
    Playback,
}

impl DeviceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Playback => "playback",
        }
    }

    pub fn config_key(self) -> &'static str {
        match self {
            Self::Capture => "pre.capture_device",
            Self::Playback => "pre.playback_device",
        }
    }
}

/// DAEMON_DEVICE_USE / PRE_SET_DEVICE 请求体：临时热切换音频设备。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceUseRequest {
    pub kind: DeviceKind,
    pub device: String,
}

/// DAEMON_CONFIG_SET 请求体（SPEC-04）：改单个键的持久基线。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSetRequest {
    pub key: String,
    pub value: String,
}

/// DAEMON_CONFIG_GET 请求体：key 缺省读整份基线。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetRequest {
    #[serde(default)]
    pub key: Option<String>,
}

/// DAEMON_RESTART 请求体；service 取进程名（pre/enhance/rvc/asr/tts/bridge/llama/web）。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartRequest {
    pub service: String,
}

/// DAEMON_PRESET_SAVE / DAEMON_PRESET_USE 的 data（SPEC-11）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetApplyData {
    pub model_id: String,
    pub preset: String,
    pub values: RvcSessionValues,
}

/// DAEMON_PRESET_LIST 的 data：预设值与 live 生效值及差异标记。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetListData {
    pub model_id: String,
    pub preset: String,
    pub preset_values: RvcSessionValues,
    pub live_values: RvcSessionValues,
    pub dirty: bool,
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_extensions_accept_old_and_new_responses() {
        let old: super::StatusExtensions = serde_json::from_str(r#"{"daemon":{"pid":1}}"#).unwrap();
        assert!(old.daemon.poll_interval_ms.is_none());
        assert!(old.file_limits.is_none());
        assert!(old.api_capabilities.is_empty());
        let new: super::StatusExtensions = serde_json::from_str(r#"{"daemon":{"poll_interval_ms":123},"file_limits":{"max_upload_bytes":456},"api_capabilities":["file_fetch_safe_errors_v1"]}"#).unwrap();
        assert_eq!(new.daemon.poll_interval_ms, Some(123));
        assert_eq!(new.file_limits.unwrap().max_upload_bytes, 456);
        assert_eq!(new.api_capabilities, ["file_fetch_safe_errors_v1"]);
    }
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
        assert_eq!(processes.web.state, ProcessState::Stopped);
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
        assert!(status.services_active);
        assert!(status.asr.is_none());
        assert!(status.tts.is_none());
        assert!(status.bridge.is_none());
    }

    #[test]
    fn status_rvc_params_defaults_to_none_and_round_trips() {
        // 旧 daemon 快照不含 rvc_params：按 None 还原，CLI 不显示该行。
        let mut status: Status = serde_json::from_value(status_json()).unwrap();
        assert!(status.rvc_params.is_none());

        status.rvc_params = Some(RvcParams {
            pitch_shift: -3,
            index_rate: 0.5,
            rms_mix_rate: 0.25,
            protect: 0.33,
            f0_method: "rmvpe".to_string(),
        });
        let json = serde_json::to_value(&status).unwrap();
        let params = &json["rvc_params"];
        assert_eq!(params["pitch_shift"], -3);
        assert_eq!(params["index_rate"], 0.5);
        assert_eq!(params["rms_mix_rate"], 0.25);
        assert_eq!(params["protect"], 0.33);
        assert_eq!(params["f0_method"], "rmvpe");
        let back: Status = serde_json::from_value(json).unwrap();
        assert_eq!(back.rvc_params, status.rvc_params);
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
        assert!(!status.file_mode, "旧快照缺 file_mode 按 false 还原");

        // 新快照往返稳定。
        let mut populated = status;
        populated.speaker_state = "broadcast".to_string();
        populated.tts_frames_played = 7;
        populated.file_mode = true;
        let json = serde_json::to_value(&populated).unwrap();
        assert_eq!(json["speaker_state"], "broadcast");
        assert_eq!(json["tts_frames_played"], 7);
        assert_eq!(json["file_mode"], true);
        let back: PreStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back.speaker_state, "broadcast");
        assert_eq!(back.tts_frames_played, 7);
        assert!(back.file_mode);
    }

    #[test]
    fn mic_mixer_set_request_validates_fields() {
        assert!(MicMixerSetRequest {
            volume_percent: Some(60),
            agc_enabled: None,
        }
        .validate()
        .is_ok());
        assert!(MicMixerSetRequest {
            volume_percent: None,
            agc_enabled: Some(true),
        }
        .validate()
        .is_ok());
        assert!(MicMixerSetRequest {
            volume_percent: None,
            agc_enabled: None,
        }
        .validate()
        .is_err());
        assert!(MicMixerSetRequest {
            volume_percent: Some(MIC_MIXER_VOLUME_MAX + 1),
            agc_enabled: None,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn file_mode_set_request_parses_active_and_rejects_extras() {
        let on: FileModeSetRequest = serde_json::from_str(r#"{"active":true}"#).unwrap();
        assert!(on.active);
        let off: FileModeSetRequest = serde_json::from_str(r#"{"active":false}"#).unwrap();
        assert!(!off.active);
        for bad in [
            "{}",
            r#"{"active":"yes"}"#,
            r#"{"active":true,"x":1}"#,
            "not json",
        ] {
            assert!(
                serde_json::from_str::<FileModeSetRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
        }
    }

    #[test]
    fn dsp_set_request_parses_four_switches_and_rejects_extras() {
        let request: DspSetRequest =
            serde_json::from_str(r#"{"denoise":false,"gate":true,"agc":true,"limiter":false}"#)
                .unwrap();
        assert!(!request.denoise);
        assert!(request.gate);
        assert!(request.agc);
        assert!(!request.limiter);
        for bad in [
            "{}",
            r#"{"denoise":true,"gate":false,"agc":false}"#,
            r#"{"denoise":"yes","gate":false,"agc":false,"limiter":false}"#,
            r#"{"denoise":true,"gate":false,"agc":false,"limiter":false,"x":1}"#,
            "not json",
        ] {
            assert!(
                serde_json::from_str::<DspSetRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
        }
    }

    #[test]
    fn device_use_request_parses_kind_and_rejects_extras() {
        let cap: DeviceUseRequest =
            serde_json::from_str(r#"{"kind":"capture","device":"plughw:CARD=MIC,DEV=0"}"#)
                .unwrap();
        assert_eq!(cap.kind, DeviceKind::Capture);
        assert_eq!(cap.device, "plughw:CARD=MIC,DEV=0");
        let pb: DeviceUseRequest =
            serde_json::from_str(r#"{"kind":"playback","device":"default"}"#).unwrap();
        assert_eq!(pb.kind, DeviceKind::Playback);
        assert_eq!(pb.device, "default");
        for bad in [
            "{}",
            r#"{"kind":"duplex","device":"default"}"#,
            r#"{"kind":"capture"}"#,
            r#"{"kind":"capture","device":"default","x":1}"#,
            "not json",
        ] {
            assert!(
                serde_json::from_str::<DeviceUseRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
        }
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
            assert!(
                serde_json::from_str::<SwitchSetRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
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
    fn preset_data_exposes_stable_fields() {
        let apply = PresetApplyData {
            model_id: "model-a".to_string(),
            preset: "default".to_string(),
            values: RvcSessionValues {
                pitch_shift: 3,
                index_rate: 0.5,
                rms_mix_rate: 0.25,
                protect: 0.33,
                f0_method: "rmvpe".to_string(),
            },
        };
        let json = round_trip(&apply);
        for key in ["model_id", "preset", "values"] {
            assert!(json.get(key).is_some(), "PresetApplyData 缺字段 {key}");
        }
        for key in [
            "pitch_shift",
            "index_rate",
            "rms_mix_rate",
            "protect",
            "f0_method",
        ] {
            assert!(json["values"].get(key).is_some(), "values 缺字段 {key}");
        }

        let list = PresetListData {
            model_id: "model-a".to_string(),
            preset: "default".to_string(),
            preset_values: apply.values.clone(),
            live_values: RvcSessionValues::default(),
            dirty: true,
        };
        let json = round_trip(&list);
        for key in [
            "model_id",
            "preset",
            "preset_values",
            "live_values",
            "dirty",
        ] {
            assert!(json.get(key).is_some(), "PresetListData 缺字段 {key}");
        }
        assert_eq!(json["dirty"], true);
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
            assert!(
                serde_json::from_str::<FileSubmitRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
        }
    }

    #[test]
    fn file_job_id_requests_parse_and_reject_extras() {
        let cancel: FileCancelRequest = serde_json::from_str(r#"{"job_id":"job_1"}"#).unwrap();
        assert_eq!(cancel.job_id, "job_1");
        let status: FileStatusRequest = serde_json::from_str(r#"{"job_id":"job_1"}"#).unwrap();
        assert_eq!(status.job_id, "job_1");
        for bad in ["{}", r#"{"job_id":1}"#, r#"{"job_id":"job_1","x":1}"#] {
            assert!(
                serde_json::from_str::<FileCancelRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
            assert!(
                serde_json::from_str::<FileStatusRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
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
            assert!(
                serde_json::from_str::<FileFetchRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
        }
    }

    #[test]
    fn restart_request_parses_and_rejects_extras() {
        let request: RestartRequest = serde_json::from_str(r#"{"service":"rvc"}"#).unwrap();
        assert_eq!(request.service, "rvc");
        for bad in ["{}", r#"{"service":1}"#, r#"{"service":"rvc","x":1}"#] {
            assert!(
                serde_json::from_str::<RestartRequest>(bad).is_err(),
                "应拒绝：{bad}"
            );
        }
    }
}
