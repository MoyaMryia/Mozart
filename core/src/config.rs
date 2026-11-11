//! daemon 与 ctl 共用的运行配置：TOML 文件 + 环境变量覆盖 + 入口校验。

use serde::Deserialize;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub process: ProcessConfig,
    pub pre: EndpointConfig,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointConfig {
    pub socket: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RvcConfig {
    pub socket: String,
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
            pre: EndpointConfig {
                socket: "pre.sock".to_string(),
            },
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
        validate_socket_name(&self.rvc.socket, "rvc.socket")?;
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
        assert_eq!(config.daemon.poll_interval_ms, 1000);
    }

    #[test]
    fn toml_overrides_defaults() {
        let config: Config = toml::from_str(
            r#"
            [daemon]
            poll_interval_ms = 250
            [pre]
            socket = "audio.sock"
            "#,
        )
        .unwrap();
        assert_eq!(config.daemon.poll_interval_ms, 250);
        assert_eq!(config.pre_socket(), PathBuf::from("/run/mozart/audio.sock"));
        assert_eq!(config.rvc.socket, "rvc.sock");
    }

    #[test]
    fn invalid_values_are_rejected() {
        let zero_interval: Config = toml::from_str("[daemon]\npoll_interval_ms = 0\n").unwrap();
        assert!(zero_interval.validate().is_err());

        let path_socket: Config = toml::from_str("[pre]\nsocket = \"a/b\"\n").unwrap();
        assert!(path_socket.validate().is_err());

        let bad_pitch: Config = toml::from_str("[rvc]\npitch_shift = 13\n").unwrap();
        assert!(bad_pitch.validate().is_err());

        let bad_method: Config = toml::from_str("[rvc]\nf0_method = \"harvest\"\n").unwrap();
        assert!(bad_method.validate().is_err());

        let unknown: Result<Config, _> = toml::from_str("[daemon]\nunknown_key = 1\n");
        assert!(unknown.is_err());
    }
}
