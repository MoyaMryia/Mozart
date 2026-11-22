//! 子服务控制：连接 pre/rvc/asr/tts/bridge、下发路由与静音、转发字幕开关、轮询状态并对账。
//! 对账以 daemon 的期望状态为准，切路由与轮询共用同一把 transition 锁。
//! llama-server 无控制协议，不建 ServiceLink，只由 supervisor 监管进程。

use crate::preset::PresetManager;
use crate::runtime::SharedConfig;
use crate::state::{lock_state, State};
use crate::state_file::BridgeRestore;
use mozart_core::config::{Config, RvcConfig};
use mozart_core::ctl::schema::{
    Activity, AsrStatus, BridgeStatus, EnhanceStatus, ModelEntry, PreStatus, Response, Route,
    RvcStatus, TtsStatus,
};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::Duration;

pub struct Services {
    enhance: Mutex<Option<ServiceLink>>,
    config: SharedConfig,
    pre: Mutex<ServiceLink>,
    rvc: Mutex<ServiceLink>,
    asr: Mutex<ServiceLink>,
    tts: Mutex<ServiceLink>,
    bridge: Mutex<ServiceLink>,
    transition: Mutex<()>,
}

impl Services {
    pub fn new(config: &SharedConfig) -> Self {
        Self {
            enhance: Mutex::new(config.snapshot().enhance.as_ref().map(|_| {
                ServiceLink::new(
                    ctl::ENHANCE_HELLO,
                    config.snapshot().enhance_socket(),
                    config.clone(),
                )
            })),
            config: config.clone(),
            pre: Mutex::new(ServiceLink::new(
                ctl::PRE_HELLO,
                config.snapshot().pre_socket(),
                config.clone(),
            )),
            rvc: Mutex::new(ServiceLink::new(
                ctl::RVC_HELLO,
                config.snapshot().rvc_socket(),
                config.clone(),
            )),
            asr: Mutex::new(ServiceLink::new(
                ctl::ASR_HELLO,
                config.snapshot().asr_socket(),
                config.clone(),
            )),
            tts: Mutex::new(ServiceLink::new(
                ctl::TTS_HELLO,
                config.snapshot().tts_socket(),
                config.clone(),
            )),
            bridge: Mutex::new(ServiceLink::new(
                ctl::BRIDGE_HELLO,
                config.snapshot().bridge_socket(),
                config.clone(),
            )),
            transition: Mutex::new(()),
        }
    }

    pub fn refresh_enhance(&self, config: &Config) {
        *self
            .enhance
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = config.enhance.as_ref().map(|_| {
            ServiceLink::new(
                ctl::ENHANCE_HELLO,
                config.enhance_socket(),
                self.config.clone(),
            )
        });
    }

    fn pre(&self) -> MutexGuard<'_, ServiceLink> {
        self.pre
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn rvc(&self) -> MutexGuard<'_, ServiceLink> {
        self.rvc
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn asr(&self) -> MutexGuard<'_, ServiceLink> {
        self.asr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn tts(&self) -> MutexGuard<'_, ServiceLink> {
        self.tts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn bridge(&self) -> MutexGuard<'_, ServiceLink> {
        self.bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn transition(&self) -> MutexGuard<'_, ()> {
        self.transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 调用方持 transition 锁；只在 pre 确认成功后提交共享配置。
    pub(crate) fn set_dsp(
        &self,
        dsp: &mozart_core::config::PreDspConfig,
    ) -> Result<(), (String, String)> {
        let body = serde_json::to_vec(dsp)
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
        let data = self
            .pre()
            .call_ok(ctl::PRE_SET_DSP, &body)
            .map_err(CallError::into_rejection)?;
        validate_dsp_echo(data.data, dsp)
    }

    /// 调用方持 transition 锁；只在 pre 确认热切换成功后由调用方更新内存生效设备。
    pub(crate) fn set_device(
        &self,
        request: &mozart_core::ctl::schema::DeviceUseRequest,
    ) -> Result<Value, (String, String)> {
        let body = serde_json::to_vec(request)
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?;
        let data = self
            .pre()
            .call_ok(ctl::PRE_SET_DEVICE, &body)
            .map_err(CallError::into_rejection)?;
        Ok(data.data)
    }

    // ---- rvc 控制入口：实时路由与文件数据面共用一条链路 ----
    // 文件 worker 在独立线程调用这些入口，不持 transition 锁：GPU 归 File
    // 期间 route_set 只动意图、reconcile 不碰 rvc（见下），两者天然互斥。
    // 单次 RPC 仍由 rvc 链路锁与每秒轮询串行。

    /// LoadModel：返回 StartStream 必填的 model_handle。
    pub fn load_model(&self, model_id: &str) -> Result<u64, (String, String)> {
        self.load_model_handle(model_id)
            .map_err(CallError::into_rejection)
    }

    /// StartStream kind=file（D1）：total_frames 作整段结束信号。
    /// 数据面复用 [rvc] 默认 input_socket/output_socket（SPEC-03 状态门控
    /// 交接）：调用前必须先经 pre_set_file_mode(true) 让 pre 停发并释放
    /// 输出 socket，daemon 再绑定同一输出 socket。
    /// 与实时不同，Busy 不吞：已有活动流说明仲裁失守，必须失败带原因。
    /// total_samples 为解码真实采样数（尾帧补零前），rvc 攒帧后据此裁掉零尾。
    pub fn file_start_stream(
        &self,
        config: &Config,
        model_handle: u64,
        total_frames: u32,
        total_samples: u64,
    ) -> Result<(), (String, String)> {
        let mut body = stream_body(&config.rvc, model_handle);
        body["kind"] = json!("file");
        body["total_frames"] = json!(total_frames);
        body["total_samples"] = json!(total_samples);
        let text = body.to_string();
        self.rvc()
            .call_ok(ctl::RVC_START_STREAM, text.as_bytes())
            .map(|_| ())
            .map_err(CallError::into_rejection)
    }

