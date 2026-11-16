//! 模型预设编排（SPEC-11）：把会话参数按模型持久化，并在模型切换时载入。
//! 载入分两步：resolve 读并校验预设，commit 一次原子写五键到 TOML 基线 →
//! 重建生效配置 → `apply_rvc_session_params_locked`。LoadModel 夹在两步之间，
//! 预设损坏与模型加载失败都发生在写入前，不留参数错位。
//! 锁顺序：transition → 预设串行锁 → toml 锁 → state 文件；调用方若已持
//! transition（路由/对账路径），必须用 `_locked` 变体。

use crate::runtime::SharedConfig;
use crate::services::{self, Services};
use crate::state::{lock_state, State};
use crate::state_file::{self, StateFile};
use mozart_core::config::{self, Config, RvcSessionValues};
use mozart_core::ctl::schema::{PresetApplyData, PresetListData};
use mozart_core::ctl;
use mozart_core::preset::{self, PresetError, PRESET_NAME};
use std::path::PathBuf;
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
            lock_state(&self.state)
                .push_degraded(format!("{}; using live params", unresolvable_reason(model_id)));
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
        config::write_rvc_session_baseline(path, values)
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
        let mut fresh = Config::load_from(Some(path))
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
        fresh.rvc.model_id = old.rvc.model_id.clone();
        let overrides = self
            .state_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .overrides()
            .clone();
        state_file::overlay(&mut fresh, &overrides);
        services::apply_rvc_session_params_locked(&self.state, &self.services, &old.rvc, &fresh.rvc)?;
        self.config.replace(fresh);
        *applied = Some(model_id.to_string());
        lock_state(&self.state).clear_degraded_prefix(&unresolvable_reason(model_id));
        Ok(self.config.snapshot())
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
