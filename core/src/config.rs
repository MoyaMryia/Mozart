//! daemon 与 ctl 共用的运行配置：TOML 文件 + 环境变量覆盖 + 入口校验。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub web: Option<WebConfig>,
    pub process: ProcessConfig,
    pub pre: PreConfig,
    pub enhance: Option<EnhanceConfig>,
    pub rvc: RvcConfig,
    pub asr: AsrConfig,
    pub tts: TtsConfig,
    pub bridge: BridgeConfig,
    pub file: FileConfig,
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub model_type: String,
    pub path: PathBuf,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartupMode {
    #[default]
    Rvc,
    Direct,
    Broadcast,
    Manual,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub startup_mode: StartupMode,
    pub run_dir: PathBuf,
    pub socket: String,
    pub poll_interval_ms: u64,
    pub rpc_timeout_ms: u64,
    pub start_timeout_ms: u64,
    pub restart_backoff_ms: u64,
    pub restart_backoff_max_ms: u64,
    pub restart_stable_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub listen: SocketAddr,
    pub static_dir: PathBuf,
    pub upload_dir: PathBuf,
    pub allowed_origins: Vec<String>,
    pub max_concurrent_uploads: usize,
    pub upload_timeout_ms: u64,
    pub sse_heartbeat_ms: u64,
    pub sse_buffer_bytes: usize,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".parse().unwrap(),
            static_dir: "web/dist".into(),
            upload_dir: "~/.local/state/mozart/web-uploads".into(),
            allowed_origins: [
                "http://localhost:8080",
                "http://127.0.0.1:8080",
                "http://[::1]:8080",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            max_concurrent_uploads: 2,
            upload_timeout_ms: 120_000,
            sse_heartbeat_ms: 15_000,
            sse_buffer_bytes: 1_048_576,
        }
    }
}

impl WebConfig {
    fn prepare(&mut self, path: Option<&Path>, home: Option<&Path>) -> Result<(), ConfigError> {
        let base = match path
            .and_then(Path::parent)
            .filter(|p| !p.as_os_str().is_empty())
        {
            Some(base) if base.is_absolute() => base.to_path_buf(),
            Some(base) => std::env::current_dir()
                .map_err(|e| ConfigError::Invalid(format!("current directory: {e}")))?
                .join(base),
            None => std::env::current_dir()
                .map_err(|e| ConfigError::Invalid(format!("current directory: {e}")))?,
        };
        self.static_dir = base.join(expand_tilde(&self.static_dir, home));
        self.upload_dir = base.join(expand_tilde(&self.upload_dir, home));
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, zero) in [
            ("max_concurrent_uploads", self.max_concurrent_uploads == 0),
            ("upload_timeout_ms", self.upload_timeout_ms == 0),
            ("sse_heartbeat_ms", self.sse_heartbeat_ms == 0),
            ("sse_buffer_bytes", self.sse_buffer_bytes == 0),
        ] {
            if zero {
                return Err(ConfigError::Invalid(format!(
                    "web.{name} must be greater than zero"
                )));
            }
        }
        Ok(())
    }
}

/// Thin client configuration: unrelated module tables are intentionally ignored.
#[derive(Debug, Clone)]
pub struct WebAdapterConfig {
    pub daemon: DaemonConfig,
    pub web: WebConfig,
    pub web_configured: bool,
    pub subtitles_path: PathBuf,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct AdapterDocument {
    daemon: DaemonConfig,
    web: Option<WebConfig>,
    bridge: AdapterBridge,
}

#[derive(Deserialize)]
#[serde(default)]
struct AdapterBridge {
    subtitles_path: PathBuf,
}
impl Default for AdapterBridge {
    fn default() -> Self {
        Self {
            subtitles_path: BridgeConfig::default().subtitles_path,
        }
    }
}

impl WebAdapterConfig {
    pub fn load_from(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut doc: AdapterDocument = match path {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
                toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?
            }
            None => AdapterDocument::default(),
        };
        doc.daemon.apply_env()?;
        doc.daemon.validate()?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let web_configured = doc.web.is_some();
        let mut web = doc.web.unwrap_or_default();
        web.prepare(path, home.as_deref())?;
        web.validate()?;
        Ok(Self {
            daemon: doc.daemon,
            web,
            web_configured,
            subtitles_path: expand_tilde(&doc.bridge.subtitles_path, home.as_deref()),
        })
    }
    pub fn daemon_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.daemon.socket)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessConfig {
    pub pre_bin: PathBuf,
    pub enhance_bin: PathBuf,
    pub rvc_bin: PathBuf,
    pub asr_bin: PathBuf,
    pub tts_bin: PathBuf,
    pub bridge_bin: PathBuf,
    pub llama_bin: PathBuf,
    pub web_bin: PathBuf,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnhanceConfig {
    pub model: PathBuf,
    #[serde(default = "default_enhance_socket")]
    pub socket: String,
    #[serde(default = "default_enhance_audio_socket")]
    pub audio_socket: String,
    #[serde(default = "default_enhance_queue_frames")]
    pub queue_frames: u32,
    #[serde(default = "default_enhance_timeout")]
    pub request_timeout_ms: u32,
}

fn default_enhance_socket() -> String {
    "enhance.sock".to_string()
}
fn default_enhance_audio_socket() -> String {
    "enhance-audio.sock".to_string()
}
fn default_enhance_queue_frames() -> u32 {
    8
}
fn default_enhance_timeout() -> u32 {
    100
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreConfig {
    pub socket: String,
    /// ALSA 采集设备名（SPEC-02：部署配置经 --config 读取）。
    pub capture_device: String,
    /// 测试采集文件（SPEC-02 文件采集源）：设置后替代 ALSA 采集。
    /// 文件契约由 pre 启动时入口校验，配置层只承载路径。
    pub capture_file: Option<PathBuf>,
    /// ALSA 播放设备名。
    pub playback_device: String,
    /// 采集 → DSP 环形队列容量（帧），1..=1024。
    pub capture_queue_frames: u32,
    /// 消费者列表（[[pre.consumers]]）：未设置时回退 rvc+asr 两支路，
    /// socket 沿用 [rvc]/[asr] 的 input_socket（经 Config::pre_consumers 取用）。
    pub consumers: Option<Vec<PreConsumerConfig>>,
    /// DSP 阶段开关（[pre.dsp] 段）。
    pub dsp: PreDspConfig,
}

/// 输入支路消费者声明（[[pre.consumers]]，SPEC-02 分发）：一个消费者一个
/// socket，背压互不影响。socket 为纯文件名，相对 run_dir；对端绑定收帧，
/// pre 连接发送（与 [rvc]/[asr] 的 input_socket 方向约定一致）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreConsumerConfig {
    /// 消费者名：GetStatus consumer_drops 的键，列表内唯一。
    /// "rvc" 额外聚合输出链路丢弃（SPEC-02 口径）；"tts" 与含点名为保留键。
    pub name: String,
    /// 输入 socket 路径（纯文件名）。
    pub socket: String,
    /// 是否受文件接管影响（SetFileMode）：true 时接管期间停发、启动等
    /// daemon 断言（rvc 语义）；false 恒发（asr 语义）。默认 false。
    #[serde(default)]
    pub file_mode_affected: bool,
}

/// DSP 阶段配置。开关默认沿用旧 main.c 的代码默认（生产脚本曾用 --no-denoise
/// 显式关闭、--gate/--agc/--limiter 显式开启）；禁用降噪时整级跳过，
/// 禁用整形级时逐位恒等（SPEC-02 已确认）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreDspConfig {
    /// WOLA 谱降噪开关，默认 true；false 时该级全部计算跳过。
    pub denoise: bool,
    /// 48 kHz 噪声门，默认 false。
    pub gate: bool,
    /// 48 kHz 自动增益，默认 false。
    pub agc: bool,
    /// 48 kHz 峰值限幅，默认 false。
    pub limiter: bool,
}

impl Default for PreDspConfig {
    fn default() -> Self {
        Self {
            denoise: true,
            gate: false,
            agc: false,
            limiter: false,
        }
    }
}

impl Default for PreConfig {
    fn default() -> Self {
        Self {
            socket: "pre.sock".to_string(),
            capture_device: "default".to_string(),
            capture_file: None,
            playback_device: "default".to_string(),
            capture_queue_frames: 8,
            consumers: None,
            dsp: PreDspConfig::default(),
        }
    }
}

/// `[rvc.realtime]` 旋钮的默认值、上限与整除模数：镜像单一来源
/// `tools/model/realtime_chain_params.json`（geometry 段）与 C++ 侧
/// `rvc/include/rvc/realtime_geometry.hpp`。跨语言双定义由
/// `realtime_knobs_mirror_params_json` 测试读 JSON 对拍钉住。
const REALTIME_CROSSFADE_MS_DEFAULT: u32 = 50;
const REALTIME_SOLA_SEARCH_MS_DEFAULT: u32 = 10;
const REALTIME_STARTUP_DELAY_MS_DEFAULT: u32 = 400;
const REALTIME_SKIP_SILENCE_DEFAULT: bool = true;
/// 上限按 48 kHz 契约换算成 ms（crossfade_max_out 2400、sola_search_max_out 480）；
/// 各采样率的 max_out 同比缩放，故 ms 边界与采样率无关。
const REALTIME_CROSSFADE_MS_MAX: u32 = 50;
const REALTIME_SOLA_SEARCH_MS_MAX: u32 = 10;
/// overlap 锁为定值而非范围：引擎资产形状按最大 overlap 固定，小于它会让
/// StreamingRvc 的分析窗帧数与资产不符，推理层每块抛异常。
const REALTIME_OVERLAP_MS_MAX: u32 = 60;
/// 启动缓冲上限：C++ 输出环 16 s @48k，留 1 s 余量。
const REALTIME_STARTUP_DELAY_MS_MAX: u32 = 15000;
/// 整除模数来自 `StreamingRvc` 的几何要求：淡化 + SOLA 须落在模型帧网格
/// （100 Hz → 10 ms），启动缓冲须落在契约帧网格（20 ms 帧 → 20 ms）。
/// 违反会在推理层抛异常，故在配置入口拒绝。
const REALTIME_MODEL_FRAME_RATE: u32 = 100;
const REALTIME_OVERLAP_MS_QUANTUM: u32 = 1000 / REALTIME_MODEL_FRAME_RATE;
const REALTIME_STARTUP_MS_QUANTUM: u32 = 1000 / (1000 / crate::FRAME_MS);

/// `[rvc.realtime]` 非形状类实时旋钮（SPEC-03 实时几何与资产参数节）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RvcRealtimeConfig {
    pub crossfade_ms: u32,
    pub sola_search_ms: u32,
    pub startup_delay_ms: u32,
    pub skip_silence: bool,
}

impl Default for RvcRealtimeConfig {
    fn default() -> Self {
        Self {
            crossfade_ms: REALTIME_CROSSFADE_MS_DEFAULT,
            sola_search_ms: REALTIME_SOLA_SEARCH_MS_DEFAULT,
            startup_delay_ms: REALTIME_STARTUP_DELAY_MS_DEFAULT,
            skip_silence: REALTIME_SKIP_SILENCE_DEFAULT,
        }
    }
}

/// RVC 会话参数五键的值对象（SPEC-11 预设文件、config 校验与 status 共用）。
/// 字段名与 `[rvc]` 同名，范围校验见 `validate_rvc_session_values`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RvcSessionValues {
    pub pitch_shift: i32,
    pub index_rate: f64,
    pub rms_mix_rate: f64,
    pub protect: f64,
    pub f0_method: String,
}

impl Default for RvcSessionValues {
    fn default() -> Self {
        Self {
            pitch_shift: 0,
            index_rate: 0.0,
            rms_mix_rate: 1.0,
            protect: 0.33,
            f0_method: "rmvpe".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RvcConfig {
    pub socket: String,
    /// 数据面：rvc 绑定收输入帧，pre 连接发送。
    pub input_socket: String,
    /// 数据面：pre 绑定收输出帧，rvc 连接发送。
    pub output_socket: String,
    /// 空表示沿用推理进程内已加载的模型；变更路由时不调用 LoadModel。
    pub model_id: String,
    pub f0_method: String,
    pub pitch_shift: i32,
    pub index_rate: f64,
    pub rms_mix_rate: f64,
    pub protect: f64,
    /// 以下进程启动键（SPEC-03 参数边界表）由 C++ 推理进程读取与校验；
    /// daemon 只透传 TOML，这里声明键名以保持共用配置可解析。
    pub models_dir: String,
    pub device: String,
    pub half: bool,
    pub cache_limit: u32,
    pub hubert_path: String,
    pub rmvpe_path: String,
    pub realtime_hubert_path: String,
    pub realtime_rmvpe_path: String,
    #[serde(default)]
    pub realtime: RvcRealtimeConfig,
}

impl Default for RvcConfig {
    fn default() -> Self {
        Self::from_session_values(RvcSessionValues::default())
    }
}

impl RvcConfig {
    /// 用会话参数覆盖五个键，其余键取默认；供默认值构造与预设载入共用。
    fn from_session_values(values: RvcSessionValues) -> Self {
        Self {
            socket: "rvc.sock".to_string(),
            input_socket: "rvc-in.sock".to_string(),
            output_socket: "rvc-out.sock".to_string(),
            model_id: String::new(),
            f0_method: values.f0_method,
            pitch_shift: values.pitch_shift,
            index_rate: values.index_rate,
            rms_mix_rate: values.rms_mix_rate,
            protect: values.protect,
            models_dir: String::new(),
            device: "cuda".to_string(),
            half: false,
            cache_limit: 2,
            hubert_path: String::new(),
            rmvpe_path: String::new(),
            realtime_hubert_path: String::new(),
            realtime_rmvpe_path: String::new(),
            realtime: RvcRealtimeConfig::default(),
        }
    }

    pub fn session_values(&self) -> RvcSessionValues {
        RvcSessionValues {
            pitch_shift: self.pitch_shift,
            index_rate: self.index_rate,
            rms_mix_rate: self.rms_mix_rate,
            protect: self.protect,
            f0_method: self.f0_method.clone(),
        }
    }
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            pre_bin: PathBuf::from("mozart-pre"),
            enhance_bin: PathBuf::from("mozart-enhance"),
            rvc_bin: PathBuf::from("mozart-rvc"),
            asr_bin: PathBuf::from("mozart-asr"),
            tts_bin: PathBuf::from("mozart-tts"),
            bridge_bin: PathBuf::from("mozart-bridge"),
            llama_bin: PathBuf::from("llama-server"),
            web_bin: PathBuf::from("mozart-web-adapter"),
            env: HashMap::new(),
        }
    }
}

/// ASR 进程配置（语言链）：控制面、音频输入与事件输出 socket，模型与队列参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    /// 控制面：asr 绑定，daemon/ctl 连接。
    pub socket: String,
    /// 数据面：asr 绑定收音频 DGRAM，pre 连接发送。
    pub input_socket: String,
    /// 事件面：asr 绑定 SEQPACKET 事件，bridge 连接接收。
    pub events_socket: String,
    /// 流式 zipformer 模型目录；空表示运行时必须显式提供。
    pub model_dir: PathBuf,
    /// SenseVoice 复核模型目录；空表示关闭复核。
    pub review_model_dir: PathBuf,
    /// sherpa-onnx 推理线程数。
    pub threads: u32,
    /// 接收 → 解码环形队列容量（板端实测后定）。
    pub ring_capacity: u32,
    /// 事件输出重连缓冲容量（板端实测后定）。
    pub output_queue_capacity: u32,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            socket: "asr.sock".to_string(),
            input_socket: "asr-in.sock".to_string(),
            events_socket: "asr-events.sock".to_string(),
            model_dir: PathBuf::new(),
            review_model_dir: PathBuf::new(),
            threads: 2,
            ring_capacity: 64,
            output_queue_capacity: 64,
        }
    }
}

