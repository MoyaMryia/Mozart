//! 子服务控制：连接 pre/rvc/asr/tts/bridge、下发路由与静音、转发字幕开关、轮询状态并对账。
//! 对账以 daemon 的期望状态为准，切路由与轮询共用同一把 transition 锁。
//! llama-server 无控制协议，不建 ServiceLink，只由 supervisor 监管进程。

use crate::runtime::SharedConfig;
use crate::state::{lock_state, State};
use crate::state_file::BridgeRestore;
use mozart_core::config::{Config, RvcConfig};
use mozart_core::ctl::schema::{
    Activity, AsrStatus, BridgeStatus, ModelEntry, PreStatus, Response, Route, RvcStatus, TtsStatus,
};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::Duration;

pub struct Services {
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
        let mut body = stream_body(config, model_handle);
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
        self.rvc()
            .call_ok(ctl::RVC_STOP_STREAM, b"{}")
            .map(|_| ())
    }
}

/// StartStream 会话参数（SPEC-03 参数边界表）：实时与文件共用 `[rvc]` 一份。
/// kind 与文件批处理字段由调用方补齐。
fn stream_body(config: &Config, model_handle: u64) -> Value {
    json!({
        "model_handle": model_handle,
        "pitch_shift": config.rvc.pitch_shift,
        "index_rate": config.rvc.index_rate,
        "rms_mix_rate": config.rvc.rms_mix_rate,
        "protect": config.rvc.protect,
        "f0_method": config.rvc.f0_method,
    })
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
    target: Route,
) -> Result<Route, (String, String)> {
    let _transition = services.transition();
    route_set_locked(state, services, config, target)
}

