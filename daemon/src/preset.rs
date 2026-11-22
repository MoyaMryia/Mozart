//! 模型预设编排（SPEC-11）：把会话参数按模型持久化，并在模型切换时载入。
//! 载入分两步：resolve 读并校验预设，commit 一次原子写五键到 TOML 基线 →
//! 重建生效配置 → `apply_rvc_session_params_locked`。LoadModel 夹在两步之间，
//! 预设损坏与模型加载失败都发生在写入前，不留参数错位。
//! 实时 `model use` 走 `commit_model_switch_locked`：额外写 `[rvc].model_id`
//! 基线并把生效配置的模型身份落为目标模型，起流失败按 `SwitchUndo` 整体回退。
//! 锁顺序：transition → 预设串行锁 → toml 锁 → state 文件；调用方若已持
//! transition（路由/对账路径），必须用 `_locked` 变体。

use crate::runtime::SharedConfig;
use crate::services::{self, Services};
use crate::state::{lock_state, State};
use crate::state_file::{self, StateFile};
use mozart_core::config::{self, write_rvc_model_baseline, Config, RvcSessionValues};
use mozart_core::ctl;
use mozart_core::ctl::schema::{PresetApplyData, PresetListData};
use mozart_core::preset::{self, PresetError, PRESET_NAME};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// daemon 侧唯一预设写者与载入编排入口。
pub struct PresetManager {
    config: SharedConfig,
    state: Arc<Mutex<State>>,
    services: Arc<Services>,
    state_file: Arc<Mutex<StateFile>>,
    config_path: Option<PathBuf>,
    toml_lock: Arc<Mutex<()>>,
    /// 当前 `[rvc]` 参数来自哪个模型的预设；daemon 启动为空，首次载入必读。
    applied: Mutex<Option<String>>,
}

/// resolve 步的结果：`Skip` 不写基线，`Ready` 的值等 LoadModel 成功后提交。
pub(crate) enum PresetResolution {
    /// 同模型已载入（保留运行时调参），或模型目录不可解析已降级用 live 参数。
    Skip,
    Ready(RvcSessionValues),
}

/// 换模型提交前的回退快照（不透明令牌，只由 `rollback_model_switch_locked` 消费）。
/// 起流失败时整体回退，不留「live 参数属目标模型、rvc 仍载旧模型」的错位。
pub(crate) struct SwitchUndo {
    live: Arc<Config>,
    applied: Option<String>,
    /// 提交前 TOML 基线的五键与 model_id；无配置文件时 None。取基线而非 live：
    /// 失败恢复可能留下尚未生效的基线，回退不得把它抹掉。
    baseline: Option<(RvcSessionValues, String)>,
}

impl PresetManager {
    pub fn new(
        config: SharedConfig,
        state: Arc<Mutex<State>>,
        services: Arc<Services>,
        state_file: Arc<Mutex<StateFile>>,
        config_path: Option<PathBuf>,
        toml_lock: Arc<Mutex<()>>,
    ) -> Self {
        Self {
            config,
            state,
            services,
            state_file,
            config_path,
            toml_lock,
            applied: Mutex::new(None),
        }
    }

    /// 模型切换载入第一步：目标模型与已载入预设不同才读并校验，不写基线、
    /// 不改生效配置与 applied。调用方不得持 transition 锁；LoadModel 成功后
    /// 用 `commit_for_model` 提交，两步之间不攥锁做 RPC。
    pub fn resolve_for_model(&self, model_id: &str) -> Result<PresetResolution, (String, String)> {
        let _transition = self.services.transition();
        self.resolve_for_model_locked(model_id)
    }

    /// 已持 transition 锁的路径（路由切换、对账）使用；避免重复加锁。
    pub(crate) fn resolve_for_model_locked(
        &self,
        model_id: &str,
    ) -> Result<PresetResolution, (String, String)> {
        let applied = self.lock_applied();
        if applied.as_deref() == Some(model_id) {
            return Ok(PresetResolution::Skip);
        }
        let config = self.config.snapshot();
        let Some(model_dir) = config.rvc_model_dir(model_id) else {
            // 旧配置可能只给 model_id、没有注册表或 models_dir：无法定位预设，
            // 用 live 参数继续，并留可查询降级原因；显式 preset 命令仍报错。
            lock_state(&self.state).push_degraded(format!(
                "{}; using live params",
                unresolvable_reason(model_id)
            ));
            return Ok(PresetResolution::Skip);
        };
        // 模型存在性归推理层（SPEC-10）：目录不存在时跳过载入，让 LoadModel 回
        // InvalidArgument「model not found」，预设层不预检、不抢报错权。
        if !model_dir.is_dir() {
            return Ok(PresetResolution::Skip);
        }
        let values = preset::ensure_default(&model_dir).map_err(reject)?;
        Ok(PresetResolution::Ready(values))
    }

