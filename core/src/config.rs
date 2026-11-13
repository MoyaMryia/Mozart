//! daemon 与 ctl 共用的运行配置：TOML 文件 + 环境变量覆盖 + 入口校验。

use serde::Deserialize;
use std::fmt;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub process: ProcessConfig,
    pub pre: PreConfig,
    pub rvc: RvcConfig,
    pub asr: AsrConfig,
    pub tts: TtsConfig,
    pub bridge: BridgeConfig,
    pub file: FileConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
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
pub struct ProcessConfig {
    pub pre_bin: PathBuf,
    pub rvc_bin: PathBuf,
    pub asr_bin: PathBuf,
    pub tts_bin: PathBuf,
    pub bridge_bin: PathBuf,
    pub llama_bin: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreConfig {
    pub socket: String,
    /// ALSA 采集设备名（SPEC-02：部署配置经 --config 读取）。
    pub capture_device: String,
    /// ALSA 播放设备名。
    pub playback_device: String,
    /// 采集 → DSP 环形队列容量（帧），1..=1024。
    pub capture_queue_frames: u32,
    /// DSP 阶段开关（[pre.dsp] 段）。
    pub dsp: PreDspConfig,
}

/// DSP 阶段配置。开关默认沿用旧 main.c 的代码默认（生产脚本曾用 --no-denoise
/// 显式关闭、--gate/--agc/--limiter 显式开启）；禁用降噪时整级跳过，
/// 禁用整形级时逐位恒等（SPEC-02 已确认）。
#[derive(Debug, Clone, Deserialize)]
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
            playback_device: "default".to_string(),
            capture_queue_frames: 8,
            dsp: PreDspConfig::default(),
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
}

impl Default for RvcConfig {
    fn default() -> Self {
        Self {
            socket: "rvc.sock".to_string(),
            input_socket: "rvc-in.sock".to_string(),
            output_socket: "rvc-out.sock".to_string(),
            model_id: String::new(),
            f0_method: "rmvpe".to_string(),
            pitch_shift: 0,
            index_rate: 0.0,
            rms_mix_rate: 1.0,
            protect: 0.33,
            models_dir: String::new(),
            device: "cuda".to_string(),
            half: false,
            cache_limit: 2,
            hubert_path: String::new(),
            rmvpe_path: String::new(),
            realtime_hubert_path: String::new(),
            realtime_rmvpe_path: String::new(),
        }
    }
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            pre_bin: PathBuf::from("mozart-pre"),
            rvc_bin: PathBuf::from("mozart-rvc"),
            asr_bin: PathBuf::from("mozart-asr"),
            tts_bin: PathBuf::from("mozart-tts"),
            bridge_bin: PathBuf::from("mozart-bridge"),
            llama_bin: PathBuf::from("llama-server"),
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
            process: ProcessConfig::default(),
            pre: PreConfig::default(),
            rvc: RvcConfig::default(),
            asr: AsrConfig::default(),
            tts: TtsConfig::default(),
            bridge: BridgeConfig::default(),
            file: FileConfig::default(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::from("/run/mozart"),
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

#[derive(Debug)]
pub enum ConfigError {
    Read { path: PathBuf, source: io::Error },
    Parse { path: PathBuf, source: toml::de::Error },
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "read config {}: {source}", path.display())
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
        config.bridge.subtitles_path =
            expand_tilde(&config.bridge.subtitles_path, home.as_deref());
        if let Some(path) = &config.bridge.corrections_path {
            config.bridge.corrections_path = Some(expand_tilde(path, home.as_deref()));
        }
        config.file.storage_dir = expand_tilde(&config.file.storage_dir, home.as_deref());
        config.validate()?;
        Ok(config)
    }