    /// 文件接管开关（PRE_SET_FILE_MODE）：active=true 让 pre 暂停 rvc AI
    /// 发送并释放输出 socket（pre 确认交接完成才应答）；active=false 恢复。
    /// 文件 worker 在起流前/收尾后调用，与实时路由切换经 transition 锁互斥。
    pub fn pre_set_file_mode(&self, active: bool) -> Result<(), (String, String)> {
        let body = json!({"active": active}).to_string();
        self.pre()
            .call_ok(ctl::PRE_SET_FILE_MODE, body.as_bytes())
            .map(|_| ())
            .map_err(CallError::into_rejection)
    }

    /// SetParams：实时流活动期间在下一推理块边界无中断更新会话参数与 skip_silence。
    pub fn set_rvc_params(&self, rvc: &RvcConfig) -> Result<(), (String, String)> {
        let body = set_params_body(rvc).to_string();
        self.rvc()
            .call_ok(ctl::RVC_SET_PARAMS, body.as_bytes())
            .map(|_| ())
            .map_err(CallError::into_rejection)
    }

    /// StopStream：实时切回 direct 与文件收尾/取消共用同一命令。
    /// 文件模式停在下一个推理检查点，可能超过 rpc_timeout_ms；超时按降级
    /// 处理（C++ 侧仍会停流），调用方不得因此卡住。
    pub fn stop_stream(&self) -> Result<(), (String, String)> {
        self.stop_stream_inner().map_err(CallError::into_rejection)
    }

    /// rvc 状态快照：文件 infer 段读 blocks、提交缺省模型解析与实时切换共用。
    pub fn rvc_status(&self) -> Result<RvcStatus, (String, String)> {
        let response = self
            .rvc()
            .call_ok(ctl::RVC_GET_STATUS, b"{}")
            .map_err(CallError::into_rejection)?;
        serde_json::from_value::<RvcStatus>(response.data)
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))
    }

    fn load_model_handle(&self, model_id: &str) -> Result<u64, CallError> {
        let body = json!({"model_id": model_id}).to_string();
        let response = self.rvc().call_ok(ctl::RVC_LOAD_MODEL, body.as_bytes())?;
        response
            .data
            .get("model_handle")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| {
                CallError::Protocol("LoadModel response missing model_handle".to_string())
            })
    }

    fn stop_stream_inner(&self) -> Result<(), CallError> {
        self.rvc().call_ok(ctl::RVC_STOP_STREAM, b"{}").map(|_| ())
    }
}

/// SetParams 载荷（SPEC-03）：五项会话参数加实时静音跳过开关。
fn set_params_body(rvc: &RvcConfig) -> Value {
    json!({
        "pitch_shift": rvc.pitch_shift,
        "index_rate": rvc.index_rate,
        "rms_mix_rate": rvc.rms_mix_rate,
        "protect": rvc.protect,
        "f0_method": rvc.f0_method,
        "skip_silence": rvc.realtime.skip_silence,
    })
}

/// StartStream 会话参数（SPEC-03 参数边界表）：实时与文件共用 `[rvc]` 一份。
/// kind 与文件批处理字段由调用方补齐；实时流另带 `[rvc.realtime]` 旋钮。
fn stream_body(rvc: &RvcConfig, model_handle: u64) -> Value {
    let mut body = set_params_body(rvc);
    body["model_handle"] = json!(model_handle);
    body["crossfade_ms"] = json!(rvc.realtime.crossfade_ms);
    body["sola_search_ms"] = json!(rvc.realtime.sola_search_ms);
    body["startup_delay_ms"] = json!(rvc.realtime.startup_delay_ms);
    body
}

enum CallError {
    Transport(io::Error),
    Remote(Response),
    Protocol(String),
}

impl CallError {
    fn into_rejection(self) -> (String, String) {
        match self {
            CallError::Transport(err) => (ctl::CODE_NOT_READY.to_string(), err.to_string()),
            CallError::Remote(response) => (response.code, response.reason),
            CallError::Protocol(message) => (ctl::CODE_INTERNAL.to_string(), message),
        }
    }

    fn describe(&self) -> String {
        match self {
            CallError::Transport(err) => err.to_string(),
            CallError::Remote(response) => format!("{}: {}", response.code, response.reason),
            CallError::Protocol(message) => message.clone(),
        }
    }
}

struct ServiceLink {
    hello: u16,
    socket: PathBuf,
    /// RPC 超时是运行时键：每次调用读最新生效值（SPEC-04）。
    config: SharedConfig,
    conn: Option<Seqpacket>,
    next_request: u32,
}

impl ServiceLink {
    fn new(hello: u16, socket: PathBuf, config: SharedConfig) -> Self {
        Self {
            hello,
            socket,
            config,
            conn: None,
            next_request: 0,
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.config.snapshot().daemon.rpc_timeout_ms)
    }

    fn call_ok(&mut self, type_: u16, body: &[u8]) -> Result<Response, CallError> {
        let response = self.call(type_, body)?;
        if response.status != "ok" {
            return Err(CallError::Remote(response));
        }
        Ok(response)
    }

    fn call(&mut self, type_: u16, body: &[u8]) -> Result<Response, CallError> {
        if self.conn.is_none() {
            if let Err(err) = self.connect_and_hello() {
                self.conn = None;
                return Err(CallError::Transport(err));
            }
        }
        match self.roundtrip(type_, body) {
            Ok(response) => Ok(response),
            Err(err) => {
                self.conn = None;
                Err(CallError::Transport(err))
            }
        }
    }