    /// 模型切换载入第二步：写基线并应用，返回新的生效配置快照。
    /// 调用方不得持 transition 锁。
    pub fn commit_for_model(
        &self,
        model_id: &str,
        resolution: PresetResolution,
    ) -> Result<Arc<Config>, (String, String)> {
        let _transition = self.services.transition();
        self.commit_for_model_locked(model_id, resolution)
    }

    /// 已持 transition 锁的路径使用。两步之间放开过锁：重新判定 applied 并
    /// 取当前生效配置，不套用 resolve 时的旧快照。
    pub(crate) fn commit_for_model_locked(
        &self,
        model_id: &str,
        resolution: PresetResolution,
    ) -> Result<Arc<Config>, (String, String)> {
        let PresetResolution::Ready(values) = resolution else {
            return Ok(self.config.snapshot());
        };
        let mut applied = self.lock_applied();
        // 别的入口可能已载入同一模型：此时保留其 live 参数，不重复写基线。
        if applied.as_deref() == Some(model_id) {
            return Ok(self.config.snapshot());
        }
        let old = self.config.snapshot();
        self.write_and_apply(&mut applied, model_id, &values, &old)
    }

    /// 实时 `model use` 的第二步（LoadModel 已成功）：同一把 toml 锁内先写
    /// `[rvc].model_id` 基线，再写五键基线并记录 applied，生效配置的 model_id
    /// 一并落为目标模型。model_id 先写：它失败时五键与 applied 都未动。
    /// 预设跳过（同模型已载入或目录不可解析）时保留 live 参数，只落模型身份。
    /// 返回新生效配置与回退快照，起流失败由调用方交回滚。
    pub(crate) fn commit_model_switch_locked(
        &self,
        model_id: &str,
        resolution: PresetResolution,
    ) -> Result<(Arc<Config>, SwitchUndo), (String, String)> {
        let mut applied = self.lock_applied();
        let old = self.config.snapshot();
        let mut undo = SwitchUndo {
            live: old.clone(),
            applied: applied.clone(),
            baseline: None,
        };
        // resolve 已对同模型返回 Skip，且 transition 全程持有（applied 只在
        // 该锁内改写）：Ready 即意味着 applied 不是目标模型，无需再判。
        let values = match resolution {
            PresetResolution::Ready(values) => Some(values),
            PresetResolution::Skip => None,
        };
        let Some(path) = self.config_path.as_deref() else {
            // 无 TOML 基线（daemon 未带 --config）：只落生效配置的模型身份。
            return Ok((self.replace_model_id(&old, model_id), undo));
        };
        let _toml = self
            .toml_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let baseline = Config::load_from(Some(path))
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
        undo.baseline = Some((baseline.rvc.session_values(), baseline.rvc.model_id.clone()));
        write_rvc_model_baseline(path, model_id)
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
        let Some(values) = values else {
            return Ok((self.replace_model_id(&old, model_id), undo));
        };
        match self.write_session_and_apply(&mut applied, model_id, &values, &old, model_id, path) {
            Ok(fresh) => Ok((fresh, undo)),
            Err(err) => {
                // 五键写入或重建失败：model_id 基线已落盘，回退它再报错，
                // 避免 TOML 指向目标模型而 live 与 applied 仍属旧模型。
                self.restore_baseline(path, &undo);
                Err(err)
            }
        }
    }

    /// 起流失败后按快照整体回退：TOML 基线、生效配置与 applied 都回到提交前。
    /// 调用方须已持 transition 锁；随后用旧模型旧参数恢复实时流。
    pub(crate) fn rollback_model_switch_locked(&self, undo: SwitchUndo) {
        let mut applied = self.lock_applied();
        if let Some(path) = self.config_path.as_deref() {
            let _toml = self
                .toml_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.restore_baseline(path, &undo);
        }
        self.config.replace((*undo.live).clone());
        *applied = undo.applied;
    }

    /// `preset use`：用当前模型的 default.toml 重置 live 参数，丢弃未保存调参。
    pub fn use_default(&self) -> Result<PresetApplyData, (String, String)> {
        let _transition = self.services.transition();
        let mut applied = self.lock_applied();
        let model_id = self.current_model()?;
        let old = self.config.snapshot();
        let model_dir = self.model_dir(&old, &model_id)?;
        let values = preset::ensure_default(&model_dir).map_err(reject)?;
        self.write_and_apply(&mut applied, &model_id, &values, &old)?;
        Ok(PresetApplyData {
            model_id,
            preset: PRESET_NAME.to_string(),
            values,
        })
    }