/// TTS 进程配置（语言链）：控制面、任务与音频输出 socket，模型与音色参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtsConfig {
    /// 控制面：tts 绑定，daemon/ctl 连接。
    pub socket: String,
    /// 任务面：tts 绑定收合成任务，bridge 连接发送。
    pub jobs_socket: String,
    /// 数据面：48 kHz 播报音频输出 socket（pre 绑定收帧播放，TTS 连接发送；
    /// v1 不走变声，经变声喂 RVC 见 FUTURE）。
    pub output_socket: String,
    /// PocketTTS 模型目录。
    pub model_dir: PathBuf,
    /// 音色参考 WAV 路径。
    pub reference_wav: PathBuf,
    /// sherpa-onnx 推理线程数。
    pub threads: u32,
    pub provider: String,
    /// 任务队列容量（jobs → synth，板端实测后定）。
    pub queue_capacity: u32,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            socket: "tts.sock".to_string(),
            jobs_socket: "tts-jobs.sock".to_string(),
            output_socket: "tts-out.sock".to_string(),
            model_dir: PathBuf::new(),
            reference_wav: PathBuf::new(),
            threads: 2,
            provider: "cpu".to_string(),
            queue_capacity: 8,
        }
    }
}

/// 字幕桥配置（语言链）：控制面、字幕落盘、翻译/播报开关与 llama-server 接入。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BridgeConfig {
    /// 控制面：bridge 绑定，daemon/ctl 连接。
    pub socket: String,
    /// 字幕 JSONL 落盘路径；加载时展开开头的 `~`。
    pub subtitles_path: PathBuf,
    /// 显式同音纠错表文件（JSON {"原文":"改为"}）；缺省用内置表，加载时展开 `~`。
    pub corrections_path: Option<PathBuf>,
    /// 翻译开关的 TOML 基线。
    pub translate_enabled: bool,
    /// 播报开关的 TOML 基线。
    pub speech_enabled: bool,
    /// llama-server HTTP 的 Unix socket。
    pub llama_socket: String,
    /// 翻译模型 GGUF 路径；空表示运行时必须显式提供。
    pub llama_model: PathBuf,
    pub translate_queue_capacity: u32,
    pub speech_queue_capacity: u32,
    /// 单句翻译失败后的重试上限（SPEC-05：最多 2 次）。
    pub retry_max: u32,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            socket: "bridge.sock".to_string(),
            subtitles_path: PathBuf::from("~/.local/state/mozart/subtitles.jsonl"),
            corrections_path: None,
            translate_enabled: true,
            speech_enabled: true,
            llama_socket: "llama.sock".to_string(),
            llama_model: PathBuf::new(),
            translate_queue_capacity: 32,
            speech_queue_capacity: 16,
            retry_max: 2,
        }
    }
}

/// 文件转换链路配置（[file]，SPEC-04）：队列、受管存储与 ffmpeg 解码。
/// 数据面复用 [rvc] 的 input_socket/output_socket（SPEC-03 状态门控交接），
/// 本段不再有独立 socket 键。v1 仅启动读取；键分类见 config.md
/// （storage_dir 为 S，其余 R）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    /// 受管目录（源副本、结果、队列快照）；加载时展开开头的 `~`，
    /// 经 `Config::file_storage_dir` 取用。
    pub storage_dir: PathBuf,
    /// 入队时查的逻辑上限（未预分配）。
    pub queue_capacity: u32,
    /// 受管目录总大小上限，超出按时间戳驱逐已完成任务。
    pub storage_limit_bytes: u64,
    /// 解码后是否经 DSP 降噪（48 kHz 路径）。
    pub denoise: bool,
    /// 单任务时长上限。
    pub max_duration_seconds: u64,
    /// 源文件上传（复制入受管目录）大小上限。
    pub max_upload_bytes: u64,
    /// ffmpeg 可执行文件（裸名走 PATH，或绝对路径）。
    pub ffmpeg_path: String,
    /// 单次 ffmpeg 调用超时，超时杀进程。
    pub ffmpeg_timeout_ms: u64,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            storage_dir: PathBuf::from("~/.local/state/mozart/files/"),
            queue_capacity: 50,
            storage_limit_bytes: 1_048_576_000,
            denoise: false,
            max_duration_seconds: 300,
            max_upload_bytes: 104_857_600,
            ffmpeg_path: "ffmpeg".to_string(),
            ffmpeg_timeout_ms: 120_000,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            web: None,
            process: ProcessConfig::default(),
            pre: PreConfig::default(),
            enhance: None,
            rvc: RvcConfig::default(),
            asr: AsrConfig::default(),
            tts: TtsConfig::default(),
            bridge: BridgeConfig::default(),
            file: FileConfig::default(),
            models: Vec::new(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::from("/run/mozart"),
            startup_mode: StartupMode::default(),
            socket: "daemon.sock".to_string(),
            poll_interval_ms: 1000,
            rpc_timeout_ms: 2000,
            start_timeout_ms: 5000,
            restart_backoff_ms: 500,
            restart_backoff_max_ms: 10_000,
            restart_stable_ms: 30_000,
        }
    }
}