    fn connect_and_hello(&mut self) -> io::Result<()> {
        let conn = Seqpacket::connect_timeout(&self.socket, Some(self.timeout()))?;
        self.conn = Some(conn);
        let response = match self.roundtrip(self.hello, b"{}") {
            Ok(response) => response,
            Err(err) => {
                self.conn = None;
                return Err(err);
            }
        };
        if response.status != "ok" {
            self.conn = None;
            return Err(io::Error::other(format!(
                "hello rejected: {} ({})",
                response.reason, response.code
            )));
        }
        Ok(())
    }

    fn roundtrip(&mut self, type_: u16, body: &[u8]) -> io::Result<Response> {
        let conn = self
            .conn
            .as_ref()
            .expect("control link must be connected before roundtrip");
        self.next_request = self.next_request.wrapping_add(1);
        let request_id = self.next_request;
        conn.send_timeout(type_, request_id, body, Some(self.timeout()))?;
        let msg = conn.recv(Some(self.timeout()))?;
        let reply_type = msg.header.type_;
        let reply_id = msg.header.request_id;
        if reply_type != type_ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response type mismatch",
            ));
        }
        if reply_id != request_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response request_id mismatch",
            ));
        }
        serde_json::from_slice(&msg.body).map_err(io::Error::other)
    }
}

pub fn route_set(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
    presets: &PresetManager,
    target: Route,
) -> Result<Route, (String, String)> {
    let _transition = services.transition();
    route_set_locked(state, services, config, presets, target)
}

/// 调用方已持 transition 锁（poller 仲裁应用等待槽路由时用）。
pub(crate) fn route_set_locked(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
    presets: &PresetManager,
    target: Route,
) -> Result<Route, (String, String)> {
    // 快查也必须在 transition 锁内：锁外读取会在切换进行中给出假成功应答。
    {
        let mut current = lock_state(state);
        if current.route_desired() == target && current.route_active() == target {
            return Ok(target);
        }
        if current.gpu_owner() == Some(Activity::File) {
            // 文件任务占用 GPU：接受意图但不抢 GPU。rvc 挂等待槽（desired
            // 单槽，后覆盖前），任务结束后 poller 应用；direct 清等待意图，
            // 不碰 rvc 流（文件数据面正在用）。
            current.set_route_desired(target);
            return Ok(target);
        }
    }
    match target {
        Route::Direct => switch_direct(state, services),
        Route::Rvc => switch_rvc(state, services, config, presets),
    }
}

pub fn mute_set(
    state: &Mutex<State>,
    services: &Services,
    mute: bool,
) -> Result<bool, (String, String)> {
    let _transition = services.transition();
    set_pre_mute(services, mute).map_err(CallError::into_rejection)?;
    lock_state(state).commit_mute(mute);
    Ok(mute)
}

// 字幕链路开关：daemon 不持有桥的活状态（SPEC-04），只转发命令并回传桥的应答数据，
// 生效值由下一秒轮询汇总进 status。转发不碰路由/静音，无需 transition 锁。

pub fn translate_set(services: &Services, enabled: bool) -> Result<Value, (String, String)> {
    let body = json!({"enabled": enabled}).to_string();
    let response = services
        .bridge()
        .call_ok(ctl::BRIDGE_TRANSLATE_SET, body.as_bytes())
        .map_err(CallError::into_rejection)?;
    Ok(response.data)
}

pub fn speech_set(services: &Services, enabled: bool) -> Result<Value, (String, String)> {
    let body = json!({"enabled": enabled}).to_string();
    let response = services
        .bridge()
        .call_ok(ctl::BRIDGE_SPEECH_SET, body.as_bytes())
        .map_err(CallError::into_rejection)?;
    Ok(response.data)
}

pub fn speech_stop(state: &Mutex<State>, services: &Services) -> Result<Value, (String, String)> {
    let response = services
        .bridge()
        .call_ok(ctl::BRIDGE_SPEECH_STOP, b"{}")
        .map_err(CallError::into_rejection)?;
    // 桥已关播报并停新合成（桥向 TTS 下发 TTS_JOB_STOP）；再清 pre 播报环里
    // 已送达未播的帧。pre 缺席只记降级，不阻断停止（与路由切换同一纪律）。
    if let Err(err) = services.pre().call_ok(ctl::PRE_SPEECH_STOP, b"{}") {
        lock_state(state).push_degraded(format!("pre speech stop: {}", err.describe()));
    }
    Ok(response.data)
}

/// 启动/重启恢复（SPEC-04）：把状态文件的字幕开关覆盖一次性下发到桥。
/// 调用方须持 `pending` 锁，避免与并发开关命令交错覆写；成功一项清一项，
/// 桥未就绪下一轮重试。活权威在桥，运行时不对账。
pub fn restore_bridge_switches(services: &Services, pending: &mut BridgeRestore) {
    if let Some(enabled) = pending.translate {
        if translate_set(services, enabled).is_ok() {
            pending.translate = None;
        }
    }
    if let Some(enabled) = pending.speech {
        if speech_set(services, enabled).is_ok() {
            pending.speech = None;
        }
    }
}

fn switch_direct(state: &Mutex<State>, services: &Services) -> Result<Route, (String, String)> {
    set_pre_route(services, Route::Direct).map_err(CallError::into_rejection)?;
    if let Err(err) = stop_rvc_stream(services) {
        lock_state(state).push_degraded(format!("rvc stream stop: {}", err.describe()));
    }
    lock_state(state).commit_route(Route::Direct);
    Ok(Route::Direct)
}

/// rvc 模型清单（RVC_LIST_MODELS）。model list 命令转发用；文件入队不预检
/// 模型存在性，缺失统一由推理层 LoadModel 运行时报错（SPEC-10）。
pub(crate) fn list_models(services: &Services) -> Result<Vec<ModelEntry>, (String, String)> {
    let response = services
        .rvc()
        .call_ok(ctl::RVC_LIST_MODELS, b"{}")
        .map_err(CallError::into_rejection)?;
    let models = response.data.get("models").cloned().ok_or_else(|| {
        (
            ctl::CODE_INTERNAL.to_string(),
            "ListModels response missing models".to_string(),
        )
    })?;
    serde_json::from_value::<Vec<ModelEntry>>(models)
        .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))
}