    /// `preset save`：当前 live 五参数原子写当前模型 default.toml，不改 TOML 基线。
    pub fn save_current(&self) -> Result<PresetApplyData, (String, String)> {
        let _transition = self.services.transition();
        let applied = self.lock_applied();
        let model_id = self.current_model()?;
        let config = self.config.snapshot();
        // 先解析目录：不可解析时给出配置指引，而不是让来源校验先回一条
        // 「先 preset use」的死路建议（use 同样会因目录不可解析失败）。
        let model_dir = self.model_dir(&config, &model_id)?;
        // 参数来源必须已知且与 rvc 已加载模型一致：来源为空（daemon 启动时
        // rvc 已有模型在跑，未经历载入）或属于别的模型时，live 参数可能来自
        // 其他模型，保存会覆盖错预设。
        match applied.as_deref() {
            Some(applied_model) if applied_model == model_id => {}
            Some(applied_model) => {
                return Err((
                    ctl::CODE_NOT_READY.to_string(),
                    format!(
                        "live params belong to model '{applied_model}', but rvc has '{model_id}' loaded; run preset use to reload the loaded model's preset"
                    ),
                ));
            }
            None => {
                return Err((
                    ctl::CODE_NOT_READY.to_string(),
                    format!(
                        "live param source is unknown for model '{model_id}'; run preset use to load its preset before saving"
                    ),
                ));
            }
        }
        let values = config.rvc.session_values();
        preset::write_default(&model_dir, &values).map_err(reject)?;
        Ok(PresetApplyData {
            model_id,
            preset: PRESET_NAME.to_string(),
            values,
        })
    }

    /// `preset list`：当前模型预设值与 live 生效值及 dirty 标记。只读；
    /// 预设缺失明确报错，不补建（补建只发生在载入路径）。
    pub fn list(&self) -> Result<PresetListData, (String, String)> {
        let _transition = self.services.transition();
        let _applied = self.lock_applied();
        let model_id = self.current_model()?;
        let config = self.config.snapshot();
        let model_dir = self.model_dir(&config, &model_id)?;
        let preset_values = match preset::read_default(&model_dir) {
            Ok(values) => values,
            Err(PresetError::Missing(path)) => {
                return Err((
                    ctl::CODE_NOT_READY.to_string(),
                    format!(
                        "preset missing: {}; load the model to create it",
                        path.display()
                    ),
                ));
            }
            Err(err) => return Err(reject(err)),
        };
        let live_values = config.rvc.session_values();
        Ok(PresetListData {
            model_id,
            preset: PRESET_NAME.to_string(),
            dirty: preset_values != live_values,
            preset_values,
            live_values,
        })
    }

    fn write_and_apply(
        &self,
        applied: &mut Option<String>,
        model_id: &str,
        values: &RvcSessionValues,
        old: &Config,
    ) -> Result<Arc<Config>, (String, String)> {
        let Some(path) = self.config_path.as_deref() else {
            return Err((
                ctl::CODE_NOT_READY.to_string(),
                "preset load requires a config file baseline; start daemon with --config"
                    .to_string(),
            ));
        };
        let _toml = self
            .toml_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 载入不改模型身份：生效配置保留当前 model_id（SPEC-11 边界）。
        let live_model_id = old.rvc.model_id.clone();
        self.write_session_and_apply(applied, model_id, values, old, &live_model_id, path)
    }

    /// 五键基线写入 → 重建生效配置 → 编排 → 替换 → 记录 applied。
    /// 调用方须已持 toml 锁。`live_model_id` 是生效配置要保留的模型身份：
    /// 预设载入保留当前值，实时换模型落为目标模型。
    fn write_session_and_apply(
        &self,
        applied: &mut Option<String>,
        model_id: &str,
        values: &RvcSessionValues,
        old: &Config,
        live_model_id: &str,
        path: &Path,
    ) -> Result<Arc<Config>, (String, String)> {
        config::write_rvc_session_baseline(path, values)
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
        let mut fresh = Config::load_from(Some(path))
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
        crate::runtime::preserve_live_pre_state(&mut fresh, old);
        fresh.rvc.model_id = live_model_id.to_string();
        let overrides = self
            .state_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .overrides()
            .clone();
        state_file::overlay(&mut fresh, &overrides);
        services::apply_rvc_session_params_locked(
            &self.state,
            &self.services,
            &old.rvc,
            &fresh.rvc,
        )?;
        self.config.replace(fresh);
        *applied = Some(model_id.to_string());
        lock_state(&self.state).clear_degraded_prefix(&unresolvable_reason(model_id));
        Ok(self.config.snapshot())
    }

    /// 只落模型身份，保留 live 会话参数（预设跳过或无 TOML 基线时）。
    fn replace_model_id(&self, old: &Config, model_id: &str) -> Arc<Config> {
        let mut fresh = old.clone();
        fresh.rvc.model_id = model_id.to_string();
        self.config.replace(fresh);
        self.config.snapshot()
    }

