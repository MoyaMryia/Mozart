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
}

impl Default for PreConfig {
    fn default() -> Self {
        Self {
            socket: "pre.sock".to_string(),
            capture_device: "default".to_string(),
            playback_device: "default".to_string(),
            capture_queue_frames: 8,
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

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            pre_bin: PathBuf::from("mozart-pre"),
            rvc_bin: PathBuf::from("mozart-rvc"),
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

        let unknown_pre_key: Result<Config, _> = toml::from_str("[pre]\nunknown_key = 1\n");
        assert!(unknown_pre_key.is_err());
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
}