fn switch_rvc(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
    presets: &PresetManager,
) -> Result<Route, (String, String)> {
    let status = services.rvc_status()?;
    let model_id = resolve_model_id(&config.rvc, &status)?;
    if !realtime_stream_matches(&status, &model_id) {
        // 模型切换：先读校验预设，LoadModel 成功后再写基线；同模型复用 live 参数。
        let resolution = presets.resolve_for_model_locked(&model_id)?;
        let handle = services
            .load_model_handle(&model_id)
            .map_err(CallError::into_rejection)?;
        let fresh = presets.commit_for_model_locked(&model_id, resolution)?;
        start_rvc_stream_with_handle(services, &fresh.rvc, handle, true)
            .map_err(CallError::into_rejection)?;
    }

    let pre_result = {
        let mut pre = services.pre();
        pre.call_ok(ctl::PRE_SET_ROUTE, br#"{"route":"rvc"}"#)
            .map_err(CallError::into_rejection)
    };
    if let Err(rejection) = pre_result {
        if let Err(err) = stop_rvc_stream(services) {
            lock_state(state).push_degraded(format!("rvc stream rollback: {}", err.describe()));
        }
        return Err(rejection);
    }

    lock_state(state).commit_route(Route::Rvc);
    Ok(Route::Rvc)
}

fn resolve_model_id(rvc: &RvcConfig, status: &RvcStatus) -> Result<String, (String, String)> {
    if !rvc.model_id.is_empty() {
        return Ok(rvc.model_id.clone());
    }
    status.model_id.clone().ok_or_else(|| {
        (
            ctl::CODE_NOT_READY.to_string(),
            "no model loaded and rvc.model_id is empty".to_string(),
        )
    })
}

/// 实时流是否已按期望跑起来。文件流残留（StopStream 超时等）不算实时流：
/// 只比 state/model 会把 kind=file 的流误判为可用，pre 切到 rvc 后无输出。
fn realtime_stream_matches(status: &RvcStatus, model_id: &str) -> bool {
    status.state == "streaming"
        && status.stream_kind.as_deref() == Some("realtime")
        && status.model_id.as_deref() == Some(model_id)
}

/// StartStream 结果：`Started` 为本次按请求起流；`Running` 为 Busy 被
/// `busy_ok` 入口当作已有流。rvc 不回报在跑流的参数，`Running` 时无法确认。
enum StartOutcome {
    Started,
    Running,
}

fn start_rvc_stream(
    services: &Services,
    rvc: &RvcConfig,
    model_id: &str,
    busy_ok: bool,
) -> Result<StartOutcome, CallError> {
    let handle = services.load_model_handle(model_id)?;
    start_rvc_stream_with_handle(services, rvc, handle, busy_ok)
}

/// 已知 model_handle 的起流：预设载入把 LoadModel 夹在读校验与写基线之间，
/// 调用方先取 handle、提交预设后再起流。
fn start_rvc_stream_with_handle(
    services: &Services,
    rvc: &RvcConfig,
    handle: u64,
    busy_ok: bool,
) -> Result<StartOutcome, CallError> {
    let mut body = stream_body(rvc, handle);
    body["kind"] = json!("realtime");
    let text = body.to_string();
    let mut rvc = services.rvc();
    match rvc.call_ok(ctl::RVC_START_STREAM, text.as_bytes()) {
        Ok(_) => Ok(StartOutcome::Started),
        // 路由切换/对账时已有流视为就绪；config set 重启链路要求确认新参生效，
        // Busy 说明流未按新参重建，按失败处理。
        Err(err) => match &err {
            CallError::Remote(response) if busy_ok && response.code == ctl::CODE_BUSY => {
                Ok(StartOutcome::Running)
            }
            _ => Err(err),
        },
    }
}

fn stop_rvc_stream(services: &Services) -> Result<(), CallError> {
    services.stop_stream_inner()
}

fn set_pre_route(services: &Services, route: Route) -> Result<(), CallError> {
    let body = json!({"route": route}).to_string();
    services
        .pre()
        .call_ok(ctl::PRE_SET_ROUTE, body.as_bytes())
        .map(|_| ())
}

fn set_pre_mute(services: &Services, mute: bool) -> Result<(), CallError> {
    let body = json!({"mute": mute}).to_string();
    services
        .pre()
        .call_ok(ctl::PRE_SET_MUTE, body.as_bytes())
        .map(|_| ())
}

pub fn poll(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
    presets: &PresetManager,
    pre_handoff_active: bool,
) {
    let mut degraded = Vec::new();
    let enhance_status = poll_enhance(services, &mut degraded);
    let pre_status = poll_pre(services, &mut degraded);
    let rvc_status = poll_rvc(services, &mut degraded);
    let asr_status = poll_asr(services, &mut degraded);
    let tts_status = poll_tts(services, &mut degraded);
    let bridge_status = poll_bridge(services, &mut degraded);

    let transition = match services.transition.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(poisoned)) => {
            degraded.push("transition lock poisoned; recovered".to_string());
            Some(poisoned.into_inner())
        }
    };
    if let Some(_transition) = transition {
        reconcile_pre_mode(
            state,
            services,
            pre_status.as_ref(),
            pre_handoff_active,
            &mut degraded,
        );
        reconcile(
            state,
            services,
            config,
            presets,
            pre_status.as_ref(),
            rvc_status.as_ref(),
            &mut degraded,
        );
    }

    let mut state = lock_state(state);
    state.set_enhance_status(enhance_status);
    state.set_pre_status(pre_status);
    state.set_rvc_status(rvc_status);
    state.set_asr_status(asr_status);
    state.set_tts_status(tts_status);
    state.set_bridge_status(bridge_status);
    state.set_degraded(degraded);
}