    /// 写回提交前的 TOML 基线。写回失败记持久降级原因：TOML 可能仍指向目标
    /// 模型，重启会读到它，与 live 分歧必须可查询，不静默。
    fn restore_baseline(&self, path: &Path, undo: &SwitchUndo) {
        let Some((values, model_id)) = undo.baseline.as_ref() else {
            return;
        };
        let result = config::write_rvc_session_baseline(path, values)
            .and_then(|()| write_rvc_model_baseline(path, model_id));
        if let Err(err) = result {
            lock_state(&self.state).push_degraded(format!(
                "preset: model switch baseline restore: {err}; TOML baseline may still name the target model"
            ));
        }
    }

    fn current_model(&self) -> Result<String, (String, String)> {
        self.services.rvc_status()?.model_id.ok_or_else(|| {
            (
                ctl::CODE_NOT_READY.to_string(),
                "no model loaded; preset requires a loaded model".to_string(),
            )
        })
    }

    fn model_dir(&self, config: &Config, model_id: &str) -> Result<PathBuf, (String, String)> {
        config.rvc_model_dir(model_id).ok_or_else(|| {
            (
                ctl::CODE_NOT_READY.to_string(),
                format!(
                    "cannot resolve model directory for '{model_id}'; configure [[models]] or [rvc].models_dir"
                ),
            )
        })
    }

    fn lock_applied(&self) -> MutexGuard<'_, Option<String>> {
        self.applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 模型目录不可解析的降级原因前缀：记录与清除共用，改一处即两边同步。
fn unresolvable_reason(model_id: &str) -> String {
    format!("preset: cannot resolve model directory for '{model_id}'")
}

fn reject(err: PresetError) -> (String, String) {
    // 客户端体为空，坏的是服务端资产文件或 IO：一律 Internal，不是请求参数错。
    // Missing 不会到这里：ensure_default 吸收它，list 单独回 NotReady。
    (ctl::CODE_INTERNAL.to_string(), err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 回退时 TOML 写不回去：必须留可查询的持久降级原因，不静默——基线可能仍
    /// 指向目标模型，重启会读到它与 live 分歧。用基线目录只读确定性构造写失败
    /// （atomic_write 的临时文件创建被拒），不需要写失败注入旋钮。
    #[test]
    fn rollback_records_queryable_degraded_when_baseline_write_fails() {
        // root 绕过目录写权限位，只读目录拦不住写，本用例无法确定性构造失败。
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("跳过：root 下目录权限位不拦写，无法构造基线写回失败");
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("mozart-preset-restore-{}", std::process::id()));
        // 上次运行若在只读期间 panic，目录会留在 0o555，先恢复再删。
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // 提交后的基线：目标模型 + 目标预设参数。
        std::fs::write(&path, "[rvc]\nmodel_id = \"model-b\"\npitch_shift = 7\n").unwrap();

        let config = SharedConfig::new(Config::default());
        let state = Arc::new(Mutex::new(State::new()));
        let services = Arc::new(Services::new(&config));
        let state_file = Arc::new(Mutex::new(StateFile::load(dir.join("state.json")).unwrap()));
        let presets = PresetManager::new(
            config.clone(),
            state.clone(),
            services,
            state_file,
            Some(path.clone()),
            Arc::new(Mutex::new(())),
        );

        let mut live = Config::default();
        live.rvc.model_id = "model-b".to_string();
        live.rvc.pitch_shift = 7;
        config.replace(live);
        *presets.lock_applied() = Some("model-b".to_string());

        let mut old = Config::default();
        old.rvc.model_id = "model-a".to_string();
        old.rvc.pitch_shift = 3;
        let undo = SwitchUndo {
            live: Arc::new(old),
            applied: Some("model-a".to_string()),
            baseline: Some((
                RvcSessionValues {
                    pitch_shift: 3,
                    ..RvcSessionValues::default()
                },
                "model-a".to_string(),
            )),
        };

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        presets.rollback_model_switch_locked(undo);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let degraded = lock_state(&state).status().degraded;
        assert!(
            degraded.iter().any(|reason| {
                reason.contains("preset: model switch baseline restore")
                    && reason.contains("TOML baseline may still name the target model")
            }),
            "写回失败必须留可查询降级原因：{degraded:?}"
        );
        // 写回失败不阻断回退本身：生效配置与 applied 仍回到切换前。
        let snapshot = config.snapshot();
        assert_eq!(snapshot.rvc.model_id, "model-a");
        assert_eq!(snapshot.rvc.pitch_shift, 3);
        assert_eq!(presets.lock_applied().as_deref(), Some("model-a"));
        // 基线确实没写回去，与降级原因描述一致。
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model_id = \"model-b\""), "{text}");
        assert!(text.contains("pitch_shift = 7"), "{text}");

        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