    pub fn daemon_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.daemon.socket)
    }

    pub fn pre_socket(&self) -> PathBuf {
        self.daemon.run_dir.join(&self.pre.socket)
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
        if let Some(dir) = std::env::var_os("MOZART_RUN_DIR") {
            self.daemon.run_dir = PathBuf::from(dir);
        }
        if let Some(bin) = std::env::var_os("MOZART_PRE_BIN") {
            self.process.pre_bin = PathBuf::from(bin);
        }
        if let Some(bin) = std::env::var_os("MOZART_RVC_BIN") {
            self.process.rvc_bin = PathBuf::from(bin);
        }
        if let Some(bin) = std::env::var_os("MOZART_ASR_BIN") {
            self.process.asr_bin = PathBuf::from(bin);
        }
        if let Some(bin) = std::env::var_os("MOZART_TTS_BIN") {
            self.process.tts_bin = PathBuf::from(bin);
        }
        if let Some(bin) = std::env::var_os("MOZART_BRIDGE_BIN") {
            self.process.bridge_bin = PathBuf::from(bin);
        }
        if let Some(bin) = std::env::var_os("MOZART_LLAMA_BIN") {
            self.process.llama_bin = PathBuf::from(bin);
        }
        if let Ok(value) = std::env::var("MOZART_POLL_INTERVAL_MS") {
            self.daemon.poll_interval_ms = value.parse().map_err(|_| {
                ConfigError::Invalid("MOZART_POLL_INTERVAL_MS must be an integer".to_string())
            })?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let interval = self.daemon.poll_interval_ms;
        let rpc = self.daemon.rpc_timeout_ms;
        let start = self.daemon.start_timeout_ms;
        let backoff = self.daemon.restart_backoff_ms;
        let backoff_max = self.daemon.restart_backoff_max_ms;
        let stable = self.daemon.restart_stable_ms;
        if interval == 0 {
            return Err(ConfigError::Invalid(
                "daemon.poll_interval_ms must be greater than zero".to_string(),
            ));
        }
        if rpc == 0 {
            return Err(ConfigError::Invalid(
                "daemon.rpc_timeout_ms must be greater than zero".to_string(),
            ));
        }
        if start == 0 {
            return Err(ConfigError::Invalid(
                "daemon.start_timeout_ms must be greater than zero".to_string(),
            ));
        }
        if backoff == 0 || backoff_max < backoff {
            return Err(ConfigError::Invalid(
                "daemon.restart_backoff_max_ms must be at least restart_backoff_ms".to_string(),
            ));
        }
        if stable == 0 {
            return Err(ConfigError::Invalid(
                "daemon.restart_stable_ms must be greater than zero".to_string(),
            ));
        }
        validate_socket_name(&self.daemon.socket, "daemon.socket")?;
        validate_socket_name(&self.pre.socket, "pre.socket")?;
        validate_device_name(&self.pre.capture_device, "pre.capture_device")?;
        validate_device_name(&self.pre.playback_device, "pre.playback_device")?;
        if self.pre.capture_queue_frames == 0 || self.pre.capture_queue_frames > 1024 {
            return Err(ConfigError::Invalid(
                "pre.capture_queue_frames must be within 1..=1024".to_string(),
            ));
        }
        validate_socket_name(&self.rvc.socket, "rvc.socket")?;
        validate_socket_name(&self.rvc.input_socket, "rvc.input_socket")?;
        validate_socket_name(&self.rvc.output_socket, "rvc.output_socket")?;
        if self.rvc.f0_method != "rmvpe" {
            return Err(ConfigError::Invalid(
                "rvc.f0_method must be \"rmvpe\"".to_string(),
            ));
        }
        if !(-12..=12).contains(&self.rvc.pitch_shift) {
            return Err(ConfigError::Invalid(
                "rvc.pitch_shift must be within -12..=12".to_string(),
            ));
        }
        if !self.rvc.index_rate.is_finite() || !(0.0..=1.0).contains(&self.rvc.index_rate) {
            return Err(ConfigError::Invalid(
                "rvc.index_rate must be within 0.0..=1.0".to_string(),
            ));
        }
        if !self.rvc.rms_mix_rate.is_finite() || !(0.0..=1.0).contains(&self.rvc.rms_mix_rate) {
            return Err(ConfigError::Invalid(
                "rvc.rms_mix_rate must be within 0.0..=1.0".to_string(),
            ));
        }
        if !self.rvc.protect.is_finite() || !(0.0..=0.5).contains(&self.rvc.protect) {
            return Err(ConfigError::Invalid(
                "rvc.protect must be within 0.0..=0.5".to_string(),
            ));
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
        if self.process.pre_bin.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "process.pre_bin must not be empty".to_string(),
            ));
        }
        if self.process.rvc_bin.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "process.rvc_bin must not be empty".to_string(),
            ));
        }
        if self.process.asr_bin.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "process.asr_bin must not be empty".to_string(),
            ));
        }
        if self.process.tts_bin.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "process.tts_bin must not be empty".to_string(),
            ));
        }
        if self.process.bridge_bin.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "process.bridge_bin must not be empty".to_string(),
            ));
        }
        if self.process.llama_bin.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "process.llama_bin must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

fn validate_socket_name(name: &str, field: &str) -> Result<(), ConfigError> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
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
fn validate_device_name(name: &str, field: &str) -> Result<(), ConfigError> {
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

/// 运行目录承载控制 socket，统一收敛到 0700；ctl 与 daemon 共用此定义，
/// 避免 ctl 抢先建目录导致权限策略失效。
pub fn ensure_run_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_confirmed_paths() {
        let config = Config::default();
        assert_eq!(config.daemon.run_dir, PathBuf::from("/run/mozart"));
        assert_eq!(config.daemon_socket(), PathBuf::from("/run/mozart/daemon.sock"));
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

        let stages_on: Config = toml::from_str(
            "[pre.dsp]\ngate = true\nagc = true\nlimiter = true\n",
        )
        .unwrap();
        assert!(stages_on.pre.dsp.gate);
        assert!(stages_on.pre.dsp.agc);
        assert!(stages_on.pre.dsp.limiter);
        assert!(stages_on.pre.dsp.denoise, "未写的键沿用默认");
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

        let huge_queue: Config =
            toml::from_str("[pre]\ncapture_queue_frames = 1025\n").unwrap();
        assert!(huge_queue.validate().is_err());

        let max_queue: Config =
            toml::from_str("[pre]\ncapture_queue_frames = 1024\n").unwrap();
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
        assert_eq!(startup.rvc.realtime_rmvpe_path, "/opt/mozart/rmvpe-realtime.onnx");
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
        assert_eq!(config.llama_socket(), PathBuf::from("/run/mozart/llama.sock"));

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
        assert_eq!(config.tts.reference_wav, PathBuf::from("/opt/mozart/voice.wav"));
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
        assert_eq!(config.bridge.llama_model, PathBuf::from("/opt/mozart/qwen.gguf"));
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
}