fn poll_enhance(services: &Services, degraded: &mut Vec<String>) -> Option<EnhanceStatus> {
    let mut link = services
        .enhance
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let enhance = link.as_mut()?;
    match enhance.call_ok(ctl::ENHANCE_GET_STATUS, b"{}") {
        Ok(response) => match serde_json::from_value::<EnhanceStatus>(response.data) {
            Ok(status) => {
                if let Some(error) = &status.last_error {
                    degraded.push(format!("enhance: {error}"));
                }
                Some(status)
            }
            Err(err) => {
                degraded.push(format!("enhance status parse: {err}"));
                None
            }
        },
        Err(err) => {
            degraded.push(format!("enhance status: {}", err.describe()));
            None
        }
    }
}

fn pre_enhancement_degraded(status: &PreStatus, degraded: &mut Vec<String>) {
    if let Some(enhancement) = &status.enhancement {
        if enhancement.enabled {
            if !enhancement.connected {
                degraded.push("pre enhancement: disconnected".to_string());
            }
            if let Some(error) = &enhancement.last_error {
                degraded.push(format!("pre enhancement: {error}"));
            }
        }
    }
}

fn poll_pre(services: &Services, degraded: &mut Vec<String>) -> Option<PreStatus> {
    let mut pre = services.pre();
    match pre.call_ok(ctl::PRE_GET_STATUS, b"{}") {
        Ok(response) => match serde_json::from_value::<PreStatus>(response.data) {
            Ok(status) => {
                pre_enhancement_degraded(&status, degraded);
                Some(status)
            }
            Err(err) => {
                degraded.push(format!("pre status parse: {err}"));
                None
            }
        },
        Err(err) => {
            degraded.push(format!("pre status: {}", err.describe()));
            None
        }
    }
}

fn poll_rvc(services: &Services, degraded: &mut Vec<String>) -> Option<RvcStatus> {
    let mut rvc = services.rvc();
    match rvc.call_ok(ctl::RVC_GET_STATUS, b"{}") {
        Ok(response) => match serde_json::from_value::<RvcStatus>(response.data) {
            Ok(status) => Some(status),
            Err(err) => {
                degraded.push(format!("rvc status parse: {err}"));
                None
            }
        },
        Err(err) => {
            degraded.push(format!("rvc status: {}", err.describe()));
            None
        }
    }
}

fn poll_asr(services: &Services, degraded: &mut Vec<String>) -> Option<AsrStatus> {
    let mut asr = services.asr();
    match asr.call_ok(ctl::ASR_GET_STATUS, b"{}") {
        Ok(response) => match serde_json::from_value::<AsrStatus>(response.data) {
            Ok(status) => Some(status),
            Err(err) => {
                degraded.push(format!("asr status parse: {err}"));
                None
            }
        },
        Err(err) => {
            degraded.push(format!("asr status: {}", err.describe()));
            None
        }
    }
}

fn poll_tts(services: &Services, degraded: &mut Vec<String>) -> Option<TtsStatus> {
    let mut tts = services.tts();
    match tts.call_ok(ctl::TTS_GET_STATUS, b"{}") {
        Ok(response) => match serde_json::from_value::<TtsStatus>(response.data) {
            Ok(status) => Some(status),
            Err(err) => {
                degraded.push(format!("tts status parse: {err}"));
                None
            }
        },
        Err(err) => {
            degraded.push(format!("tts status: {}", err.describe()));
            None
        }
    }
}

fn poll_bridge(services: &Services, degraded: &mut Vec<String>) -> Option<BridgeStatus> {
    let mut bridge = services.bridge();
    match bridge.call_ok(ctl::BRIDGE_GET_STATUS, b"{}") {
        Ok(response) => match serde_json::from_value::<BridgeStatus>(response.data) {
            Ok(status) => Some(status),
            Err(err) => {
                degraded.push(format!("bridge status parse: {err}"));
                None
            }
        },
        Err(err) => {
            degraded.push(format!("bridge status: {}", err.describe()));
            None
        }
    }
}

/// 权威对账 pre 数据面模式（SPEC-04 单一权威）：pre 的 rvc AI 支路仅在 daemon
/// 断言实时态时活跃，pre 启动/重启后不单边 reclaim，等 daemon 断言。daemon 每轮
/// 把 pre 模式收敛到权威态（gpu_owner==File → 文件接管），并在 pre 重启（startup_id
/// 变化）或尚未断言时重新断言。文件交接窗口内让位给 FileHandoff，避免争用。
fn reconcile_pre_mode(
    state: &Mutex<State>,
    services: &Services,
    pre_status: Option<&PreStatus>,
    handoff_active: bool,
    degraded: &mut Vec<String>,
) {
    let Some(pre) = pre_status else {
        return;
    };
    // 交接建立期间 pre 模式由 FileHandoff 起停流直接管理，对账不介入。
    if handoff_active {
        return;
    }
    let expected_file = lock_state(state).gpu_owner() == Some(Activity::File);
    let restarted = {
        let state = lock_state(state);
        state.pre_startup_id() != Some(pre.startup_id.as_str())
    };
    let needs_assert = restarted || !pre.mode_asserted || pre.file_mode != expected_file;
    if needs_assert {
        if let Err((code, reason)) = services.pre_set_file_mode(expected_file) {
            degraded.push(format!("pre mode assert: {code}: {reason}"));
        }
    }
    lock_state(state).set_pre_startup_id(Some(pre.startup_id.clone()));
}