impl DaemonConfig {
    fn apply_env(&mut self) -> Result<(), ConfigError> {
        if let Ok(mode) = std::env::var("MOZART_STARTUP_MODE") {
            self.startup_mode = serde_json::from_value(serde_json::Value::String(mode))
                .map_err(|err| ConfigError::Invalid(format!("MOZART_STARTUP_MODE: {err}")))?;
        }
        if let Some(dir) = std::env::var_os("MOZART_RUN_DIR") {
            self.run_dir = dir.into();
        }
        if let Ok(value) = std::env::var("MOZART_POLL_INTERVAL_MS") {
            self.poll_interval_ms = value.parse().map_err(|_| {
                ConfigError::Invalid("MOZART_POLL_INTERVAL_MS must be an integer".into())
            })?;
        }
        Ok(())
    }
    fn validate(&self) -> Result<(), ConfigError> {
        for (name, value) in [
            ("poll_interval_ms", self.poll_interval_ms),
            ("rpc_timeout_ms", self.rpc_timeout_ms),
            ("start_timeout_ms", self.start_timeout_ms),
            ("restart_stable_ms", self.restart_stable_ms),
        ] {
            if value == 0 {
                return Err(ConfigError::Invalid(format!(
                    "daemon.{name} must be greater than zero"
                )));
            }
        }
        if self.restart_backoff_ms == 0 || self.restart_backoff_max_ms < self.restart_backoff_ms {
            return Err(ConfigError::Invalid(
                "daemon.restart_backoff_max_ms must be at least restart_backoff_ms".into(),
            ));
        }
        validate_socket_name(&self.socket, "daemon.socket")
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "read config {}: {source}", path.display())
            }
            ConfigError::Write { path, source } => {
                write!(f, "write config {}: {source}", path.display())
            }
            ConfigError::Parse { path, source } => {
                write!(f, "parse config {}: {source}", path.display())
            }
            ConfigError::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// 解析顺序：显式路径 > `MOZART_CONFIG` > `./mozart.toml` > 内置默认。
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        Self::load_from(Self::resolve_path(explicit).as_deref())
    }

    pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
        explicit
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("MOZART_CONFIG").map(PathBuf::from))
            .or_else(|| {
                let local = PathBuf::from("mozart.toml");
                local.exists().then_some(local)
            })
    }

    pub fn load_from(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut config = match path {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
                toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?
            }
            None => Config::default(),
        };
        config.apply_env()?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        config.bridge.subtitles_path = expand_tilde(&config.bridge.subtitles_path, home.as_deref());
        if let Some(path) = &config.bridge.corrections_path {
            config.bridge.corrections_path = Some(expand_tilde(path, home.as_deref()));
        }
        config.file.storage_dir = expand_tilde(&config.file.storage_dir, home.as_deref());
        if let Some(web) = &mut config.web {
            web.prepare(path, home.as_deref())?;
        }
        config.validate()?;
        if let Some(enhance) = &mut config.enhance {
            if enhance.model.is_relative() {
                let base = path
                    .and_then(Path::parent)
                    .unwrap_or_else(|| Path::new("."));
                enhance.model = base.join(&enhance.model);
            }
        }
        Ok(config)
    }

    pub fn daemon_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.daemon.socket)
    }

    pub fn enhance_socket(&self) -> PathBuf {
        self.daemon
            .run_dir
            .join(&self.enhance.as_ref().expect("enhance configured").socket)
    }

    pub fn enhance_audio_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(
            &self
                .enhance
                .as_ref()
                .expect("enhance configured")
                .audio_socket,
        )
    }

    pub fn pre_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.pre.socket)
    }

    /// 生效消费者列表（SPEC-02 分发）：显式 [[pre.consumers]] 优先；未设置
    /// 回退 rvc+asr 两支路，socket 沿用 [rvc]/[asr] 的 input_socket，
    /// 文件接管语义 rvc 受影响、asr 不受影响（向后兼容默认）。
    pub fn pre_consumers(&self) -> Vec<PreConsumerConfig> {
        if let Some(consumers) = &self.pre.consumers {
            return consumers.clone();
        }
        vec![
            PreConsumerConfig {
                name: "rvc".to_string(),
                socket: self.rvc.input_socket.clone(),
                file_mode_affected: true,
            },
            PreConsumerConfig {
                name: "asr".to_string(),
                socket: self.asr.input_socket.clone(),
                file_mode_affected: false,
            },
        ]
    }

    pub fn rvc_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.rvc.socket)
    }

    pub fn rvc_input_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.rvc.input_socket)
    }

    pub fn rvc_output_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.rvc.output_socket)
    }

    pub fn asr_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.asr.socket)
    }

    pub fn asr_input_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.asr.input_socket)
    }

    pub fn asr_events_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.asr.events_socket)
    }

    pub fn tts_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.tts.socket)
    }

    pub fn tts_jobs_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.tts.jobs_socket)
    }

    pub fn tts_output_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.tts.output_socket)
    }

    pub fn bridge_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.bridge.socket)
    }

    pub fn llama_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.bridge.llama_socket)
    }

    /// 受管文件目录；`load_from` 已展开开头的 `~`（同 subtitles_path）。
    pub fn file_storage_dir(&self) -> PathBuf {
        self.file.storage_dir.clone()
    }

    pub fn log_dir(&self) -> PathBuf {
        self.daemon.run_dir.join("logs")
    }

    fn apply_env(&mut self) -> Result<(), ConfigError> {
        self.daemon.apply_env()?;
        for (var, slot) in [
            ("MOZART_ENHANCE_BIN", &mut self.process.enhance_bin),
            ("MOZART_PRE_BIN", &mut self.process.pre_bin),
            ("MOZART_RVC_BIN", &mut self.process.rvc_bin),
            ("MOZART_ASR_BIN", &mut self.process.asr_bin),
            ("MOZART_TTS_BIN", &mut self.process.tts_bin),
            ("MOZART_BRIDGE_BIN", &mut self.process.bridge_bin),
            ("MOZART_LLAMA_BIN", &mut self.process.llama_bin),
            ("MOZART_WEB_BIN", &mut self.process.web_bin),
        ] {
            if let Some(bin) = std::env::var_os(var) {
                *slot = PathBuf::from(bin);
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.daemon.validate()?;
        if let Some(web) = &self.web {
            web.validate()?;
        }
        validate_socket_name(&self.pre.socket, "pre.socket")?;
        validate_device_name(&self.pre.capture_device, "pre.capture_device")?;
        validate_device_name(&self.pre.playback_device, "pre.playback_device")?;
        if self.pre.capture_queue_frames == 0 || self.pre.capture_queue_frames > 1024 {
            return Err(ConfigError::Invalid(
                "pre.capture_queue_frames must be within 1..=1024".to_string(),
            ));
        }
        if let Some(consumers) = &self.pre.consumers {
            let mut names = std::collections::HashSet::new();
            let mut sockets = std::collections::HashSet::new();
            for consumer in consumers {
                if consumer.name.is_empty() {
                    return Err(ConfigError::Invalid(
                        "pre.consumers[].name must not be empty".to_string(),
                    ));
                }
                // "tts" 是播报链路固定键，点名留给 rvc.skipped 这类子键。
                if consumer.name == "tts" || consumer.name.contains('.') {
                    return Err(ConfigError::Invalid(format!(
                        "pre.consumers name '{}' is reserved (tts or dotted sub-key)",
                        consumer.name
                    )));
                }
                validate_socket_name(&consumer.socket, "pre.consumers[].socket")?;
                if !names.insert(&consumer.name) {
                    return Err(ConfigError::Invalid(format!(
                        "pre.consumers duplicate name '{}'",
                        consumer.name
                    )));
                }
                if !sockets.insert(&consumer.socket) {
                    return Err(ConfigError::Invalid(format!(
                        "pre.consumers duplicate socket '{}'",
                        consumer.socket
                    )));
                }
            }
        }
        validate_socket_name(&self.rvc.socket, "rvc.socket")?;
        validate_socket_name(&self.rvc.input_socket, "rvc.input_socket")?;
        validate_socket_name(&self.rvc.output_socket, "rvc.output_socket")?;
        validate_rvc_session_values(&self.rvc.session_values())?;
        let realtime = &self.rvc.realtime;
        if realtime.crossfade_ms > REALTIME_CROSSFADE_MS_MAX {
            return Err(ConfigError::Invalid(format!(
                "rvc.realtime.crossfade_ms must be <= {REALTIME_CROSSFADE_MS_MAX}"
            )));
        }
        if realtime.sola_search_ms > REALTIME_SOLA_SEARCH_MS_MAX {
            return Err(ConfigError::Invalid(format!(
                "rvc.realtime.sola_search_ms must be <= {REALTIME_SOLA_SEARCH_MS_MAX}"
            )));
        }
        let overlap_ms = realtime.crossfade_ms + realtime.sola_search_ms;
        if overlap_ms != REALTIME_OVERLAP_MS_MAX {
            return Err(ConfigError::Invalid(format!(
                "rvc.realtime.crossfade_ms + sola_search_ms must equal \
                 {REALTIME_OVERLAP_MS_MAX} ms: engine asset shapes are fixed at the \
                 maximum overlap; adjustable overlap is not implemented yet"
            )));
        }
        // overlap 锁为定值后天然落在模型帧网格上，不再重复校验整除；
        // 解耦分析窗与淡化量、放开范围时须恢复该约束（见 SPEC-03 改进点）。
        if realtime.startup_delay_ms > REALTIME_STARTUP_DELAY_MS_MAX {
            return Err(ConfigError::Invalid(format!(
                "rvc.realtime.startup_delay_ms must be <= {REALTIME_STARTUP_DELAY_MS_MAX}"
            )));
        }
        if realtime.startup_delay_ms % REALTIME_STARTUP_MS_QUANTUM != 0 {
            return Err(ConfigError::Invalid(format!(
                "rvc.realtime.startup_delay_ms must be a multiple of \
                 {REALTIME_STARTUP_MS_QUANTUM} ms (output frame grid)"
            )));
        }
        validate_socket_name(&self.asr.socket, "asr.socket")?;
        validate_socket_name(&self.asr.input_socket, "asr.input_socket")?;
        validate_socket_name(&self.asr.events_socket, "asr.events_socket")?;
        if self.asr.threads == 0 {
            return Err(ConfigError::Invalid(
                "asr.threads must be at least 1".to_string(),
            ));
        }
        if self.asr.ring_capacity == 0 {
            return Err(ConfigError::Invalid(
                "asr.ring_capacity must be at least 1".to_string(),
            ));
        }
        if self.asr.output_queue_capacity == 0 {
            return Err(ConfigError::Invalid(
                "asr.output_queue_capacity must be at least 1".to_string(),
            ));
        }
        validate_socket_name(&self.tts.socket, "tts.socket")?;
        validate_socket_name(&self.tts.jobs_socket, "tts.jobs_socket")?;
        validate_socket_name(&self.tts.output_socket, "tts.output_socket")?;
        if self.tts.threads == 0 {
            return Err(ConfigError::Invalid(
                "tts.threads must be at least 1".to_string(),
            ));
        }
        if self.tts.provider.is_empty() {
            return Err(ConfigError::Invalid(
                "tts.provider must not be empty".to_string(),
            ));
        }
        if self.tts.queue_capacity == 0 {
            return Err(ConfigError::Invalid(
                "tts.queue_capacity must be at least 1".to_string(),
            ));
        }
        validate_socket_name(&self.bridge.socket, "bridge.socket")?;
        validate_socket_name(&self.bridge.llama_socket, "bridge.llama_socket")?;
        if self.bridge.translate_queue_capacity == 0 {
            return Err(ConfigError::Invalid(
                "bridge.translate_queue_capacity must be at least 1".to_string(),
            ));
        }
        if self.bridge.speech_queue_capacity == 0 {
            return Err(ConfigError::Invalid(
                "bridge.speech_queue_capacity must be at least 1".to_string(),
            ));
        }
        if self.bridge.retry_max > 2 {
            return Err(ConfigError::Invalid(
                "bridge.retry_max must be at most 2".to_string(),
            ));
        }
        if self.file.queue_capacity == 0 {
            return Err(ConfigError::Invalid(
                "file.queue_capacity must be at least 1".to_string(),
            ));
        }
        if self.file.storage_limit_bytes == 0 {
            return Err(ConfigError::Invalid(
                "file.storage_limit_bytes must be greater than zero".to_string(),
            ));
        }
        if self.file.max_duration_seconds == 0 {
            return Err(ConfigError::Invalid(
                "file.max_duration_seconds must be at least 1".to_string(),
            ));
        }
        if self.file.max_upload_bytes == 0 {
            return Err(ConfigError::Invalid(
                "file.max_upload_bytes must be greater than zero".to_string(),
            ));
        }
        if self.file.ffmpeg_timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "file.ffmpeg_timeout_ms must be greater than zero".to_string(),
            ));
        }
        for (bin, field) in [
            (&self.process.pre_bin, "process.pre_bin"),
            (&self.process.rvc_bin, "process.rvc_bin"),
            (&self.process.asr_bin, "process.asr_bin"),
            (&self.process.tts_bin, "process.tts_bin"),
            (&self.process.bridge_bin, "process.bridge_bin"),
            (&self.process.llama_bin, "process.llama_bin"),
            (&self.process.enhance_bin, "process.enhance_bin"),
            (&self.process.web_bin, "process.web_bin"),
        ] {
            if bin.as_os_str().is_empty() {
                return Err(ConfigError::Invalid(format!("{field} must not be empty")));
            }
        }
        self.validate_enhance(self.enhance.as_ref())?;
        let mut seen = std::collections::HashSet::new();
        for entry in &self.models {
            if entry.id.is_empty() {
                return Err(ConfigError::Invalid(
                    "[[models]].id must not be empty".to_string(),
                ));
            }
            match entry.model_type.as_str() {
                "rvc" | "asr" | "tts" | "llm" => {}
                other => {
                    return Err(ConfigError::Invalid(format!(
                        "[[models]].type must be rvc/asr/tts/llm, got '{other}'"
                    )));
                }
            }
            if entry.path.as_os_str().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "[[models]] id='{}': path must not be empty",
                    entry.id
                )));
            }
            if !seen.insert((&entry.id, &entry.model_type)) {
                return Err(ConfigError::Invalid(format!(
                    "[[models]] duplicate id='{}' type='{}'",
                    entry.id, entry.model_type
                )));
            }
        }
        Ok(())
    }

    /// Apply the supervisor's effective backend snapshot from external JSON.
    /// Validate only enhancement settings against the already loaded configuration.
    pub fn apply_effective_enhance(
        &mut self,
        enhance: Option<EnhanceConfig>,
    ) -> Result<(), ConfigError> {
        self.validate_enhance(enhance.as_ref())?;
        self.enhance = enhance;
        Ok(())
    }

    fn validate_enhance(&self, enhance: Option<&EnhanceConfig>) -> Result<(), ConfigError> {
        if let Some(enhance) = enhance {
            if enhance.model.as_os_str().is_empty() {
                return Err(ConfigError::Invalid(
                    "enhance.model must not be empty".to_string(),
                ));
            }
            if !(1..=1024).contains(&enhance.queue_frames) {
                return Err(ConfigError::Invalid(
                    "enhance.queue_frames must be within 1..=1024".to_string(),
                ));
            }
            if !(1..=1000).contains(&enhance.request_timeout_ms) {
                return Err(ConfigError::Invalid(
                    "enhance.request_timeout_ms must be within 1..=1000".to_string(),
                ));
            }
            let mut sockets: std::collections::HashSet<&str> = [
                self.daemon.socket.as_str(),
                self.pre.socket.as_str(),
                self.rvc.socket.as_str(),
                self.rvc.input_socket.as_str(),
                self.rvc.output_socket.as_str(),
                self.asr.socket.as_str(),
                self.asr.input_socket.as_str(),
                self.asr.events_socket.as_str(),
                self.tts.socket.as_str(),
                self.tts.jobs_socket.as_str(),
                self.tts.output_socket.as_str(),
                self.bridge.socket.as_str(),
                self.bridge.llama_socket.as_str(),
            ]
            .into_iter()
            .collect();
            if let Some(consumers) = &self.pre.consumers {
                sockets.extend(consumers.iter().map(|consumer| consumer.socket.as_str()));
            }
            for (name, field) in [
                (&enhance.socket, "enhance.socket"),
                (&enhance.audio_socket, "enhance.audio_socket"),
            ] {
                validate_socket_name(name, field)?;
                if !sockets.insert(name.as_str()) {
                    return Err(ConfigError::Invalid(format!(
                        "{field} conflicts with another socket"
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn model_path(&self, id: &str, model_type: &str) -> Option<&Path> {
        self.models
            .iter()
            .find(|m| m.id == id && m.model_type == model_type)
            .map(|m| m.path.as_path())
    }

    /// RVC 模型目录（预设文件定位）：注册表 path 优先，空时回退
    /// models_dir/<id>；回退只接受纯目录名，拒绝路径穿越。
    pub fn rvc_model_dir(&self, id: &str) -> Option<PathBuf> {
        if let Some(path) = self.model_path(id, "rvc") {
            return Some(path.to_path_buf());
        }
        if self.rvc.models_dir.is_empty() || !is_plain_component(id) {
            return None;
        }
        Some(Path::new(&self.rvc.models_dir).join(id))
    }
}

/// 会话参数五键的范围校验（单一来源）：Config::validate 与预设解析共用。
pub fn validate_rvc_session_values(values: &RvcSessionValues) -> Result<(), ConfigError> {
    if values.f0_method != "rmvpe" {
        return Err(ConfigError::Invalid(
            "rvc.f0_method must be \"rmvpe\"".to_string(),
        ));
    }
    if !(-12..=12).contains(&values.pitch_shift) {
        return Err(ConfigError::Invalid(
            "rvc.pitch_shift must be within -12..=12".to_string(),
        ));
    }
    if !values.index_rate.is_finite() || !(0.0..=1.0).contains(&values.index_rate) {
        return Err(ConfigError::Invalid(
            "rvc.index_rate must be within 0.0..=1.0".to_string(),
        ));
    }
    if !values.rms_mix_rate.is_finite() || !(0.0..=1.0).contains(&values.rms_mix_rate) {
        return Err(ConfigError::Invalid(
            "rvc.rms_mix_rate must be within 0.0..=1.0".to_string(),
        ));
    }
    if !values.protect.is_finite() || !(0.0..=0.5).contains(&values.protect) {
        return Err(ConfigError::Invalid(
            "rvc.protect must be within 0.0..=0.5".to_string(),
        ));
    }
    Ok(())
}

/// 预设载入：一次原子写五个会话参数键到 TOML 基线，保留其余键与注释。
pub fn write_rvc_session_baseline(
    path: &Path,
    values: &RvcSessionValues,
) -> Result<(), ConfigError> {
    let mut doc = load_doc(path)?;
    for (key, item) in [
        (
            "rvc.pitch_shift",
            toml_edit::value(i64::from(values.pitch_shift)),
        ),
        ("rvc.index_rate", toml_edit::value(values.index_rate)),
        ("rvc.rms_mix_rate", toml_edit::value(values.rms_mix_rate)),
        ("rvc.protect", toml_edit::value(values.protect)),
        ("rvc.f0_method", toml_edit::value(values.f0_method.clone())),
    ] {
        set_doc_key(&mut doc, key, item)?;
    }
    validate_doc(path, &doc)?;
    atomic_write(path, &doc.to_string())
}

/// 将 `model use` 切换后的模型 ID 原子写回 TOML `[rvc].model_id` 基线，
/// 保留注释与其他键；供 daemon 重启后延续上次选用的模型。
pub fn write_rvc_model_baseline(path: &Path, model_id: &str) -> Result<(), ConfigError> {
    let mut doc = load_doc(path)?;
    set_doc_key(&mut doc, "rvc.model_id", toml_edit::value(model_id))?;
    validate_doc(path, &doc)?;
    atomic_write(path, &doc.to_string())
}

/// 纯单段目录名（模型 id 回退拼路径用）：在 socket 名规则上再加 NUL 拦截。
fn is_plain_component(name: &str) -> bool {
    is_plain_name(name) && !name.contains('\0')
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

fn validate_socket_name(name: &str, field: &str) -> Result<(), ConfigError> {
    if !is_plain_name(name) {
        return Err(ConfigError::Invalid(format!(
            "{field} must be a plain file name"
        )));
    }
    Ok(())
}

/// 展开路径开头的 `~` 组件；home 缺失或路径不以 `~` 开头时原样返回。
fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) if rest.as_os_str().is_empty() => home.to_path_buf(),
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// ALSA 设备名不做格式限制；只拦空名与内嵌 NUL（CString 转换会 panic）。
pub fn validate_device_name(name: &str, field: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::Invalid(format!("{field} must not be empty")));
    }
    if name.contains('\0') {
        return Err(ConfigError::Invalid(format!(
            "{field} must not contain NUL bytes"
        )));
    }
    Ok(())
}

/// 运行目录承载控制 socket，统一收敛到 0700；ctl、daemon 与 pre 共用此定义，
/// 避免 ctl 抢先建目录导致权限策略失效。
pub fn ensure_run_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// 日志目录与运行目录同口径收敛到 0700，避免默认 umask 0002 生成 0775 与 Web 启动校验冲突。
pub fn ensure_log_dir(dir: &Path) -> io::Result<()> {
    ensure_run_dir(dir)
}

/// 设备键 → 方向（SPEC-04 设备存在性校验用）；非设备键返回 None。
/// 设备键分类的唯一定义源，daemon 与 set_key 共用。
pub fn device_key_kind(key: &str) -> Option<&'static str> {
    match key {
        "pre.capture_device" => Some("capture"),
        "pre.playback_device" => Some("playback"),
        _ => None,
    }
}

/// 运行时键 → owning 服务（docs/config.md 的 R 类分类）。None 表示非运行时键。
/// daemon/file 键由 daemon 自身即时应用；rvc 会话参数与实时旋钮由 daemon 按变更键
/// 编排 `SetParams`、`StopStream`→`StartStream` 或只更新生效配置；pre/bridge 键
/// 需对端运行时命令下发（未接入）。
pub fn runtime_key_owner(key: &str) -> Option<&'static str> {
    match key {
        "daemon.poll_interval_ms"
        | "daemon.rpc_timeout_ms"
        | "daemon.start_timeout_ms"
        | "daemon.restart_backoff_ms"
        | "daemon.restart_backoff_max_ms"
        | "daemon.restart_stable_ms"
        | "file.queue_capacity"
        | "file.storage_limit_bytes"
        | "file.denoise"
        | "file.max_duration_seconds"
        | "file.max_upload_bytes"
        | "file.ffmpeg_path"
        | "file.ffmpeg_timeout_ms" => Some("daemon"),
        "pre.dsp.denoise" | "pre.dsp.gate" | "pre.dsp.agc" | "pre.dsp.limiter" => Some("pre"),
        "rvc.f0_method"
        | "rvc.pitch_shift"
        | "rvc.index_rate"
        | "rvc.rms_mix_rate"
        | "rvc.protect"
        | "rvc.realtime.crossfade_ms"
        | "rvc.realtime.sola_search_ms"
        | "rvc.realtime.startup_delay_ms"
        | "rvc.realtime.skip_silence" => Some("rvc"),
        "bridge.translate_enabled" | "bridge.speech_enabled" => Some("bridge"),
        _ => None,
    }
}

/// 运行时状态文件可记录的键（SPEC-04，daemon 统一管）。
/// 当前只有字幕开关；RVC 会话参数与实时旋钮由 config set / preset use 直接写基线并下发。
pub fn is_state_key(key: &str) -> bool {
    matches!(key, "bridge.translate_enabled" | "bridge.speech_enabled")
}

/// 运行时状态文件路径（SPEC-04 持久数据目录）：
/// `MOZART_STATE_DIR` > `~/.local/state/mozart`，文件名 `state.json`。
/// HOME 缺失时回退相对目录 `.local/state/mozart`。
pub fn state_file_path() -> PathBuf {
    let dir = std::env::var_os("MOZART_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local/state/mozart")
        });
    dir.join("state.json")
}