/// 调用方已持 transition 锁（poller 仲裁应用等待槽路由时用）。
pub(crate) fn route_set_locked(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
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
        Route::Rvc => switch_rvc(state, services, config),
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

/// 启动恢复（SPEC-04）：把状态文件的字幕开关覆盖一次性下发到桥。
/// 成功一项清一项；桥未就绪下一轮重试。活权威在桥，运行时不对账。
pub fn restore_bridge_switches(services: &Services, pending: &Mutex<BridgeRestore>) {
    let (translate, speech) = {
        let guard = pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (guard.translate, guard.speech)
    };
    if let Some(enabled) = translate {
        if translate_set(services, enabled).is_ok() {
            pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .translate = None;
        }
    }
    if let Some(enabled) = speech {
        if speech_set(services, enabled).is_ok() {
            pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .speech = None;
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
    let models = response
        .data
        .get("models")
        .cloned()
        .ok_or_else(|| {
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
) -> Result<Route, (String, String)> {
    let status = services.rvc_status()?;
    let model_id = resolve_model_id(config, &status)?;
    if !realtime_stream_matches(&status, &model_id) {
        start_rvc_stream(services, config, &model_id, true).map_err(CallError::into_rejection)?;
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

fn resolve_model_id(config: &Config, status: &RvcStatus) -> Result<String, (String, String)> {
    if !config.rvc.model_id.is_empty() {
        return Ok(config.rvc.model_id.clone());
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

fn start_rvc_stream(
    services: &Services,
    config: &Config,
    model_id: &str,
    busy_ok: bool,
) -> Result<(), CallError> {
    let handle = services.load_model_handle(model_id)?;
    let mut body = stream_body(config, handle);
    body["kind"] = json!("realtime");
    let text = body.to_string();
    let mut rvc = services.rvc();
    match rvc.call_ok(ctl::RVC_START_STREAM, text.as_bytes()) {
        Ok(_) => Ok(()),
        // 路由切换/对账时已有流视为就绪；config set 重启链路要求确认新参生效，
        // Busy 说明流未按新参重建，按失败处理。
        Err(err) => match &err {
            CallError::Remote(response) if busy_ok && response.code == ctl::CODE_BUSY => Ok(()),
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

pub fn poll(state: &Mutex<State>, services: &Services, config: &Config, pre_handoff_active: bool) {
    let mut degraded = Vec::new();
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
        reconcile_pre_mode(state, services, pre_status.as_ref(), pre_handoff_active, &mut degraded);
        reconcile(
            state,
            services,
            config,
            pre_status.as_ref(),
            rvc_status.as_ref(),
            &mut degraded,
        );
    }

    let mut state = lock_state(state);
    state.set_pre_status(pre_status);
    state.set_rvc_status(rvc_status);
    state.set_asr_status(asr_status);
    state.set_tts_status(tts_status);
    state.set_bridge_status(bridge_status);
    state.set_degraded(degraded);
}

fn poll_pre(services: &Services, degraded: &mut Vec<String>) -> Option<PreStatus> {
    let mut pre = services.pre();
    match pre.call_ok(ctl::PRE_GET_STATUS, b"{}") {
        Ok(response) => match serde_json::from_value::<PreStatus>(response.data) {
            Ok(status) => Some(status),
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
        ensure_realtime_stream(services, config, rvc_status, degraded)
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
    config: &Config,
    status: Option<&RvcStatus>,
    degraded: &mut Vec<String>,
) -> bool {
    let Some(status) = status else {
        return false;
    };
    let model_id = match resolve_model_id(config, status) {
        Ok(model_id) => model_id,
        Err((code, reason)) => {
            degraded.push(format!("rvc stream apply: {code}: {reason}"));
            return false;
        }
    };
    if realtime_stream_matches(status, &model_id) {
        return true;
    }
    match start_rvc_stream(services, config, &model_id, true) {
        Ok(()) => true,
        Err(err) => {
            degraded.push(format!("rvc stream apply: {}", err.describe()));
            false
        }
    }
}

/// config set rvc.<会话参数> 的实时链路编排（SPEC-04）：写 TOML 由 core 完成，
/// daemon 负责应用。只有 realtime 流在跑且实时链路实际使用的 `pitch_shift`、
/// `index_rate`、`f0_method` 变化时才 StopStream→StartStream（短暂打断）；
/// 失败用旧参恢复实时流，恢复失败返回组合错误并记录持久降级原因。
/// GPU 归文件、路由非 rvc 或没有实时流时只更新生效配置：文件任务使用启动时
/// 的配置快照，下一任务与下一次建流自然读到新值。
pub(crate) fn apply_rvc_session_params(
    state: &Mutex<State>,
    services: &Services,
    old: &Config,
    new: &Config,
) -> Result<(), (String, String)> {
    // 仅 protect/rms_mix_rate 变化或值未变化：实时链路不应用，无需停流。
    if !realtime_params_changed(&old.rvc, &new.rvc) {
        return Ok(());
    }
    let _transition = services.transition();
    let (route, gpu_owner) = {
        let state = lock_state(state);
        (state.route_active(), state.gpu_owner())
    };
    if gpu_owner == Some(Activity::File) || route != Route::Rvc {
        return Ok(());
    }
    let Ok(status) = services.rvc_status() else {
        // rvc 不可达时无法确认实时流；不打断，下一次建流用新快照。
        return Ok(());
    };
    if status.state != "streaming" || status.stream_kind.as_deref() != Some("realtime") {
        return Ok(());
    }
    let model_id = resolve_model_id(new, &status)?;

    if let Err(err) = stop_rvc_stream(services) {
        // 停流失败时流可能仍在（旧参）：Busy 视为已恢复。
        let (code, reason) = err.into_rejection();
        let original = format!("rvc stop stream: {reason}");
        return match start_rvc_stream(services, old, &model_id, true) {
            Ok(()) => Err((code, original)),
            Err(restore) => Err(restore_failure(state, code, original, restore)),
        };
    }
    // 停流成功：先按新参重建，失败再按旧参恢复。
    match start_rvc_stream(services, new, &model_id, false) {
        Ok(()) => Ok(()),
        Err(err) => {
            let (code, reason) = err.into_rejection();
            let original = format!("rvc start with new params: {reason}");
            match start_rvc_stream(services, old, &model_id, true) {
                Ok(()) => Err((code, original)),
                Err(restore) => Err(restore_failure(state, code, original, restore)),
            }
        }
    }
}

/// 实时链路实际使用的会话参数是否变化（SPEC-03 参数边界表）：
/// `protect`、`rms_mix_rate` 只在文件链路生效，变化不打断实时流。
fn realtime_params_changed(old: &RvcConfig, new: &RvcConfig) -> bool {
    old.pitch_shift != new.pitch_shift
        || old.index_rate != new.index_rate
        || old.f0_method != new.f0_method
}

/// 恢复失败：组合原始错误与恢复错误，并留可查询的持久降级原因。
fn restore_failure(
    state: &Mutex<State>,
    code: String,
    original: String,
    restore: CallError,
) -> (String, String) {
    let reason = format!(
        "{original}; restore with old params: {}",
        restore.describe()
    );
    lock_state(state).push_degraded(format!("rvc session params: {reason}"));
    (code, reason)
}