fn reconcile(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
    presets: &PresetManager,
    pre_status: Option<&PreStatus>,
    rvc_status: Option<&RvcStatus>,
    degraded: &mut Vec<String>,
) {
    let (active, mute, gpu_owner) = {
        let state = lock_state(state);
        (state.route_active(), state.mute(), state.gpu_owner())
    };

    // 文件任务持有 GPU 时 rvc 流归文件数据面，实时对账不得停/起流；
    // pre 路由/静音仍按 active（direct）对账。
    let file_gpu = gpu_owner == Some(Activity::File);

    // rvc 方向与 switch_rvc 同序：先确保实时流在跑，再让 pre 切到 rvc。
    // 双偏（rvc 重启 + pre 路由偏）时先切路由，起流窗口内 pre 发往 rvc
    // 的帧会丢。流未就绪本轮不切 rvc 路由，下一轮重试。
    let rvc_route_ready = if file_gpu || active != Route::Rvc {
        true
    } else {
        ensure_realtime_stream(services, &config.rvc, rvc_status, presets, degraded)
    };

    if let Some(status) = pre_status {
        if status.route != active && rvc_route_ready {
            if let Err(err) = set_pre_route(services, active) {
                degraded.push(format!("pre route apply: {}", err.describe()));
            }
        }
        if status.mute != mute {
            if let Err(err) = set_pre_mute(services, mute) {
                degraded.push(format!("pre mute apply: {}", err.describe()));
            }
        }
    }

    if !file_gpu && active == Route::Direct {
        if let Some(status) = rvc_status {
            if status.state == "streaming" {
                if let Err(err) = stop_rvc_stream(services) {
                    degraded.push(format!("rvc stream stop: {}", err.describe()));
                }
            }
        }
    }
}

/// 实时流是否已按期望就绪：已在跑或本轮起流成功为 true。
/// rvc 缺席、模型不可解析或起流失败为 false，原因记入 degraded。
fn ensure_realtime_stream(
    services: &Services,
    rvc: &RvcConfig,
    status: Option<&RvcStatus>,
    presets: &PresetManager,
    degraded: &mut Vec<String>,
) -> bool {
    let Some(status) = status else {
        return false;
    };
    let model_id = match resolve_model_id(rvc, status) {
        Ok(model_id) => model_id,
        Err((code, reason)) => {
            degraded.push(format!("rvc stream apply: {code}: {reason}"));
            return false;
        }
    };
    if realtime_stream_matches(status, &model_id) {
        return true;
    }
    let resolution = match presets.resolve_for_model_locked(&model_id) {
        Ok(resolution) => resolution,
        Err((code, reason)) => {
            degraded.push(format!("rvc preset load: {code}: {reason}"));
            return false;
        }
    };
    // LoadModel 成功后才写基线：加载失败时 live 参数仍属已加载模型。
    let handle = match services.load_model_handle(&model_id) {
        Ok(handle) => handle,
        Err(err) => {
            degraded.push(format!("rvc stream apply: {}", err.describe()));
            return false;
        }
    };
    let fresh = match presets.commit_for_model_locked(&model_id, resolution) {
        Ok(config) => config,
        Err((code, reason)) => {
            degraded.push(format!("rvc preset load: {code}: {reason}"));
            return false;
        }
    };
    match start_rvc_stream_with_handle(services, &fresh.rvc, handle, true) {
        Ok(_) => true,
        Err(err) => {
            degraded.push(format!("rvc stream apply: {}", err.describe()));
            false
        }
    }
}

/// config set rvc.<会话参数> 的生效配置合并（SPEC-04）：只吸收本次写入的键，
/// 其余 rvc 键保留旧生效值。未随本次写入应用的 TOML 新基线（如失败恢复后
/// 留下的 pitch）不得混入，否则 status 与在跑流不一致；这些基线等对应键
/// 再次写入或 daemon 重启时应用。未知会话键返回错误而不是 panic：core 新增
/// RVC 运行时键时控制线程保持可用。
pub(crate) fn merge_session_key(
    key: &str,
    old: &RvcConfig,
    fresh: &RvcConfig,
) -> Result<RvcConfig, (String, String)> {
    let mut merged = old.clone();
    match key {
        "rvc.pitch_shift" => merged.pitch_shift = fresh.pitch_shift,
        "rvc.index_rate" => merged.index_rate = fresh.index_rate,
        "rvc.f0_method" => merged.f0_method.clone_from(&fresh.f0_method),
        "rvc.rms_mix_rate" => merged.rms_mix_rate = fresh.rms_mix_rate,
        "rvc.protect" => merged.protect = fresh.protect,
        "rvc.realtime.crossfade_ms" => merged.realtime.crossfade_ms = fresh.realtime.crossfade_ms,
        "rvc.realtime.sola_search_ms" => {
            merged.realtime.sola_search_ms = fresh.realtime.sola_search_ms
        }
        "rvc.realtime.startup_delay_ms" => {
            merged.realtime.startup_delay_ms = fresh.realtime.startup_delay_ms
        }
        "rvc.realtime.skip_silence" => merged.realtime.skip_silence = fresh.realtime.skip_silence,
        other => {
            return Err((
                ctl::CODE_INTERNAL.to_string(),
                format!("rvc session params: no orchestration for key {other}"),
            ));
        }
    }
    Ok(merged)
}