/// config set 的结果（SPEC-04 重启提示契约：restart_required + affected）。
pub struct KeyEffect {
    pub restart_required: bool,
    pub affected: Vec<String>,
}

/// 写单个键的持久基线（SPEC-04 模型甲，原子写）。支持重启类设备键、
/// daemon/file、pre DSP 与 rvc 会话参数键；bridge 运行时键尚未接入。
/// 设备存在性校验由 daemon 在调用前做（需 ALSA 枚举）。
///
/// rvc 会话参数只写 TOML 基线，返回 `restart_required=false`；实时/文件链路的
/// 生效编排（按变更键下发 SetParams 或 StopStream→StartStream）归 daemon，见 SPEC-04。
pub fn set_key(path: Option<&Path>, key: &str, value: &str) -> Result<KeyEffect, ConfigError> {
    if matches!(
        key,
        "enhance.model"
            | "enhance.socket"
            | "enhance.audio_socket"
            | "enhance.queue_frames"
            | "enhance.request_timeout_ms"
            | "process.enhance_bin"
    ) {
        let typed = match key {
            "enhance.queue_frames" | "enhance.request_timeout_ms" => {
                parse_runtime_value(key, value)?
            }
            _ => toml_edit::value(value),
        };
        write_runtime_key(path, key, typed)?;
        return Ok(KeyEffect {
            restart_required: true,
            affected: vec!["enhance".to_string(), "pre".to_string()],
        });
    }
    if device_key_kind(key).is_some() {
        validate_device_name(value, key)?;
        write_toml_key(path, key, toml_edit::value(value))?;
        return Ok(KeyEffect {
            restart_required: true,
            affected: vec!["pre".to_string()],
        });
    }
    if let Some(owner) = runtime_key_owner(key) {
        if !matches!(owner, "daemon" | "rvc" | "pre") {
            return Err(ConfigError::Invalid(format!(
                "config set 暂不支持键 {key}（{owner} 的运行时下发命令未接入）"
            )));
        }
        let typed = parse_runtime_value(key, value)?;
        write_runtime_key(path, key, typed)?;
        return Ok(KeyEffect {
            restart_required: false,
            affected: Vec::new(),
        });
    }
    Err(ConfigError::Invalid(format!(
        "config set 暂不支持键 {key}（当前支持设备键、daemon/file 运行时键、pre DSP 与 rvc 会话参数键）"
    )))
}

/// daemon/file、pre DSP 与 rvc 会话参数键的字符串值 → 带类型 TOML 值。
/// 这里只做类型转换：DSP 开关与 file.denoise 为 bool；file.ffmpeg_path、rvc.f0_method 为
/// string；rvc.index_rate/rms_mix_rate/protect 为 float；其余（含 rvc.pitch_shift）
/// 为整数。取值范围与 f0_method 白名单只在 `Config::validate()` 定义，由
/// write_runtime_key 写入前的整体校验拒绝非法值，不落盘。
fn parse_runtime_value(key: &str, value: &str) -> Result<toml_edit::Item, ConfigError> {
    match key {
        "file.denoise"
        | "rvc.realtime.skip_silence"
        | "pre.dsp.denoise"
        | "pre.dsp.gate"
        | "pre.dsp.agc"
        | "pre.dsp.limiter" => match value {
            "true" => Ok(toml_edit::value(true)),
            "false" => Ok(toml_edit::value(false)),
            _ => Err(ConfigError::Invalid(format!("键 {key} 需要 true/false"))),
        },
        "file.ffmpeg_path" | "rvc.f0_method" => Ok(toml_edit::value(value)),
        "rvc.index_rate" | "rvc.rms_mix_rate" | "rvc.protect" => {
            let number: f64 = value
                .parse()
                .map_err(|_| ConfigError::Invalid(format!("键 {key} 需要浮点")))?;
            Ok(toml_edit::value(number))
        }
        _ => {
            let number: i64 = value
                .parse()
                .map_err(|_| ConfigError::Invalid(format!("键 {key} 需要整数")))?;
            Ok(toml_edit::value(number))
        }
    }
}

/// config save（SPEC-04）：状态文件覆盖批量写进 TOML 基线（原子写，保留注释）。
/// 返回写入键数；空覆盖不动 TOML。写入前整体校验，失败不写（状态文件由调用方保留）。
pub fn save_overrides(
    path: Option<&Path>,
    overrides: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<usize, ConfigError> {
    if overrides.is_empty() {
        return Ok(0);
    }
    let path = require_path(path)?;
    let mut doc = load_doc(path)?;
    for (key, value) in overrides {
        set_doc_key(&mut doc, key, json_to_toml(value)?)?;
    }
    validate_doc(path, &doc)?;
    atomic_write(path, &doc.to_string())?;
    Ok(overrides.len())
}

/// 状态文件标量值 → TOML 值。null/数组/对象不是配置值，拒绝。
fn json_to_toml(value: &serde_json::Value) -> Result<toml_edit::Item, ConfigError> {
    use serde_json::Value;
    match value {
        Value::Bool(flag) => Ok(toml_edit::value(*flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Ok(toml_edit::value(int))
            } else if let Some(float) = number.as_f64() {
                Ok(toml_edit::value(float))
            } else {
                Err(ConfigError::Invalid("状态文件数值超出范围".to_string()))
            }
        }
        Value::String(text) => Ok(toml_edit::value(text.clone())),
        _ => Err(ConfigError::Invalid(
            "状态文件值必须是标量（bool/整数/浮点/字符串）".to_string(),
        )),
    }
}

/// 写回需要配置文件基线；内置默认启动（无文件）时明确拒绝。
fn require_path(path: Option<&Path>) -> Result<&Path, ConfigError> {
    path.ok_or_else(|| {
        ConfigError::Invalid(
            "配置写回需要配置文件基线；daemon 当前以内置默认启动（无配置文件），请以配置文件重启 daemon"
                .to_string(),
        )
    })
}

/// 读 TOML 为可编辑文档；文件缺失按空文档（写回时新建）。
fn load_doc(path: &Path) -> Result<toml_edit::DocumentMut, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|err| ConfigError::Invalid(format!("parse config {}: {err}", path.display())))
}

/// 在文档里按点分键路径设值，保留注释与排版；中间段不是表则拒绝。
fn set_doc_key(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: toml_edit::Item,
) -> Result<(), ConfigError> {
    let mut parts: Vec<&str> = key.split('.').collect();
    let leaf = parts.pop().expect("键非空（调用方已分类）");
    let mut table = doc.as_table_mut();
    for part in parts {
        let item = table
            .entry(part)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        table = item
            .as_table_mut()
            .ok_or_else(|| ConfigError::Invalid(format!("键 {key} 的路径 {part} 不是表")))?;
    }
    table[leaf] = value;
    Ok(())
}

/// 整体校验编辑后的文档：复用 Config 的单一校验源（含跨键约束）。
fn validate_doc(path: &Path, doc: &toml_edit::DocumentMut) -> Result<(), ConfigError> {
    let config: Config = toml::from_str(&doc.to_string()).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    config.validate()
}

/// 原子写 TOML 单键（设备键）：toml_edit 保留注释与排版，临时文件 + rename。
fn write_toml_key(
    path: Option<&Path>,
    key: &str,
    value: toml_edit::Item,
) -> Result<(), ConfigError> {
    let path = require_path(path)?;
    let mut doc = load_doc(path)?;
    set_doc_key(&mut doc, key, value)?;
    atomic_write(path, &doc.to_string())
}

/// 原子写运行时键：写前整体校验，非法值不落盘。
fn write_runtime_key(
    path: Option<&Path>,
    key: &str,
    value: toml_edit::Item,
) -> Result<(), ConfigError> {
    let path = require_path(path)?;
    let mut doc = load_doc(path)?;
    set_doc_key(&mut doc, key, value)?;
    validate_doc(path, &doc)?;
    atomic_write(path, &doc.to_string())
}

/// 临时文件名含进程号与序号，避免并发写回撞名；rename 原子替换。
/// 配置基线与运行时状态文件共用此原子写。
pub fn atomic_write(path: &Path, text: &str) -> Result<(), ConfigError> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".mozart-config-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)
        .map_err(|source| ConfigError::Write { path: tmp.clone(), source })?;
    if let Err(source) = file.write_all(text.as_bytes()).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigError::Write { path: tmp, source });
    }
    drop(file);
    if let Err(source) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigError::Write { path: path.to_path_buf(), source });
    }
    Ok(())
}

