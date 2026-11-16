//! 模型预设文件契约（SPEC-11）：`<模型目录>/presets/default.toml`。
//! 文件只含五个会话参数与 `version`；解析时复用 config 的会话参数校验。

use crate::config::{
    atomic_write, validate_rvc_session_values, ConfigError, RvcSessionValues,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// v1 单预设固定名；文件为 `presets/default.toml`。
pub const PRESET_NAME: &str = "default";
pub const PRESET_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetDocument {
    version: u32,
    pitch_shift: i32,
    index_rate: f64,
    rms_mix_rate: f64,
    protect: f64,
    f0_method: String,
}

impl PresetDocument {
    fn from_values(values: &RvcSessionValues) -> Self {
        Self {
            version: PRESET_VERSION,
            pitch_shift: values.pitch_shift,
            index_rate: values.index_rate,
            rms_mix_rate: values.rms_mix_rate,
            protect: values.protect,
            f0_method: values.f0_method.clone(),
        }
    }

    fn into_values(self) -> RvcSessionValues {
        RvcSessionValues {
            pitch_shift: self.pitch_shift,
            index_rate: self.index_rate,
            rms_mix_rate: self.rms_mix_rate,
            protect: self.protect,
            f0_method: self.f0_method,
        }
    }
}

#[derive(Debug)]
pub enum PresetError {
    Missing(PathBuf),
    Io { path: PathBuf, source: io::Error },
    Parse { path: PathBuf, source: toml::de::Error },
    Serialize(toml::ser::Error),
    Config(ConfigError),
    Invalid(String),
}

impl fmt::Display for PresetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PresetError::Missing(path) => write!(f, "preset not found: {}", path.display()),
            PresetError::Io { path, source } => write!(f, "preset {}: {source}", path.display()),
            PresetError::Parse { path, source } => {
                write!(f, "parse preset {}: {source}", path.display())
            }
            PresetError::Serialize(source) => write!(f, "serialize preset: {source}"),
            PresetError::Config(source) => write!(f, "preset config: {source}"),
            PresetError::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PresetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PresetError::Io { source, .. } => Some(source),
            PresetError::Parse { source, .. } => Some(source),
            PresetError::Serialize(source) => Some(source),
            PresetError::Config(source) => Some(source),
            PresetError::Missing(_) | PresetError::Invalid(_) => None,
        }
    }
}

pub fn preset_path(model_dir: &Path) -> PathBuf {
    model_dir
        .join("presets")
        .join(format!("{PRESET_NAME}.toml"))
}

/// 读 `default.toml`：入口校验版本与五键范围，缺失返回 `Missing`。
pub fn read_default(model_dir: &Path) -> Result<RvcSessionValues, PresetError> {
    let path = preset_path(model_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(PresetError::Missing(path));
        }
        Err(source) => return Err(PresetError::Io { path, source }),
    };
    let document: PresetDocument =
        toml::from_str(&text).map_err(|source| PresetError::Parse {
            path: path.clone(),
            source,
        })?;
    if document.version != PRESET_VERSION {
        return Err(PresetError::Invalid(format!(
            "preset {}: unsupported version {} (expected {PRESET_VERSION})",
            path.display(),
            document.version
        )));
    }
    let values = document.into_values();
    validate_rvc_session_values(&values)
        .map_err(|err| PresetError::Invalid(format!("preset {}: {err}", path.display())))?;
    Ok(values)
}

/// 首次加载自动建 `default.toml`（规范默认值）；已存在则只读校验。
pub fn ensure_default(model_dir: &Path) -> Result<RvcSessionValues, PresetError> {
    match read_default(model_dir) {
        Ok(values) => Ok(values),
        Err(PresetError::Missing(_)) => {
            let values = RvcSessionValues::default();
            write_default(model_dir, &values)?;
            Ok(values)
        }
        Err(err) => Err(err),
    }
}

/// 原子写 `default.toml`；模型目录必须已存在，只创建 `presets/` 子目录。
pub fn write_default(model_dir: &Path, values: &RvcSessionValues) -> Result<(), PresetError> {
    if !model_dir.is_dir() {
        return Err(PresetError::Invalid(format!(
            "model directory not found: {}",
            model_dir.display()
        )));
    }
    let path = preset_path(model_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| PresetError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    let text =
        toml::to_string(&PresetDocument::from_values(values)).map_err(PresetError::Serialize)?;
    atomic_write(&path, &text).map_err(PresetError::Config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mozart-preset-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_write_round_trips_with_contract_fields() {
        let dir = temp_dir("roundtrip");
        let values = RvcSessionValues::default();
        write_default(&dir, &values).unwrap();

        let text = std::fs::read_to_string(preset_path(&dir)).unwrap();
        for key in [
            "version = 1",
            "pitch_shift = 0",
            "index_rate = 0.0",
            "rms_mix_rate = 1.0",
            "protect = 0.33",
            "f0_method = \"rmvpe\"",
        ] {
            assert!(text.contains(key), "预设缺少 {key}：{text}");
        }
        assert_eq!(read_default(&dir).unwrap(), values);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_preset_is_reported_then_created_by_ensure() {
        let dir = temp_dir("missing");
        let err = read_default(&dir).unwrap_err();
        assert!(matches!(err, PresetError::Missing(_)), "{err}");

        let values = ensure_default(&dir).unwrap();
        assert_eq!(values, RvcSessionValues::default());
        assert!(preset_path(&dir).is_file());
        assert_eq!(ensure_default(&dir).unwrap(), values);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_requires_existing_model_dir() {
        let dir = temp_dir("no-model-dir");
        let model_dir = dir.join("missing");
        let err = write_default(&model_dir, &RvcSessionValues::default()).unwrap_err();
        assert!(err.to_string().contains("model directory not found"), "{err}");
        assert!(!model_dir.exists(), "不得自动创建模型目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rejects_unknown_missing_and_bad_version() {
        let dir = temp_dir("parse");
        let path = preset_path(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let cases = [
            (
                "version = 1\npitch_shift = 0\nindex_rate = 0.0\nrms_mix_rate = 1.0\nprotect = 0.33\nf0_method = \"rmvpe\"\nextra = 1\n",
                "unknown",
            ),
            (
                "version = 1\npitch_shift = 0\nindex_rate = 0.0\nrms_mix_rate = 1.0\nprotect = 0.33\n",
                "missing",
            ),
            (
                "version = 2\npitch_shift = 0\nindex_rate = 0.0\nrms_mix_rate = 1.0\nprotect = 0.33\nf0_method = \"rmvpe\"\n",
                "unsupported version 2",
            ),
        ];
        for (text, needle) in cases {
            std::fs::write(&path, text).unwrap();
            let err = read_default(&dir).unwrap_err();
            assert!(err.to_string().contains(needle), "应含 {needle}：{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_reuses_session_range_validation() {
        let dir = temp_dir("range");
        let path = preset_path(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "version = 1\npitch_shift = 13\nindex_rate = 0.0\nrms_mix_rate = 1.0\nprotect = 0.33\nf0_method = \"rmvpe\"\n",
        )
        .unwrap();
        let err = read_default(&dir).unwrap_err();
        assert!(err.to_string().contains("-12..=12"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