/// config set rvc.<会话参数> 与 preset use 的实时链路编排（SPEC-04）：
/// 写 TOML 由 core/preset 完成，daemon 负责应用。调用方须持 transition 锁，
/// 并在返回 Ok 后、释放锁前替换生效配置，避免轮询在替换前用旧快照起流。
/// `new` 只含本次写入键的改动（见 `merge_session_key`）：
/// - `crossfade_ms` / `sola_search_ms` / `startup_delay_ms` 决定流缓冲结构，
///   变化且实时流在跑时走 `StopStream`→`StartStream`（失败用旧参恢复）；
/// - `pitch_shift` / `index_rate` / `f0_method` / `skip_silence` 变化且实时流
///   在跑时走 `SetParams` 在下一推理块边界无中断生效（失败保留在跑旧参）；
/// - 仅改 `protect` / `rms_mix_rate` 或值未变化时只更新 TOML 基线与生效配置。
/// 失败恢复后不自动重试，下一次对应键写入或 daemon 重启再应用。路由非 rvc、
/// rvc 不可达或没有实时流时只更新生效配置：文件任务使用启动时的配置快照，
/// 下一任务与下一次建流自然读到新值。
pub(crate) fn apply_rvc_session_params_locked(
    state: &Mutex<State>,
    services: &Services,
    old: &RvcConfig,
    new: &RvcConfig,
) -> Result<(), (String, String)> {
    let restart_changed = realtime_restart_changed(old, new);
    let hot_changed = realtime_hot_params_changed(old, new);
    // 仅 protect/rms_mix_rate 变化或值未变化：实时链路不应用，无需下发 rvc。
    if !restart_changed && !hot_changed {
        return Ok(());
    }
    // 文件任务只在 Direct 下启动（GPU 归文件期间 route set 只写 desired），
    // 且 rvc 在跑 file 流时下面的 stream_kind 也不匹配：route 与 kind 已覆盖，
    // 无需再看 gpu_owner。
    if lock_state(state).route_active() != Route::Rvc {
        return Ok(());
    }
    let Ok(status) = services.rvc_status() else {
        // rvc 不可达时无法确认实时流：不打断，下一次建流用新快照。
        // 生效配置仍更新，但无法验证在跑流，留可查询降级原因。
        lock_state(state).push_degraded(
            "rvc session params: rvc status unavailable; config updated, stream not verified"
                .to_string(),
        );
        return Ok(());
    };
    if status.state != "streaming" || status.stream_kind.as_deref() != Some("realtime") {
        return Ok(());
    }
    if !restart_changed {
        return services
            .set_rvc_params(new)
            .map_err(|(code, reason)| (code, format!("rvc set params: {reason}")));
    }
    let model_id = resolve_model_id(new, &status)?;

    if let Err(err) = stop_rvc_stream(services) {
        // 停流失败时流可能仍在（旧参）：Busy 视为已恢复。
        let (code, reason) = err.into_rejection();
        let original = format!("rvc stop stream: {reason}");
        return match start_rvc_stream(services, old, &model_id, true) {
            Ok(_) => Err((code, original)),
            Err(restore) => Err(restore_failure(
                state,
                "rvc session params",
                code,
                original,
                restore,
            )),
        };
    }
    // 停流成功：先按新参重建，失败再按旧参恢复。
    match start_rvc_stream(services, new, &model_id, false) {
        Ok(_) => Ok(()),
        Err(err) => {
            let (code, reason) = err.into_rejection();
            let original = format!("rvc start with new params: {reason}");
            match start_rvc_stream(services, old, &model_id, true) {
                Ok(StartOutcome::Started) => Err((code, original)),
                Ok(StartOutcome::Running) => {
                    // 旧参恢复遇 Busy：流仍在跑，但 rvc 不回报其参数，
                    // 记录持久降级原因供 status 查询。
                    lock_state(state).push_degraded(
                        "rvc session params: recovery busy; running params unknown".to_string(),
                    );
                    Err((
                        code,
                        format!("{original}; restore busy, running params unknown"),
                    ))
                }
                Err(restore) => Err(restore_failure(
                    state,
                    "rvc session params",
                    code,
                    original,
                    restore,
                )),
            }
        }
    }
}

/// 决定流缓冲结构、须停流重起才生效的 `[rvc.realtime]` 旋钮是否变化。
fn realtime_restart_changed(old: &RvcConfig, new: &RvcConfig) -> bool {
    old.realtime.crossfade_ms != new.realtime.crossfade_ms
        || old.realtime.sola_search_ms != new.realtime.sola_search_ms
        || old.realtime.startup_delay_ms != new.realtime.startup_delay_ms
}

/// 实时链路实际使用的、可经 `SetParams` 块边界无中断生效的参数是否变化：
/// `protect`、`rms_mix_rate` 只在文件链路生效，变化不触发 `SetParams`。
fn realtime_hot_params_changed(old: &RvcConfig, new: &RvcConfig) -> bool {
    old.pitch_shift != new.pitch_shift
        || old.index_rate != new.index_rate
        || old.f0_method != new.f0_method
        || old.realtime.skip_silence != new.realtime.skip_silence
}

/// 换模型降级原因前缀：记录与 status 查询共用一处定义。
const MODEL_USE_CONTEXT: &str = "rvc model use";