/// 读持久基线的键值（config get）。缺省返回整份 TOML；无配置文件返回空表。
/// 读的是 TOML 基线，不含环境变量覆盖与运行时状态（生效值恢复见 SPEC-04）。
pub fn get_key(path: Option<&Path>, key: Option<&str>) -> Result<toml::Value, ConfigError> {
    let value = match path {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            toml::from_str::<toml::Value>(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?
        }
        None => toml::Value::Table(toml::map::Map::new()),
    };
    let Some(key) = key else {
        return Ok(value);
    };
    let mut cur = &value;
    for part in key.split('.') {
        cur = cur
            .get(part)
            .ok_or_else(|| ConfigError::Invalid(format!("键 {key} 不存在")))?;
    }
    Ok(cur.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mozart-cfg-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn enhance_optional_defaults_paths_and_restart_keys() {
        let dir = temp_config_dir("enhance");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "").unwrap();
        assert!(Config::load_from(Some(&path)).unwrap().enhance.is_none());
        assert_eq!(
            Config::default().process.enhance_bin,
            PathBuf::from("mozart-enhance")
        );
        let effect = set_key(Some(&path), "enhance.model", "models/test.onnx").unwrap();
        assert!(effect.restart_required);
        assert_eq!(effect.affected, ["enhance", "pre"]);
        let config = Config::load_from(Some(&path)).unwrap();
        let enhance = config.enhance.as_ref().unwrap();
        assert_eq!(enhance.model, dir.join("models/test.onnx"));
        assert_eq!(enhance.queue_frames, 8);
        assert_eq!(enhance.request_timeout_ms, 100);
        assert_eq!(
            config.enhance_socket(),
            config.daemon.run_dir.join("enhance.sock")
        );
        assert_eq!(
            config.enhance_audio_socket(),
            config.daemon.run_dir.join("enhance-audio.sock")
        );
        set_key(Some(&path), "enhance.model", "/external/model.onnx").unwrap();
        assert_eq!(
            Config::load_from(Some(&path))
                .unwrap()
                .enhance
                .unwrap()
                .model,
            PathBuf::from("/external/model.onnx")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn effective_enhance_validates_only_snapshot_and_preserves_on_failure() {
        let mut config = Config::default();
        // Unrelated config is already checked by the file loader, not this JSON boundary.
        config.daemon.poll_interval_ms = 0;
        let valid: EnhanceConfig = serde_json::from_str(r#"{"model":"/model.onnx"}"#).unwrap();
        config.apply_effective_enhance(Some(valid.clone())).unwrap();
        for (field, value) in [
            ("model", serde_json::json!("")),
            ("queue_frames", serde_json::json!(0)),
            ("request_timeout_ms", serde_json::json!(1001)),
            ("socket", serde_json::json!("../bad")),
            ("audio_socket", serde_json::json!("rvc-in.sock")),
            ("audio_socket", serde_json::json!("enhance.sock")),
        ] {
            let mut json = serde_json::to_value(&valid).unwrap();
            json[field] = value;
            let invalid = serde_json::from_value(json).unwrap();
            assert!(config.apply_effective_enhance(Some(invalid)).is_err());
            assert_eq!(config.enhance.as_ref().unwrap().model, valid.model);
        }
        config.apply_effective_enhance(None).unwrap();
        assert!(config.enhance.is_none());
    }

    #[test]
    fn enhance_rejects_invalid_config_and_socket_conflicts() {
        for body in [
            "[enhance]", "[enhance]\nmodel = ''",
            "[enhance]\nmodel = 'a'\nunknown = 1",
            "[enhance]\nmodel = 'a'\nqueue_frames = 0",
            "[enhance]\nmodel = 'a'\nqueue_frames = 1025",
            "[enhance]\nmodel = 'a'\nrequest_timeout_ms = 0",
            "[enhance]\nmodel = 'a'\nrequest_timeout_ms = 1001",
            "[enhance]\nmodel = 'a'\nsocket = '../bad'",
            "[enhance]\nmodel = 'a'\naudio_socket = '/bad'",
            "[enhance]\nmodel = 'a'\nsocket = 'rvc-in.sock'",
            "[enhance]\nmodel = 'a'\naudio_socket = 'asr-events.sock'",
            "[enhance]\nmodel = 'a'\naudio_socket = 'enhance.sock'",
            "[[pre.consumers]]\nname = 'custom'\nsocket = 'enhance-audio.sock'\n[enhance]\nmodel = 'a'",
        ] {
            let result = toml::from_str::<Config>(body).map_err(|err| err.to_string())
                .and_then(|config| config.validate().map_err(|err| err.to_string()));
            assert!(result.is_err(), "accepted {body}");
        }
        for (queue, timeout) in [(1, 1), (1024, 1000)] {
            let config: Config = toml::from_str(&format!(
                "[enhance]\nmodel = 'a'\nqueue_frames = {queue}\nrequest_timeout_ms = {timeout}"
            ))
            .unwrap();
            config.validate().unwrap();
        }
    }

    #[test]
    fn enhance_invalid_write_keeps_baseline() {
        let dir = temp_config_dir("enhance-invalid-write");
        let path = dir.join("mozart.toml");
        let baseline = "# retained\n[enhance]\nmodel = 'model.onnx'\n";
        std::fs::write(&path, baseline).unwrap();
        for (key, value) in [
            ("enhance.queue_frames", "0"),
            ("enhance.request_timeout_ms", "1001"),
            ("enhance.socket", "pre.sock"),
            ("enhance.model", ""),
        ] {
            assert!(set_key(Some(&path), key, value).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), baseline);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn web_adapter_ignores_other_modules_and_validates_consumed_fields() {
        let dir = temp_config_dir("web");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "[web]\nstatic_dir = 'assets'\n[bridge]\nsubtitles_path = 'subs.jsonl'\nfuture = 123\n[rvc.realtime]\nfuture = [1, 2]\n").unwrap();
        let config = WebAdapterConfig::load_from(Some(&path)).unwrap();
        assert_eq!(config.web.static_dir, dir.join("assets"));
        assert_eq!(config.subtitles_path, PathBuf::from("subs.jsonl"));
        assert!(Config::load_from(Some(&path)).is_err());
        for key in [
            "max_concurrent_uploads",
            "upload_timeout_ms",
            "sse_heartbeat_ms",
            "sse_buffer_bytes",
        ] {
            std::fs::write(&path, format!("[web]\n{key} = 0\n")).unwrap();
            assert!(WebAdapterConfig::load_from(Some(&path)).is_err());
            assert!(Config::load_from(Some(&path)).is_err());
        }
        std::fs::write(&path, "[daemon]\nsocket = '../bad'\n").unwrap();
        assert!(WebAdapterConfig::load_from(Some(&path)).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn device_key_kind_classification() {
        assert_eq!(device_key_kind("pre.capture_device"), Some("capture"));
        assert_eq!(device_key_kind("pre.playback_device"), Some("playback"));
        assert_eq!(device_key_kind("rvc.pitch_shift"), None);
        assert_eq!(device_key_kind(""), None);
    }

    #[test]
    fn set_key_writes_device_and_preserves_comments() {
        let dir = temp_config_dir("set");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "# 基线注释\n[pre]\ncapture_device = \"default\"\n").unwrap();
        let effect = set_key(Some(&path), "pre.capture_device", "plughw:CARD=X,DEV=0").unwrap();
        assert!(effect.restart_required);
        assert_eq!(effect.affected, vec!["pre".to_string()]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 基线注释"), "原子写回应保留注释：{text}");
        let config = Config::load_from(Some(&path)).unwrap();
        assert_eq!(config.pre.capture_device, "plughw:CARD=X,DEV=0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_key_rejects_unsupported_key_and_invalid_device() {
        let dir = temp_config_dir("reject");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "[pre]\n").unwrap();
        // 重启类 rvc 键与未接入对端下发的 pre/bridge 运行时键仍拒绝。
        assert!(set_key(Some(&path), "rvc.model_id", "demo").is_err());
        assert!(set_key(Some(&path), "rvc.device", "cpu").is_err());
        assert!(set_key(Some(&path), "bridge.translate_enabled", "false").is_err());
        assert!(set_key(Some(&path), "pre.capture_device", "").is_err());
        assert!(set_key(Some(&path), "pre.capture_device", "a\0b").is_err());
        assert!(set_key(None, "pre.capture_device", "default").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_key_reads_baseline_value() {
        let dir = temp_config_dir("get");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "[pre]\ncapture_device = \"default\"\n").unwrap();
        let value = get_key(Some(&path), Some("pre.capture_device")).unwrap();
        assert_eq!(value.as_str(), Some("default"));
        let whole = get_key(Some(&path), None).unwrap();
        assert!(whole.get("pre").is_some());
        assert!(get_key(Some(&path), Some("pre.nope")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_key_owner_matches_config_md() {
        assert_eq!(runtime_key_owner("daemon.rpc_timeout_ms"), Some("daemon"));
        assert_eq!(runtime_key_owner("file.queue_capacity"), Some("daemon"));
        assert_eq!(runtime_key_owner("file.denoise"), Some("daemon"));
        assert_eq!(runtime_key_owner("pre.dsp.denoise"), Some("pre"));
        assert_eq!(runtime_key_owner("rvc.pitch_shift"), Some("rvc"));
        assert_eq!(
            runtime_key_owner("bridge.translate_enabled"),
            Some("bridge")
        );
        // 重启类与未知键不是运行时键。
        assert_eq!(runtime_key_owner("pre.capture_device"), None);
        assert_eq!(runtime_key_owner("file.storage_dir"), None);
        assert_eq!(runtime_key_owner("daemon.run_dir"), None);
        assert_eq!(runtime_key_owner("nope"), None);
    }

    #[test]
    fn state_keys_are_bridge_switches_only() {
        assert!(is_state_key("bridge.translate_enabled"));
        assert!(is_state_key("bridge.speech_enabled"));
        assert!(!is_state_key("daemon.rpc_timeout_ms"));
        assert!(!is_state_key("rvc.pitch_shift"));
    }

    #[test]
    fn state_file_path_ends_with_state_json() {
        assert_eq!(state_file_path().file_name().unwrap(), "state.json");
    }

    #[test]
    fn set_key_writes_runtime_key_and_validates_range() {
        let dir = temp_config_dir("runtime-set");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "# 基线\n[daemon]\nrpc_timeout_ms = 2000\n").unwrap();
        let effect = set_key(Some(&path), "daemon.rpc_timeout_ms", "1500").unwrap();
        assert!(!effect.restart_required);
        assert!(effect.affected.is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 基线"), "原子写回应保留注释：{text}");
        assert!(text.contains("rpc_timeout_ms = 1500"), "{text}");

        // bool 与 string 运行时键。
        set_key(Some(&path), "file.denoise", "true").unwrap();
        set_key(Some(&path), "file.ffmpeg_path", "/usr/bin/ffmpeg").unwrap();
        let config = Config::load_from(Some(&path)).unwrap();
        assert!(config.file.denoise);
        assert_eq!(config.file.ffmpeg_path, "/usr/bin/ffmpeg");

        // 非法值不落盘：超范围、错类型、非 daemon owning。
        assert!(set_key(Some(&path), "daemon.rpc_timeout_ms", "0").is_err());
        assert!(set_key(Some(&path), "daemon.rpc_timeout_ms", "abc").is_err());
        assert!(set_key(Some(&path), "file.denoise", "yes").is_err());
        assert!(set_key(Some(&path), "rvc.models_dir", "/opt/models").is_err());
        for key in [
            "pre.dsp.denoise",
            "pre.dsp.gate",
            "pre.dsp.agc",
            "pre.dsp.limiter",
        ] {
            assert!(set_key(Some(&path), key, "yes").is_err());
            assert!(!set_key(Some(&path), key, "false").unwrap().restart_required);
        }
        assert!(!Config::load_from(Some(&path)).unwrap().pre.dsp.denoise);
        let config = Config::load_from(Some(&path)).unwrap();
        assert_eq!(
            config.daemon.rpc_timeout_ms, 1500,
            "被拒绝的 set 不得改基线"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_key_runtime_backoff_cross_key_constraint() {
        let dir = temp_config_dir("runtime-cross");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "[daemon]\nrestart_backoff_ms = 500\n").unwrap();
        // max < backoff 违反跨键约束：整体校验拒绝，不落盘。
        assert!(set_key(Some(&path), "daemon.restart_backoff_max_ms", "100").is_err());
        set_key(Some(&path), "daemon.restart_backoff_max_ms", "800").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_key_writes_rvc_session_keys_and_preserves_comments() {
        let dir = temp_config_dir("rvc-set");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "# 基线注释\n[rvc]\npitch_shift = 0\n").unwrap();

        let cases = [
            ("rvc.pitch_shift", "-12"),
            ("rvc.index_rate", "0.5"),
            ("rvc.rms_mix_rate", "1"),
            ("rvc.protect", "0.25"),
            ("rvc.f0_method", "rmvpe"),
        ];
        for (key, value) in cases {
            let effect = set_key(Some(&path), key, value).unwrap();
            assert!(!effect.restart_required, "rvc 会话参数无需重启：{key}");
            assert!(effect.affected.is_empty(), "{key}");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 基线注释"), "原子写应保留注释：{text}");
        let config = Config::load_from(Some(&path)).unwrap();
        assert_eq!(config.rvc.pitch_shift, -12);
        assert_eq!(config.rvc.index_rate, 0.5);
        assert_eq!(config.rvc.rms_mix_rate, 1.0);
        assert_eq!(config.rvc.protect, 0.25);
        assert_eq!(config.rvc.f0_method, "rmvpe");

        // 边界值合法：pitch_shift 12/-12、index_rate 0/1.0、rms_mix_rate 0、
        // protect 0/0.5。
        set_key(Some(&path), "rvc.pitch_shift", "12").unwrap();
        set_key(Some(&path), "rvc.index_rate", "0").unwrap();
        set_key(Some(&path), "rvc.rms_mix_rate", "0").unwrap();
        set_key(Some(&path), "rvc.protect", "0.5").unwrap();
        set_key(Some(&path), "rvc.index_rate", "1.0").unwrap();
        set_key(Some(&path), "rvc.protect", "0").unwrap();
        let config = Config::load_from(Some(&path)).unwrap();
        assert_eq!(config.rvc.pitch_shift, 12);
        assert_eq!(config.rvc.index_rate, 1.0);
        assert_eq!(config.rvc.rms_mix_rate, 0.0);
        assert_eq!(config.rvc.protect, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_rvc_session_baseline_only_touches_five_keys() {
        let dir = temp_config_dir("preset-baseline");
        let path = dir.join("mozart.toml");
        std::fs::write(
            &path,
            "# 顶部注释\n[rvc]\nmodel_id = \"model-a\"\nmodels_dir = \"/models\"\ndevice = \"cpu\"\npitch_shift = 0\n",
        )
        .unwrap();
        let values = RvcSessionValues {
            pitch_shift: 3,
            index_rate: 0.5,
            rms_mix_rate: 0.25,
            protect: 0.4,
            f0_method: "rmvpe".to_string(),
        };
        write_rvc_session_baseline(&path, &values).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 顶部注释"), "应保留注释：{text}");
        assert!(text.contains("model_id = \"model-a\""), "{text}");
        assert!(text.contains("models_dir = \"/models\""), "{text}");
        assert!(text.contains("device = \"cpu\""), "{text}");
        let config = Config::load_from(Some(&path)).unwrap();
        assert_eq!(config.rvc.session_values(), values);
        assert_eq!(config.rvc.model_id, "model-a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rvc_model_dir_prefers_registry_and_guards_fallback_id() {
        let config: Config = toml::from_str(
            "[rvc]\nmodels_dir = \"/models\"\n\n[[models]]\nid = \"model-a\"\ntype = \"rvc\"\npath = \"/registry/a\"\n",
        )
        .unwrap();
        assert_eq!(
            config.rvc_model_dir("model-a"),
            Some(PathBuf::from("/registry/a"))
        );
        assert_eq!(
            config.rvc_model_dir("model-b"),
            Some(PathBuf::from("/models/model-b"))
        );
        assert_eq!(config.rvc_model_dir("../escape"), None);
        assert_eq!(config.rvc_model_dir("a/b"), None);
        assert_eq!(config.rvc_model_dir("."), None);
        assert_eq!(config.rvc_model_dir(""), None);

        let no_dir: Config = toml::from_str("[rvc]\n").unwrap();
        assert_eq!(no_dir.rvc_model_dir("model-a"), None);
    }

    #[test]
    fn get_key_reads_rvc_session_baseline_after_set() {
        let dir = temp_config_dir("rvc-get");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "[rvc]\n").unwrap();

        // 未显式写入的键不在 TOML 基线中；config get 只读显式基线。
        assert!(get_key(Some(&path), Some("rvc.pitch_shift")).is_err());

        set_key(Some(&path), "rvc.pitch_shift", "3").unwrap();
        set_key(Some(&path), "rvc.f0_method", "rmvpe").unwrap();
        assert_eq!(
            get_key(Some(&path), Some("rvc.pitch_shift"))
                .unwrap()
                .as_integer(),
            Some(3)
        );
        assert_eq!(
            get_key(Some(&path), Some("rvc.f0_method"))
                .unwrap()
                .as_str(),
            Some("rmvpe")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_key_rejects_invalid_rvc_session_values_without_writing() {
        let dir = temp_config_dir("rvc-reject");
        let path = dir.join("mozart.toml");
        std::fs::write(
            &path,
            "[rvc]\npitch_shift = 2\nindex_rate = 0.5\nrms_mix_rate = 0.5\nprotect = 0.2\n",
        )
        .unwrap();

        let cases = [
            // 越界。
            ("rvc.pitch_shift", "13"),
            ("rvc.pitch_shift", "-13"),
            ("rvc.index_rate", "1.01"),
            ("rvc.index_rate", "-0.01"),
            ("rvc.rms_mix_rate", "1.5"),
            ("rvc.protect", "0.51"),
            // 错类型 / 非有限值。
            ("rvc.pitch_shift", "1.5"),
            ("rvc.index_rate", "abc"),
            ("rvc.protect", "NaN"),
            ("rvc.index_rate", "inf"),
            ("rvc.index_rate", "-inf"),
            ("rvc.protect", "inf"),
            ("rvc.protect", "-inf"),
            // f0_method 仅 rmvpe。
            ("rvc.f0_method", "harvest"),
            ("rvc.f0_method", "RMVPE"),
        ];
        for (key, value) in cases {
            assert!(
                set_key(Some(&path), key, value).is_err(),
                "应拒绝 {key}={value}"
            );
        }
        let err = set_key(Some(&path), "rvc.f0_method", "harvest")
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("rmvpe"),
            "错误信息应指明 rmvpe：{err}"
        );
        let err = set_key(Some(&path), "rvc.pitch_shift", "13").err().unwrap();
        assert!(
            err.to_string().contains("-12..=12"),
            "错误信息应带范围：{err}"
        );
        let err = set_key(Some(&path), "rvc.index_rate", "1.01")
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("0.0..=1.0"),
            "错误信息应带范围：{err}"
        );

        // 被拒绝的 set 不得改基线。
        let config = Config::load_from(Some(&path)).unwrap();
        assert_eq!(config.rvc.pitch_shift, 2);
        assert_eq!(config.rvc.index_rate, 0.5);
        assert_eq!(config.rvc.rms_mix_rate, 0.5);
        assert_eq!(config.rvc.protect, 0.2);
        assert_eq!(config.rvc.f0_method, "rmvpe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_overrides_batch_writes_and_preserves_comments() {
        let dir = temp_config_dir("save");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "# 顶部注释\n[bridge]\ntranslate_enabled = true\n").unwrap();
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert(
            "bridge.translate_enabled".to_string(),
            serde_json::json!(false),
        );
        overrides.insert(
            "bridge.speech_enabled".to_string(),
            serde_json::json!(false),
        );
        let saved = save_overrides(Some(&path), &overrides).unwrap();
        assert_eq!(saved, 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 顶部注释"), "{text}");
        let config = Config::load_from(Some(&path)).unwrap();
        assert!(!config.bridge.translate_enabled);
        assert!(!config.bridge.speech_enabled);

        // 空覆盖不动 TOML，无配置文件也可空写回。
        let empty = std::collections::BTreeMap::new();
        assert_eq!(save_overrides(Some(&path), &empty).unwrap(), 0);
        assert_eq!(save_overrides(None, &empty).unwrap(), 0);
        // 非空覆盖无配置文件：拒绝。
        assert!(save_overrides(None, &overrides).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_match_confirmed_paths() {
        let config = Config::default();
        assert_eq!(config.daemon.run_dir, PathBuf::from("/run/mozart"));
        assert_eq!(
            config.daemon_socket(),
            PathBuf::from("/run/mozart/daemon.sock")
        );
        assert_eq!(config.pre_socket(), PathBuf::from("/run/mozart/pre.sock"));
        assert_eq!(config.rvc_socket(), PathBuf::from("/run/mozart/rvc.sock"));
        assert_eq!(
            config.rvc_input_socket(),
            PathBuf::from("/run/mozart/rvc-in.sock")
        );
        assert_eq!(
            config.rvc_output_socket(),
            PathBuf::from("/run/mozart/rvc-out.sock")
        );
        assert_eq!(config.daemon.poll_interval_ms, 1000);
        assert_eq!(config.pre.capture_device, "default");
        assert_eq!(config.pre.playback_device, "default");
        assert_eq!(config.pre.capture_queue_frames, 8);
        assert!(config.pre.dsp.denoise, "denoise 默认沿用旧 main.c 代码默认");
        assert!(!config.pre.dsp.gate, "gate 默认关（旧 main.c 代码默认）");
        assert!(!config.pre.dsp.agc, "agc 默认关");
        assert!(!config.pre.dsp.limiter, "limiter 默认关");
    }

    #[test]
    fn toml_overrides_defaults() {
        let config: Config = toml::from_str(
            r#"
            [daemon]
            poll_interval_ms = 250
            [pre]
            socket = "audio.sock"
            capture_device = "plughw:CARD=Loopback,DEV=1,SUBDEV=0"
            "#,
        )
        .unwrap();
        assert_eq!(config.daemon.poll_interval_ms, 250);
        assert_eq!(config.pre_socket(), PathBuf::from("/run/mozart/audio.sock"));
        assert_eq!(config.rvc.socket, "rvc.sock");
        assert_eq!(
            config.pre.capture_device,
            "plughw:CARD=Loopback,DEV=1,SUBDEV=0"
        );
        // 未写的键沿用默认。
        assert_eq!(config.pre.playback_device, "default");
        assert_eq!(config.pre.capture_queue_frames, 8);
        assert!(config.pre.dsp.denoise);

        let denoise_off: Config = toml::from_str("[pre.dsp]\ndenoise = false\n").unwrap();
        assert!(!denoise_off.pre.dsp.denoise);

        let stages_on: Config =
            toml::from_str("[pre.dsp]\ngate = true\nagc = true\nlimiter = true\n").unwrap();
        assert!(stages_on.pre.dsp.gate);
        assert!(stages_on.pre.dsp.agc);
        assert!(stages_on.pre.dsp.limiter);
        assert!(stages_on.pre.dsp.denoise, "未写的键沿用默认");
    }

    #[test]
    fn pre_consumers_default_falls_back_to_rvc_and_asr() {
        // 未写 [[pre.consumers]]：回退两支路，socket 沿用 [rvc]/[asr]。
        let config = Config::default();
        let consumers = config.pre_consumers();
        assert_eq!(consumers.len(), 2);
        assert_eq!(consumers[0].name, "rvc");
        assert_eq!(consumers[0].socket, "rvc-in.sock");
        assert!(consumers[0].file_mode_affected, "rvc 受文件接管影响");
        assert_eq!(consumers[1].name, "asr");
        assert_eq!(consumers[1].socket, "asr-in.sock");
        assert!(!consumers[1].file_mode_affected, "asr 不受文件接管影响");

        // 自定义 [rvc]/[asr] socket 时回退列表跟随（向后兼容）。
        let custom: Config = toml::from_str(
            "[rvc]\ninput_socket = \"voice-in.sock\"\n[asr]\ninput_socket = \"listen-in.sock\"\n",
        )
        .unwrap();
        let consumers = custom.pre_consumers();
        assert_eq!(consumers[0].socket, "voice-in.sock");
        assert_eq!(consumers[1].socket, "listen-in.sock");
        assert!(custom.validate().is_ok());
    }

    #[test]
    fn pre_consumers_toml_overrides() {
        let config: Config = toml::from_str(
            r#"
            [[pre.consumers]]
            name = "rvc"
            socket = "rvc-in.sock"
            file_mode_affected = true
            [[pre.consumers]]
            name = "asr"
            socket = "asr-in.sock"
            [[pre.consumers]]
            name = "extra"
            socket = "extra-in.sock"
            "#,
        )
        .unwrap();
        let consumers = config.pre_consumers();
        assert_eq!(consumers.len(), 3);
        assert_eq!(consumers[2].name, "extra");
        assert_eq!(consumers[2].socket, "extra-in.sock");
        assert!(
            !consumers[2].file_mode_affected,
            "file_mode_affected 缺省 false（恒发）"
        );
        assert!(config.validate().is_ok());

        // 显式列表优先于 [rvc]/[asr] 回退；空列表合法（不分发）。
        let empty: Config = toml::from_str("[pre]\nconsumers = []\n").unwrap();
        assert!(empty.pre_consumers().is_empty());
        assert!(empty.validate().is_ok());

        let unknown: Result<Config, _> = toml::from_str(
            "[[pre.consumers]]\nname = \"x\"\nsocket = \"x.sock\"\nunknown_key = 1\n",
        );
        assert!(unknown.is_err());
    }

    #[test]
    fn pre_consumers_invalid_values_are_rejected() {
        let cases = [
            // 空名 / 保留名 / 点名。
            "[[pre.consumers]]\nname = \"\"\nsocket = \"x.sock\"\n",
            "[[pre.consumers]]\nname = \"tts\"\nsocket = \"x.sock\"\n",
            "[[pre.consumers]]\nname = \"rvc.skipped\"\nsocket = \"x.sock\"\n",
            // socket 非纯文件名。
            "[[pre.consumers]]\nname = \"x\"\nsocket = \"a/b.sock\"\n",
            "[[pre.consumers]]\nname = \"x\"\nsocket = \"\"\n",
            // 重名 / 重 socket。
            "[[pre.consumers]]\nname = \"x\"\nsocket = \"a.sock\"\n\
             [[pre.consumers]]\nname = \"x\"\nsocket = \"b.sock\"\n",
            "[[pre.consumers]]\nname = \"x\"\nsocket = \"a.sock\"\n\
             [[pre.consumers]]\nname = \"y\"\nsocket = \"a.sock\"\n",
        ];
        for text in cases {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.validate().is_err(), "应拒绝：{text}");
        }

        // 开关类型错误：解析期即拒绝。
        let bad_type: Result<Config, _> = toml::from_str(
            "[[pre.consumers]]\nname = \"x\"\nsocket = \"x.sock\"\nfile_mode_affected = \"yes\"\n",
        );
        assert!(bad_type.is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        let zero_interval: Config = toml::from_str("[daemon]\npoll_interval_ms = 0\n").unwrap();
        assert!(zero_interval.validate().is_err());

        let path_socket: Config = toml::from_str("[pre]\nsocket = \"a/b\"\n").unwrap();
        assert!(path_socket.validate().is_err());

        let empty_device: Config = toml::from_str("[pre]\ncapture_device = \"\"\n").unwrap();
        assert!(empty_device.validate().is_err());

        let nul_device: Config =
            toml::from_str("[pre]\nplayback_device = \"defa\\u0000ult\"\n").unwrap();
        assert!(nul_device.validate().is_err());

        let zero_queue: Config = toml::from_str("[pre]\ncapture_queue_frames = 0\n").unwrap();
        assert!(zero_queue.validate().is_err());

        let huge_queue: Config = toml::from_str("[pre]\ncapture_queue_frames = 1025\n").unwrap();
        assert!(huge_queue.validate().is_err());

        let max_queue: Config = toml::from_str("[pre]\ncapture_queue_frames = 1024\n").unwrap();
        assert!(max_queue.validate().is_ok());

        let bad_pitch: Config = toml::from_str("[rvc]\npitch_shift = 13\n").unwrap();
        assert!(bad_pitch.validate().is_err());

        let bad_method: Config = toml::from_str("[rvc]\nf0_method = \"harvest\"\n").unwrap();
        assert!(bad_method.validate().is_err());

        let data_socket_with_slash: Config =
            toml::from_str("[rvc]\ninput_socket = \"a/b\"\n").unwrap();
        assert!(data_socket_with_slash.validate().is_err());

        let unknown: Result<Config, _> = toml::from_str("[daemon]\nunknown_key = 1\n");
        assert!(unknown.is_err());

        // [rvc] 进程启动键（C++ 推理进程读取）：默认零值，可解析。
        let startup: Config = toml::from_str(
            r#"
            [rvc]
            models_dir = "/opt/mozart/models"
            device = "cuda"
            half = false
            cache_limit = 2
            hubert_path = "/opt/mozart/hubert.onnx"
            rmvpe_path = "/opt/mozart/rmvpe.onnx"
            realtime_hubert_path = "/opt/mozart/hubert-realtime.onnx"
            realtime_rmvpe_path = "/opt/mozart/rmvpe-realtime.onnx"
            "#,
        )
        .unwrap();
        assert_eq!(startup.rvc.models_dir, "/opt/mozart/models");
        assert_eq!(startup.rvc.device, "cuda");
        assert!(!startup.rvc.half);
        assert_eq!(startup.rvc.cache_limit, 2);
        assert_eq!(
            startup.rvc.realtime_rmvpe_path,
            "/opt/mozart/rmvpe-realtime.onnx"
        );
        assert!(startup.validate().is_ok());
        assert_eq!(Config::default().rvc.models_dir, "");

        let unknown_pre_key: Result<Config, _> = toml::from_str("[pre]\nunknown_key = 1\n");
        assert!(unknown_pre_key.is_err());

        let unknown_dsp_key: Result<Config, _> = toml::from_str("[pre.dsp]\nunknown_key = 1\n");
        assert!(unknown_dsp_key.is_err());

        let bad_denoise_type: Result<Config, _> = toml::from_str("[pre.dsp]\ndenoise = \"yes\"\n");
        assert!(bad_denoise_type.is_err());

        let bad_gate_type: Result<Config, _> = toml::from_str("[pre.dsp]\ngate = 1\n");
        assert!(bad_gate_type.is_err());
    }

    #[test]
    fn ensure_run_dir_converges_to_0700() {
        let dir = std::env::temp_dir().join(format!("mozart-rundir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        ensure_run_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dir, perms).unwrap();
        ensure_run_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_log_dir_converges_to_0700() {
        let dir = std::env::temp_dir().join(format!("mozart-logdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        ensure_log_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        ensure_log_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn web_config_defaults_to_8080() {
        let web = WebConfig::default();
        assert_eq!(web.listen, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
        assert_eq!(
            web.allowed_origins,
            vec![
                "http://localhost:8080",
                "http://127.0.0.1:8080",
                "http://[::1]:8080",
            ]
        );
        let default_cfg = Config::default();
        assert!(default_cfg.web.is_none());
        assert_eq!(
            default_cfg.process.web_bin,
            PathBuf::from("mozart-web-adapter")
        );
        let with_web: Config = toml::from_str("[web]\n").unwrap();
        assert_eq!(
            with_web.web.as_ref().unwrap().listen,
            "0.0.0.0:8080".parse::<SocketAddr>().unwrap()
        );
        let empty_web_bin: Config = toml::from_str("[process]\nweb_bin = \"\"\n").unwrap();
        assert!(empty_web_bin.validate().is_err());
        let dir = temp_config_dir("web_configured");
        let without_path = dir.join("no_web.toml");
        std::fs::write(&without_path, "").unwrap();
        assert!(!WebAdapterConfig::load_from(Some(&without_path))
            .unwrap()
            .web_configured);
        let with_path = dir.join("with_web.toml");
        std::fs::write(&with_path, "[web]\n").unwrap();
        assert!(WebAdapterConfig::load_from(Some(&with_path))
            .unwrap()
            .web_configured);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn language_chain_defaults() {
        let config = Config::default();
        assert_eq!(config.asr_socket(), PathBuf::from("/run/mozart/asr.sock"));
        assert_eq!(
            config.asr_input_socket(),
            PathBuf::from("/run/mozart/asr-in.sock")
        );
        assert_eq!(
            config.asr_events_socket(),
            PathBuf::from("/run/mozart/asr-events.sock")
        );
        assert_eq!(config.tts_socket(), PathBuf::from("/run/mozart/tts.sock"));
        assert_eq!(
            config.tts_jobs_socket(),
            PathBuf::from("/run/mozart/tts-jobs.sock")
        );
        assert_eq!(
            config.tts_output_socket(),
            PathBuf::from("/run/mozart/tts-out.sock")
        );
        assert_eq!(
            config.bridge_socket(),
            PathBuf::from("/run/mozart/bridge.sock")
        );
        assert_eq!(
            config.llama_socket(),
            PathBuf::from("/run/mozart/llama.sock")
        );

        assert_eq!(config.asr.model_dir, PathBuf::new());
        assert_eq!(config.asr.review_model_dir, PathBuf::new());
        assert_eq!(config.asr.threads, 2);
        assert_eq!(config.asr.ring_capacity, 64);
        assert_eq!(config.asr.output_queue_capacity, 64);

        assert_eq!(config.tts.model_dir, PathBuf::new());
        assert_eq!(config.tts.reference_wav, PathBuf::new());
        assert_eq!(config.tts.threads, 2);
        assert_eq!(config.tts.provider, "cpu");
        assert_eq!(config.tts.queue_capacity, 8);

        assert_eq!(
            config.bridge.subtitles_path,
            PathBuf::from("~/.local/state/mozart/subtitles.jsonl")
        );
        assert!(config.bridge.translate_enabled);
        assert!(config.bridge.speech_enabled);
        assert_eq!(config.bridge.llama_model, PathBuf::new());
        assert_eq!(config.bridge.translate_queue_capacity, 32);
        assert_eq!(config.bridge.speech_queue_capacity, 16);
        assert_eq!(config.bridge.retry_max, 2);

        assert_eq!(config.process.asr_bin, PathBuf::from("mozart-asr"));
        assert_eq!(config.process.tts_bin, PathBuf::from("mozart-tts"));
        assert_eq!(config.process.bridge_bin, PathBuf::from("mozart-bridge"));
        assert_eq!(config.process.llama_bin, PathBuf::from("llama-server"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn language_chain_toml_overrides() {
        // 旧 TOML 不含新段：沿用默认，可解析、可校验。
        let legacy: Config = toml::from_str("[daemon]\npoll_interval_ms = 250\n").unwrap();
        assert_eq!(legacy.asr.socket, "asr.sock");
        assert_eq!(legacy.tts.provider, "cpu");
        assert!(legacy.bridge.translate_enabled);
        assert!(legacy.validate().is_ok());

        let config: Config = toml::from_str(
            r#"
            [asr]
            socket = "recognize.sock"
            model_dir = "/opt/mozart/zipformer"
            review_model_dir = "/opt/mozart/sensevoice"
            threads = 4
            ring_capacity = 128
            output_queue_capacity = 32
            [tts]
            jobs_socket = "synth-jobs.sock"
            model_dir = "/opt/mozart/pocket"
            reference_wav = "/opt/mozart/voice.wav"
            threads = 3
            provider = "cuda"
            queue_capacity = 4
            [bridge]
            subtitles_path = "/var/lib/mozart/subtitles.jsonl"
            translate_enabled = false
            speech_enabled = false
            llama_socket = "llm.sock"
            llama_model = "/opt/mozart/qwen.gguf"
            translate_queue_capacity = 64
            speech_queue_capacity = 8
            retry_max = 1
            [process]
            asr_bin = "/opt/mozart/bin/mozart-asr"
            llama_bin = "/opt/llama/bin/llama-server"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.asr_socket(),
            PathBuf::from("/run/mozart/recognize.sock")
        );
        assert_eq!(config.asr.model_dir, PathBuf::from("/opt/mozart/zipformer"));
        assert_eq!(config.asr.threads, 4);
        assert_eq!(config.asr.output_queue_capacity, 32);
        assert_eq!(
            config.tts_jobs_socket(),
            PathBuf::from("/run/mozart/synth-jobs.sock")
        );
        assert_eq!(
            config.tts.reference_wav,
            PathBuf::from("/opt/mozart/voice.wav")
        );
        assert_eq!(config.tts.threads, 3);
        assert_eq!(config.tts.provider, "cuda");
        assert_eq!(config.tts.queue_capacity, 4);
        assert_eq!(
            config.bridge.subtitles_path,
            PathBuf::from("/var/lib/mozart/subtitles.jsonl")
        );
        assert!(!config.bridge.translate_enabled);
        assert!(!config.bridge.speech_enabled);
        assert_eq!(config.llama_socket(), PathBuf::from("/run/mozart/llm.sock"));
        assert_eq!(
            config.bridge.llama_model,
            PathBuf::from("/opt/mozart/qwen.gguf")
        );
        assert_eq!(config.bridge.translate_queue_capacity, 64);
        assert_eq!(config.bridge.speech_queue_capacity, 8);
        assert_eq!(config.bridge.retry_max, 1);
        assert_eq!(
            config.process.asr_bin,
            PathBuf::from("/opt/mozart/bin/mozart-asr")
        );
        assert_eq!(
            config.process.llama_bin,
            PathBuf::from("/opt/llama/bin/llama-server")
        );
        // 未写的键沿用默认。
        assert_eq!(config.asr.input_socket, "asr-in.sock");
        assert_eq!(config.tts.socket, "tts.sock");
        assert_eq!(config.bridge.socket, "bridge.sock");
        assert!(config.validate().is_ok());

        let unknown_asr: Result<Config, _> = toml::from_str("[asr]\nunknown_key = 1\n");
        assert!(unknown_asr.is_err());
        let unknown_tts: Result<Config, _> = toml::from_str("[tts]\nunknown_key = 1\n");
        assert!(unknown_tts.is_err());
        let unknown_bridge: Result<Config, _> = toml::from_str("[bridge]\nunknown_key = 1\n");
        assert!(unknown_bridge.is_err());
    }

    #[test]
    fn language_chain_invalid_values_are_rejected() {
        let cases = [
            "[asr]\nsocket = \"a/b\"\n",
            "[asr]\ninput_socket = \"a/b\"\n",
            "[asr]\nevents_socket = \"\"\n",
            "[asr]\nthreads = 0\n",
            "[asr]\nring_capacity = 0\n",
            "[asr]\noutput_queue_capacity = 0\n",
            "[tts]\nsocket = \"a/b\"\n",
            "[tts]\njobs_socket = \"\"\n",
            "[tts]\noutput_socket = \"a/b\"\n",
            "[tts]\nthreads = 0\n",
            "[tts]\nprovider = \"\"\n",
            "[tts]\nqueue_capacity = 0\n",
            "[bridge]\nsocket = \"a/b\"\n",
            "[bridge]\nllama_socket = \"a/b\"\n",
            "[bridge]\ntranslate_queue_capacity = 0\n",
            "[bridge]\nspeech_queue_capacity = 0\n",
            "[bridge]\nretry_max = 3\n",
            "[process]\nasr_bin = \"\"\n",
            "[process]\ntts_bin = \"\"\n",
            "[process]\nbridge_bin = \"\"\n",
            "[process]\nllama_bin = \"\"\n",
        ];
        for text in cases {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.validate().is_err(), "应拒绝：{text}");
        }

        // 边界值合法：线程数与队列容量取最小 1。
        let edge: Config = toml::from_str(
            "[asr]\nthreads = 1\nring_capacity = 1\noutput_queue_capacity = 1\n\
             [tts]\nthreads = 1\nqueue_capacity = 1\n[bridge]\ntranslate_queue_capacity = 1\nspeech_queue_capacity = 1\n",
        )
        .unwrap();
        assert!(edge.validate().is_ok());
    }

    #[test]
    fn expand_tilde_only_touches_leading_tilde() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_tilde(Path::new("~/state/subtitles.jsonl"), Some(home)),
            PathBuf::from("/home/tester/state/subtitles.jsonl")
        );
        assert_eq!(expand_tilde(Path::new("~"), Some(home)), home);
        assert_eq!(
            expand_tilde(Path::new("/var/lib/subtitles.jsonl"), Some(home)),
            PathBuf::from("/var/lib/subtitles.jsonl")
        );
        assert_eq!(
            expand_tilde(Path::new("~other/subtitles.jsonl"), Some(home)),
            PathBuf::from("~other/subtitles.jsonl")
        );
        assert_eq!(
            expand_tilde(Path::new("~/subtitles.jsonl"), None),
            PathBuf::from("~/subtitles.jsonl")
        );
    }

    #[test]
    fn load_from_expands_subtitles_path() {
        let dir = std::env::temp_dir().join(format!("mozart-bridge-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mozart.toml");
        std::fs::write(
            &path,
            "[bridge]\nsubtitles_path = \"~/mozart-test/subtitles.jsonl\"\n\
             corrections_path = \"~/mozart-test/corrections.json\"\n",
        )
        .unwrap();

        let config = Config::load_from(Some(&path)).unwrap();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let expected = |tail: &str| match &home {
            Some(home) => home.join(tail),
            None => PathBuf::from("~/").join(tail),
        };
        assert_eq!(
            config.bridge.subtitles_path,
            expected("mozart-test/subtitles.jsonl")
        );
        assert_eq!(
            config.bridge.corrections_path,
            Some(expected("mozart-test/corrections.json"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrections_path_defaults_to_none() {
        let config = Config::load_from(None).unwrap();
        assert_eq!(config.bridge.corrections_path, None);
    }

    #[test]
    fn file_chain_defaults() {
        let config = Config::default();
        assert_eq!(
            config.file.storage_dir,
            PathBuf::from("~/.local/state/mozart/files/")
        );
        assert_eq!(config.file.queue_capacity, 50);
        assert_eq!(config.file.storage_limit_bytes, 1_048_576_000);
        assert!(!config.file.denoise);
        assert_eq!(config.file.max_duration_seconds, 300);
        assert_eq!(config.file.max_upload_bytes, 104_857_600);
        assert_eq!(config.file.ffmpeg_path, "ffmpeg");
        assert_eq!(config.file.ffmpeg_timeout_ms, 120_000);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn file_chain_toml_overrides() {
        // 旧 TOML 不含 [file]：沿用默认，可解析、可校验。
        let legacy: Config = toml::from_str("[daemon]\npoll_interval_ms = 250\n").unwrap();
        assert_eq!(legacy.file.queue_capacity, 50);
        assert!(legacy.validate().is_ok());

        let config: Config = toml::from_str(
            r#"
            [file]
            storage_dir = "/var/lib/mozart/files"
            queue_capacity = 10
            storage_limit_bytes = 2048
            denoise = true
            max_duration_seconds = 60
            max_upload_bytes = 4096
            ffmpeg_path = "/usr/bin/ffmpeg"
            ffmpeg_timeout_ms = 5000
            "#,
        )
        .unwrap();
        assert_eq!(
            config.file_storage_dir(),
            PathBuf::from("/var/lib/mozart/files")
        );
        assert_eq!(config.file.queue_capacity, 10);
        assert_eq!(config.file.storage_limit_bytes, 2048);
        assert!(config.file.denoise);
        assert_eq!(config.file.max_duration_seconds, 60);
        assert_eq!(config.file.max_upload_bytes, 4096);
        assert_eq!(config.file.ffmpeg_path, "/usr/bin/ffmpeg");
        assert_eq!(config.file.ffmpeg_timeout_ms, 5000);
        assert!(config.validate().is_ok());

        let unknown: Result<Config, _> = toml::from_str("[file]\nunknown_key = 1\n");
        assert!(unknown.is_err());
        // D2 独立 socket 键已回退：数据面复用 [rvc]，旧键拒收。
        let legacy_socket: Result<Config, _> =
            toml::from_str("[file]\noutput_socket = \"rvc-file-out.sock\"\n");
        assert!(legacy_socket.is_err());
        let legacy_input: Result<Config, _> =
            toml::from_str("[file]\ninput_socket = \"rvc-file-in.sock\"\n");
        assert!(legacy_input.is_err());
    }

    #[test]
    fn file_chain_invalid_values_are_rejected() {
        let cases = [
            "[file]\nqueue_capacity = 0\n",
            "[file]\nstorage_limit_bytes = 0\n",
            "[file]\nmax_duration_seconds = 0\n",
            "[file]\nmax_upload_bytes = 0\n",
            "[file]\nffmpeg_timeout_ms = 0\n",
        ];
        for text in cases {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.validate().is_err(), "应拒绝：{text}");
        }

        // 边界值合法：各下限取最小正值。
        let edge: Config = toml::from_str(
            "[file]\nqueue_capacity = 1\nstorage_limit_bytes = 1\nmax_duration_seconds = 1\n\
             max_upload_bytes = 1\nffmpeg_timeout_ms = 1\n",
        )
        .unwrap();
        assert!(edge.validate().is_ok());
    }

    #[test]
    fn load_from_expands_file_storage_dir() {
        let dir = std::env::temp_dir().join(format!("mozart-file-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "[file]\nstorage_dir = \"~/mozart-test/files\"\n").unwrap();

        let home = std::env::var_os("HOME").map(PathBuf::from);
        let config = Config::load_from(Some(&path)).unwrap();
        let expected = match &home {
            Some(home) => home.join("mozart-test/files"),
            None => PathBuf::from("~/mozart-test/files"),
        };
        assert_eq!(config.file_storage_dir(), expected);

        // 未写 [file] 时默认目录同样展开。
        let default_loaded = Config::load_from(None).unwrap();
        let expected_default = match &home {
            Some(home) => home.join(".local/state/mozart/files"),
            None => PathBuf::from("~/.local/state/mozart/files"),
        };
        assert_eq!(default_loaded.file_storage_dir(), expected_default);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rvc_realtime_defaults_and_validation() {
        let dir = temp_config_dir("rvc-realtime");
        let path = dir.join("mozart.toml");
        std::fs::write(&path, "[rvc]\npitch_shift = 0\n").unwrap();

        let cfg = Config::load_from(Some(&path)).unwrap();
        assert_eq!(cfg.rvc.realtime.crossfade_ms, 50);
        assert_eq!(cfg.rvc.realtime.sola_search_ms, 10);
        assert_eq!(cfg.rvc.realtime.startup_delay_ms, 400);
        assert!(cfg.rvc.realtime.skip_silence);

        // overlap 锁为 60 ms（50 + 10）；startup_delay 与 skip_silence 仍可调。
        set_key(Some(&path), "rvc.realtime.crossfade_ms", "50").unwrap();
        set_key(Some(&path), "rvc.realtime.sola_search_ms", "10").unwrap();
        set_key(Some(&path), "rvc.realtime.startup_delay_ms", "200").unwrap();
        set_key(Some(&path), "rvc.realtime.skip_silence", "false").unwrap();

        let updated = Config::load_from(Some(&path)).unwrap();
        assert_eq!(updated.rvc.realtime.crossfade_ms, 50);
        assert_eq!(updated.rvc.realtime.sola_search_ms, 10);
        assert_eq!(updated.rvc.realtime.startup_delay_ms, 200);
        assert!(!updated.rvc.realtime.skip_silence);

        assert!(set_key(Some(&path), "rvc.realtime.crossfade_ms", "51").is_err());
        assert!(set_key(Some(&path), "rvc.realtime.sola_search_ms", "11").is_err());
        // 上限内但 overlap 不等于 60：引擎形状对不上，入口拒绝且不写回文件。
        assert!(set_key(Some(&path), "rvc.realtime.crossfade_ms", "40").is_err());
        assert!(set_key(Some(&path), "rvc.realtime.sola_search_ms", "5").is_err());
        // 上限内但违反启动缓冲整除：同样在入口拒绝。
        assert!(set_key(Some(&path), "rvc.realtime.startup_delay_ms", "410").is_err());
        let unchanged = Config::load_from(Some(&path)).unwrap();
        assert_eq!(unchanged.rvc.realtime.crossfade_ms, 50);
        assert_eq!(unchanged.rvc.realtime.sola_search_ms, 10);
        assert_eq!(unchanged.rvc.realtime.startup_delay_ms, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 启动缓冲整除约束来自 StreamingRvc 的几何要求（落在契约帧网格）：
    /// 上下限内的非法值必须在入口拒绝，不冒到推理层。overlap 锁定后
    /// startup_delay_ms 仍可在整个合法范围内变动。
    #[test]
    fn rvc_realtime_grid_constraints_are_enforced_at_entry() {
        let rejected = [
            "[rvc.realtime]\nstartup_delay_ms = 410\n",
            "[rvc.realtime]\nstartup_delay_ms = 1\n",
        ];
        for text in rejected {
            let config: Config = toml::from_str(text).unwrap();
            let err = config.validate().unwrap_err().to_string();
            assert!(
                err.contains("multiple of"),
                "应因整除约束拒绝：{text}（实际 {err}）"
            );
        }

        let accepted = [
            "[rvc.realtime]\ncrossfade_ms = 50\nsola_search_ms = 10\nstartup_delay_ms = 15000\n",
            "[rvc.realtime]\ncrossfade_ms = 50\nsola_search_ms = 10\nstartup_delay_ms = 0\n",
            "[rvc.realtime]\ncrossfade_ms = 50\nsola_search_ms = 10\nstartup_delay_ms = 20\n",
            "[rvc.realtime]\ncrossfade_ms = 50\nsola_search_ms = 10\nstartup_delay_ms = 200\n",
        ];
        for text in accepted {
            let config: Config = toml::from_str(text).unwrap();
            assert!(config.validate().is_ok(), "应接受：{text}");
        }
    }

    /// overlap 锁为引擎形状对应的定值 60 ms：小于它时 StreamingRvc 的分析窗
    /// 帧数与资产不符，推理层每块抛异常，故两个入口都必须在入口拒绝。
    /// 与 C++ 侧 `validate_realtime_knobs` 同口径（tests/test_rvc_realtime_params.cpp）。
    #[test]
    fn rvc_realtime_overlap_is_locked_to_engine_shape() {
        let rejected = [
            "[rvc.realtime]\ncrossfade_ms = 40\nsola_search_ms = 10\n",
            "[rvc.realtime]\ncrossfade_ms = 30\nsola_search_ms = 10\n",
            "[rvc.realtime]\ncrossfade_ms = 45\nsola_search_ms = 0\n",
            "[rvc.realtime]\ncrossfade_ms = 40\nsola_search_ms = 5\n",
            "[rvc.realtime]\ncrossfade_ms = 0\nsola_search_ms = 5\n",
            "[rvc.realtime]\ncrossfade_ms = 0\nsola_search_ms = 0\n",
            "[rvc.realtime]\ncrossfade_ms = 50\nsola_search_ms = 0\n",
            "[rvc.realtime]\ncrossfade_ms = 0\nsola_search_ms = 10\n",
        ];
        for text in rejected {
            let config: Config = toml::from_str(text).unwrap();
            let err = config.validate().unwrap_err().to_string();
            assert!(
                err.contains("must equal")
                    && err.contains("fixed at the maximum overlap")
                    && err.contains("not implemented"),
                "应说明 overlap 是定值而非范围：{text}（实际 {err}）"
            );
        }

        // 上限本身即定值：50 + 10 = 60 是唯一合法组合。
        let config: Config =
            toml::from_str("[rvc.realtime]\ncrossfade_ms = 50\nsola_search_ms = 10\n").unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(
            u64::from(REALTIME_CROSSFADE_MS_MAX + REALTIME_SOLA_SEARCH_MS_MAX),
            u64::from(REALTIME_OVERLAP_MS_MAX),
            "两项上限之和必须等于锁定的 overlap，否则定值无解"
        );
    }

    /// 跨语言对拍：配置入口的默认值、上限与整除模数必须等于单一来源
    /// `tools/model/realtime_chain_params.json`。C++ 镜像由
    /// `tests/test_rvc_realtime_params.cpp` 读同一份 JSON 比对。
    #[test]
    fn realtime_knobs_mirror_params_json() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tools/model/realtime_chain_params.json"
        );
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("读不到实时参数单一来源 {path}: {err}"));
        let params: serde_json::Value = serde_json::from_str(&text).unwrap();
        let geometry = &params["geometry"];
        let contract = &params["model_contract"];
        let num = |value: &serde_json::Value| value.as_u64().expect("JSON 数值键");

        let defaults = RvcRealtimeConfig::default();
        assert_eq!(
            u64::from(defaults.crossfade_ms),
            num(&geometry["crossfade_ms"])
        );
        assert_eq!(
            u64::from(defaults.sola_search_ms),
            num(&geometry["sola_search_ms"])
        );
        assert_eq!(
            u64::from(defaults.startup_delay_ms),
            num(&geometry["startup_delay_ms"])
        );
        assert_eq!(
            defaults.skip_silence,
            geometry["skip_silence"]
                .as_bool()
                .expect("JSON skip_silence")
        );

        let samples_per_ms = num(&contract["output_sample_rate"]) / 1000;
        let crossfade_max_out = num(&geometry["crossfade_max_out"]);
        let sola_max_out = num(&geometry["sola_search_max_out"]);
        assert_eq!(
            u64::from(REALTIME_CROSSFADE_MS_MAX),
            crossfade_max_out / samples_per_ms
        );
        assert_eq!(
            u64::from(REALTIME_SOLA_SEARCH_MS_MAX),
            sola_max_out / samples_per_ms
        );
        assert_eq!(
            u64::from(REALTIME_OVERLAP_MS_MAX),
            (crossfade_max_out + sola_max_out) / samples_per_ms
        );
        assert_eq!(
            u64::from(REALTIME_STARTUP_DELAY_MS_MAX),
            num(&geometry["startup_delay_max_ms"])
        );
        assert_eq!(
            u64::from(REALTIME_MODEL_FRAME_RATE),
            num(&contract["feature_frame_rate"])
        );
        assert_eq!(
            u64::from(REALTIME_OVERLAP_MS_QUANTUM),
            1000 / num(&contract["feature_frame_rate"])
        );
        // 启动缓冲网格跨语言对拍：两侧模数各由本侧帧长推导（Rust 的 FRAME_MS、
        // C++ 的 MOZART_OUTPUT_FRAME_MS），各自断言都是自等式。这里直接读 C 头
        // 常量钉住，任一侧帧长漂移即失败（C++ 侧见 tests/test_rvc_realtime_params.cpp）。
        let header_path = concat!(env!("CARGO_MANIFEST_DIR"), "/include/mozart/frame.h");
        let header = std::fs::read_to_string(header_path)
            .unwrap_or_else(|err| panic!("读不到帧契约头 {header_path}: {err}"));
        let contract_frame_ms = header
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix("#define MOZART_OUTPUT_FRAME_MS")?;
                rest.split_whitespace().next()?.parse::<u32>().ok()
            })
            .expect("frame.h 缺少 MOZART_OUTPUT_FRAME_MS");
        assert_eq!(u64::from(crate::FRAME_MS), u64::from(contract_frame_ms));
        assert_eq!(
            u64::from(REALTIME_STARTUP_MS_QUANTUM),
            u64::from(contract_frame_ms)
        );

        // JSON 的出厂默认必须自身合法（含整除），否则部署即被入口拒绝。
        let config = Config {
            rvc: RvcConfig {
                realtime: defaults,
                ..RvcConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_ok(), "JSON 默认旋钮必须通过入口校验");
    }
}
