//! 子服务控制：连接 pre/rvc/asr/tts/bridge、下发路由与静音、转发字幕开关、轮询状态并对账。
//! 对账以 daemon 的期望状态为准，切路由与轮询共用同一把 transition 锁。
//! llama-server 无控制协议，不建 ServiceLink，只由 supervisor 监管进程。

use crate::state::{lock_state, State};
use mozart_core::config::Config;
use mozart_core::ctl::schema::{AsrStatus, BridgeStatus, PreStatus, Response, Route, RvcStatus, TtsStatus};
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
    pub fn new(config: &Config) -> Self {
        let timeout = Duration::from_millis(config.daemon.rpc_timeout_ms);
        Self {
            pre: Mutex::new(ServiceLink::new(
                ctl::PRE_HELLO,
                config.pre_socket(),
                timeout,
            )),
            rvc: Mutex::new(ServiceLink::new(
                ctl::RVC_HELLO,
                config.rvc_socket(),
                timeout,
            )),
            asr: Mutex::new(ServiceLink::new(
                ctl::ASR_HELLO,
                config.asr_socket(),
                timeout,
            )),
            tts: Mutex::new(ServiceLink::new(
                ctl::TTS_HELLO,
                config.tts_socket(),
                timeout,
            )),
            bridge: Mutex::new(ServiceLink::new(
                ctl::BRIDGE_HELLO,
                config.bridge_socket(),
                timeout,
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

    fn transition(&self) -> MutexGuard<'_, ()> {
        self.transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
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
    timeout: Duration,
    conn: Option<Seqpacket>,
    next_request: u32,
}

impl ServiceLink {
    fn new(hello: u16, socket: PathBuf, timeout: Duration) -> Self {
        Self {
            hello,
            socket,
            timeout,
            conn: None,
            next_request: 0,
        }
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
        let conn = Seqpacket::connect_timeout(&self.socket, Some(self.timeout))?;
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
        conn.send_timeout(type_, request_id, body, Some(self.timeout))?;
        let msg = conn.recv(Some(self.timeout))?;
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
    // 快查也必须在 transition 锁内：锁外读取会在切换进行中给出假成功应答。
    let _transition = services.transition();
    {
        let current = lock_state(state);
        if current.route_desired() == target && current.route_active() == target {
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

fn switch_direct(state: &Mutex<State>, services: &Services) -> Result<Route, (String, String)> {
    set_pre_route(services, Route::Direct).map_err(CallError::into_rejection)?;
    if let Err(err) = stop_rvc_stream(services) {
        lock_state(state).push_degraded(format!("rvc stream stop: {}", err.describe()));
    }
    lock_state(state).commit_route(Route::Direct);
    Ok(Route::Direct)
}

fn switch_rvc(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
) -> Result<Route, (String, String)> {
    let status = {
        let mut rvc = services.rvc();
        let response = rvc
            .call_ok(ctl::RVC_GET_STATUS, b"{}")
            .map_err(CallError::into_rejection)?;
        serde_json::from_value::<RvcStatus>(response.data)
            .map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))?
    };
    let model_id = resolve_model_id(config, &status)?;
    let model_matches = status.model_id.as_deref() == Some(model_id.as_str());
    if status.state != "streaming" || !model_matches {
        start_rvc_stream(services, config, &model_id).map_err(CallError::into_rejection)?;
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

fn start_rvc_stream(
    services: &Services,
    config: &Config,
    model_id: &str,
) -> Result<(), CallError> {
    let handle = {
        let mut rvc = services.rvc();
        let body = json!({"model_id": model_id}).to_string();
        let response = rvc.call_ok(ctl::RVC_LOAD_MODEL, body.as_bytes())?;
        response
            .data
            .get("model_handle")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| CallError::Protocol("LoadModel response missing model_handle".to_string()))?
    };
    let body = json!({
        "kind": "realtime",
        "model_handle": handle,
        "pitch_shift": config.rvc.pitch_shift,
        "index_rate": config.rvc.index_rate,
        "rms_mix_rate": config.rvc.rms_mix_rate,
        "protect": config.rvc.protect,
        "f0_method": config.rvc.f0_method,
    })
    .to_string();
    let mut rvc = services.rvc();
    match rvc.call_ok(ctl::RVC_START_STREAM, body.as_bytes()) {
        Ok(_) => Ok(()),
        Err(err) => match &err {
            CallError::Remote(response) if response.code == ctl::CODE_BUSY => Ok(()),
            _ => Err(err),
        },
    }
}

fn stop_rvc_stream(services: &Services) -> Result<(), CallError> {
    services
        .rvc()
        .call_ok(ctl::RVC_STOP_STREAM, b"{}")
        .map(|_| ())
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

pub fn poll(state: &Mutex<State>, services: &Services, config: &Config) {
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

fn reconcile(
    state: &Mutex<State>,
    services: &Services,
    config: &Config,
    pre_status: Option<&PreStatus>,
    rvc_status: Option<&RvcStatus>,
    degraded: &mut Vec<String>,
) {
    let (active, mute) = {
        let state = lock_state(state);
        (state.route_active(), state.mute())
    };

    if let Some(status) = pre_status {
        if status.route != active {
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

    if let Some(status) = rvc_status {
        match active {
            Route::Rvc => match resolve_model_id(config, status) {
                Ok(model_id) => {
                    let model_matches =
                        status.model_id.as_deref() == Some(model_id.as_str());
                    if status.state != "streaming" || !model_matches {
                        if let Err(err) = start_rvc_stream(services, config, &model_id) {
                            degraded.push(format!("rvc stream apply: {}", err.describe()));
                        }
                    }
                }
                Err((code, reason)) => {
                    degraded.push(format!("rvc stream apply: {code}: {reason}"));
                }
            },
            Route::Direct => {
                if status.state == "streaming" {
                    if let Err(err) = stop_rvc_stream(services) {
                        degraded.push(format!("rvc stream stop: {}", err.describe()));
                    }
                }
            }
        }
    }
}