/// 实时/空闲换模型（SPEC-04 `model use <id>`）：采用断流切换
/// （StopStream→LoadModel→预设提交→StartStream），避免 8 GB 设备同时驻留双引擎
/// OOM，并支持跨采样率（40k/48k）重建 `StreamingRvc`。
/// `old` 是切换前的生效配置：所有失败路径都用它恢复旧模型与旧参数，目标预设
/// 参数只用于提交后的那一次起流。预设提交夹在 LoadModel 与 StartStream 之间
/// （SPEC-11 两步契约）；提交时流已停，`apply_rvc_session_params_locked` 只更新
/// 配置，成功路径因此只起一次流。起流失败先整体回退提交，再恢复旧流。
/// 返回切换前已加载的模型 id。调用方须已持 transition 锁。
pub(crate) fn switch_rvc_model_locked(
    state: &Mutex<State>,
    services: &Services,
    presets: &PresetManager,
    old: &Config,
    model_id: &str,
) -> Result<Option<String>, (String, String)> {
    if model_id.is_empty() {
        return Err((
            ctl::CODE_INVALID_ARGUMENT.to_string(),
            "model_id must not be empty".to_string(),
        ));
    }
    if lock_state(state).gpu_owner() == Some(Activity::File) {
        return Err((
            ctl::CODE_BUSY.to_string(),
            "cannot switch realtime model while file job owns GPU".to_string(),
        ));
    }
    let status = services.rvc_status()?;
    let previous = status.model_id.clone();
    let was_realtime_streaming =
        status.state == "streaming" && status.stream_kind.as_deref() == Some("realtime");
    // 读校验在停流前：损坏预设不打断在跑的流，也不写任何基线。
    let resolution = presets.resolve_for_model_locked(model_id)?;

    if was_realtime_streaming {
        stop_rvc_stream(services).map_err(|err| err.into_rejection())?;
    }

    let handle = match services.load_model_handle(model_id) {
        Ok(handle) => handle,
        Err(err) => {
            let (code, reason) = err.into_rejection();
            return Err(restore_model_stream(
                state,
                services,
                &old.rvc,
                previous.as_deref(),
                was_realtime_streaming,
                code,
                reason,
            ));
        }
    };

    // LoadModel 成功才写基线与 applied；提交时流已停，不会二次起停。
    let (fresh, undo) = match presets.commit_model_switch_locked(model_id, resolution) {
        Ok(committed) => committed,
        Err((code, reason)) => {
            // 提交内部已回退它写下的基线；这里只恢复旧模型旧参数的流。
            return Err(restore_model_stream(
                state,
                services,
                &old.rvc,
                previous.as_deref(),
                was_realtime_streaming,
                code,
                format!("model use preset commit: {reason}"),
            ));
        }
    };

    if was_realtime_streaming {
        if let Err(err) = start_rvc_stream_with_handle(services, &fresh.rvc, handle, false) {
            let (code, reason) = err.into_rejection();
            // 基线与 applied 已落目标模型：先整体回退，再恢复旧流，
            // 否则留下「live 参数属目标模型、rvc 载旧模型」的错位。
            presets.rollback_model_switch_locked(undo);
            return Err(restore_model_stream(
                state,
                services,
                &old.rvc,
                previous.as_deref(),
                true,
                code,
                format!("model use start stream: {reason}"),
            ));
        }
    }
    Ok(previous)
}

/// 换模型失败后恢复旧模型与旧参数的实时流。`old` 必须是切换前的生效会话参数：
/// 用目标预设参数恢复会让旧模型跑新模型的音高。恢复失败记持久降级原因并组合
/// 进返回错误；恢复遇 Busy 说明流仍在跑但参数不确定，同样记录。
fn restore_model_stream(
    state: &Mutex<State>,
    services: &Services,
    old: &RvcConfig,
    previous: Option<&str>,
    was_realtime_streaming: bool,
    code: String,
    original: String,
) -> (String, String) {
    // 切换前没有实时流或没有已加载模型：无流可恢复，rvc 停在加载态/空闲。
    if !was_realtime_streaming {
        return (code, original);
    }
    let Some(prev) = previous else {
        return (code, original);
    };
    match start_rvc_stream(services, old, prev, true) {
        Ok(StartOutcome::Started) => (code, original),
        Ok(StartOutcome::Running) => {
            let reason = format!("{original}; restore busy, running params unknown");
            lock_state(state).push_degraded(format!("{MODEL_USE_CONTEXT}: {reason}"));
            (code, reason)
        }
        Err(restore) => restore_failure(state, MODEL_USE_CONTEXT, code, original, restore),
    }
}

/// 恢复失败：组合原始错误与恢复错误，并留可查询的持久降级原因。
/// `context` 标明编排来源（会话参数写入 / 换模型），status 据此区分。
fn restore_failure(
    state: &Mutex<State>,
    context: &str,
    code: String,
    original: String,
    restore: CallError,
) -> (String, String) {
    let reason = format!(
        "{original}; restore with old params: {}",
        restore.describe()
    );
    lock_state(state).push_degraded(format!("{context}: {reason}"));
    (code, reason)
}

fn validate_dsp_echo(
    data: serde_json::Value,
    expected: &mozart_core::config::PreDspConfig,
) -> Result<(), (String, String)> {
    let echoed: ctl::schema::DspSetRequest = serde_json::from_value(data).map_err(|err| {
        (
            ctl::CODE_INTERNAL.to_string(),
            format!("pre SetDsp invalid echo: {err}"),
        )
    })?;
    if (echoed.denoise, echoed.gate, echoed.agc, echoed.limiter)
        != (
            expected.denoise,
            expected.gate,
            expected.agc,
            expected.limiter,
        )
    {
        return Err((
            ctl::CODE_INTERNAL.to_string(),
            "pre SetDsp echo differs from request".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsp_echo_requires_all_boolean_fields_and_exact_requested_values() {
        let expected = mozart_core::config::PreDspConfig::default();
        assert!(validate_dsp_echo(
            serde_json::json!({"denoise":true,"gate":false,"agc":false,"limiter":false}),
            &expected
        )
        .is_ok());
        for invalid in [
            serde_json::json!({"denoise":true}),
            serde_json::json!({"denoise":"true","gate":false,"agc":false,"limiter":false}),
            serde_json::json!({"denoise":false,"gate":false,"agc":false,"limiter":false}),
            serde_json::json!({"denoise":true,"gate":false,"agc":false,"limiter":false,"extra":true}),
        ] {
            let (code, reason) = validate_dsp_echo(invalid, &expected).unwrap_err();
            assert_eq!(code, ctl::CODE_INTERNAL);
            assert!(reason.contains("echo"));
        }
    }

    /// core 新增 RVC 运行时键但 daemon 未实现编排时返回错误，不 panic 控制线程。
    #[test]
    fn merge_session_key_rejects_unknown_key_without_panicking() {
        let old = RvcConfig::default();
        let fresh = RvcConfig::default();
        let (code, reason) = merge_session_key("rvc.future_key", &old, &fresh).unwrap_err();
        assert_eq!(code, ctl::CODE_INTERNAL);
        assert!(reason.contains("rvc.future_key"), "{reason}");
    }
}
