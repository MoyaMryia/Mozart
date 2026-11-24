//! 运行时状态文件（SPEC-04）：TOML 基线之上的运行时覆盖快照，daemon 统一管。
//! JSON 键值对（如 {"bridge.translate_enabled": false}），每次改动原子写
//! （临时文件 + rename），崩溃也保留。存档快照不是活权威：启动叠加恢复一次，
//! 运行时不对账；桥只持有内存活状态，启动与桥重启时由 daemon 恢复一次。

use mozart_core::config::{atomic_write, is_state_key, Config};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

pub struct StateFile {
    path: PathBuf,
    overrides: BTreeMap<String, Value>,
}

impl StateFile {
    /// 启动读回。缺失按空覆盖；损坏报错停启动（不静默丢运行时改动）。
    /// 入口校验：键必须在状态键集合内，值必须是 bool（v1 状态键均为开关）。
    pub fn load(path: PathBuf) -> io::Result<Self> {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    overrides: BTreeMap::new(),
                });
            }
            Err(err) => return Err(err),
        };
        let overrides: BTreeMap<String, Value> = serde_json::from_str(&text)
            .map_err(|err| io::Error::other(format!("parse {}: {err}", path.display())))?;
        for (key, value) in &overrides {
            if !is_state_key(key) {
                return Err(io::Error::other(format!(
                    "state file {}: unknown key {key}",
                    path.display()
                )));
            }
            if !value.is_boolean() {
                return Err(io::Error::other(format!(
                    "state file {}: key {key} expects a bool",
                    path.display()
                )));
            }
        }
        Ok(Self { path, overrides })
    }

    pub fn overrides(&self) -> &BTreeMap<String, Value> {
        &self.overrides
    }

    /// 改一个键并原子落盘。键集合由调用方保证（daemon 只写字幕开关）。
    pub fn set_key(&mut self, key: &str, value: Value) -> io::Result<()> {
        self.overrides.insert(key.to_string(), value);
        // The bridge already applied the switch; keep its live value even if saving fails.
        self.save(&self.overrides)
    }

    /// 清该键的状态覆盖（config set 运行时键后调用）；不在则不动盘。
    pub fn remove_key(&mut self, key: &str) -> io::Result<()> {
        if self.overrides.contains_key(key) {
            let mut next = self.overrides.clone();
            next.remove(key);
            self.save(&next)?;
            self.overrides = next;
        }
        Ok(())
    }

    /// config save 后清空：删文件（缺失视为已清）。
    pub fn clear(&mut self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        self.overrides.clear();
        Ok(())
    }

    fn save(&self, overrides: &BTreeMap<String, Value>) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(overrides).map_err(io::Error::other)?;
        atomic_write(&self.path, &text).map_err(|err| io::Error::other(err.to_string()))
    }

    /// 启动恢复用的字幕开关覆盖（桥就绪后下发一次）。
    pub fn bridge_restore(&self) -> BridgeRestore {
        let get = |key: &str| self.overrides.get(key).and_then(Value::as_bool);
        BridgeRestore {
            translate: get("bridge.translate_enabled"),
            speech: get("bridge.speech_enabled"),
            speech_stop: None,
        }
    }
}

/// 叠加状态覆盖到基线，得到生效配置（启动时一次）。值类型由 load 校验。
pub fn overlay(config: &mut Config, overrides: &BTreeMap<String, Value>) {
    if let Some(value) = overrides
        .get("bridge.translate_enabled")
        .and_then(Value::as_bool)
    {
        config.bridge.translate_enabled = value;
    }
    if let Some(value) = overrides
        .get("bridge.speech_enabled")
        .and_then(Value::as_bool)
    {
        config.bridge.speech_enabled = value;
    }
}

/// 启动恢复待下发项（SPEC-04：恢复一次，运行时不对账）。
#[derive(Default)]
pub struct BridgeRestore {
    pub translate: Option<bool>,
    pub speech: Option<bool>,
    pub speech_stop: Option<SpeechStop>,
}

#[derive(Clone, Copy)]
pub enum SpeechStop {
    Withdraw,
    Fence(mozart_core::speech::StopFence),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mozart-state-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.json")
    }

    #[test]
    fn load_missing_is_empty_and_corrupt_fails() {
        let path = temp_path("missing");
        let state = StateFile::load(path.clone()).unwrap();
        assert!(state.overrides().is_empty());

        std::fs::write(&path, "{ not json").unwrap();
        assert!(StateFile::load(path.clone()).is_err());
        // 未知键与非 bool 值都拒绝。
        std::fs::write(&path, r#"{"daemon.rpc_timeout_ms": 1}"#).unwrap();
        assert!(StateFile::load(path.clone()).is_err());
        std::fs::write(&path, r#"{"bridge.translate_enabled": 1}"#).unwrap();
        assert!(StateFile::load(path).is_err());
    }

    #[test]
    fn set_key_persists_atomically_and_clear_removes() {
        let path = temp_path("set");
        let mut state = StateFile::load(path.clone()).unwrap();
        state.set_key("bridge.translate_enabled", json!(false)).unwrap();
        state.set_key("bridge.speech_enabled", json!(true)).unwrap();

        // 重新读回：落盘生效。
        let reloaded = StateFile::load(path.clone()).unwrap();
        assert_eq!(reloaded.overrides()["bridge.translate_enabled"], json!(false));
        assert_eq!(reloaded.overrides()["bridge.speech_enabled"], json!(true));

        // remove_key 只清指定键。
        state.remove_key("bridge.translate_enabled").unwrap();
        let reloaded = StateFile::load(path.clone()).unwrap();
        assert!(!reloaded.overrides().contains_key("bridge.translate_enabled"));
        assert!(reloaded.overrides().contains_key("bridge.speech_enabled"));

        // clear 删文件；再 load 为空。
        state.clear().unwrap();
        assert!(!path.exists());
        assert!(StateFile::load(path).unwrap().overrides().is_empty());
    }

    #[test]
    fn failed_remove_and_clear_keep_override_for_retry() {
        let path = temp_path("retry-remove");
        let mut state = StateFile::load(path.clone()).unwrap();
        let key = "bridge.speech_enabled";
        state.set_key(key, json!(false)).unwrap();
        let backup = path.with_extension("backup");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(state.remove_key(key).is_err());
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 2,
            "rename failure must not strand an atomic-write temporary file");
        assert!(state.clear().is_err());
        assert_eq!(state.bridge_restore().speech, Some(false));
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(backup, &path).unwrap();
        state.remove_key(key).unwrap();
        assert_eq!(StateFile::load(path.clone()).unwrap().bridge_restore().speech, None);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn overlay_and_bridge_restore_read_switches() {
        let path = temp_path("overlay");
        let mut state = StateFile::load(path).unwrap();
        state.set_key("bridge.translate_enabled", json!(false)).unwrap();
        let mut config = Config::default();
        assert!(config.bridge.translate_enabled);
        overlay(&mut config, state.overrides());
        assert!(!config.bridge.translate_enabled);
        assert!(config.bridge.speech_enabled, "未覆盖的键沿用基线");

        let restore = state.bridge_restore();
        assert_eq!(restore.translate, Some(false));
        assert_eq!(restore.speech, None);
    }
}