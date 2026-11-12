use mozart_core::ctl::schema::{
    Activity, ArbiterState, AsrStatus, BridgeStatus, DaemonInfo, ExitInfo, GpuState, HardwareStatus,
    PreStatus, ProcessInfo, ProcessState, Processes, Route, RouteState, RvcStatus, Status, TtsStatus,
};
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Pre,
    Rvc,
    Asr,
    Tts,
    Bridge,
    Llama,
}

pub fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct State {
    started: Instant,
    route_desired: Route,
    route_active: Route,
    route_since_unix_ms: u64,
    mute: bool,
    gpu_owner: Option<Activity>,
    processes: Processes,
    pre: Option<PreStatus>,
    rvc: Option<RvcStatus>,
    asr: Option<AsrStatus>,
    tts: Option<TtsStatus>,
    bridge: Option<BridgeStatus>,
    hw: Option<HardwareStatus>,
    degraded: Vec<String>,
    shutting_down: bool,
}

impl State {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            route_desired: Route::Direct,
            route_active: Route::Direct,
            route_since_unix_ms: now_unix_ms(),
            mute: false,
            gpu_owner: None,
            processes: Processes {
                pre: ProcessInfo::stopped(),
                rvc: ProcessInfo::stopped(),
                asr: ProcessInfo::stopped(),
                tts: ProcessInfo::stopped(),
                bridge: ProcessInfo::stopped(),
                llama: ProcessInfo::stopped(),
            },
            pre: None,
            rvc: None,
            asr: None,
            tts: None,
            bridge: None,
            hw: None,
            degraded: Vec::new(),
            shutting_down: false,
        }
    }

    pub fn status(&self) -> Status {
        Status {
            daemon: DaemonInfo {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                uptime_s: self.started.elapsed().as_secs(),
            },
            route: RouteState {
                desired: self.route_desired,
                active: self.route_active,
                since_unix_ms: self.route_since_unix_ms,
            },
            mute: self.mute,
            arbiter: ArbiterState {
                gpu: GpuState {
                    owner: self.gpu_owner,
                },
            },
            processes: self.processes.clone(),
            pre: self.pre.clone(),
            rvc: self.rvc.clone(),
            asr: self.asr.clone(),
            tts: self.tts.clone(),
            bridge: self.bridge.clone(),
            hw: self.hw.clone(),
            degraded: self.degraded.clone(),
        }
    }

    pub fn route_desired(&self) -> Route {
        self.route_desired
    }

    pub fn route_active(&self) -> Route {
        self.route_active
    }

    pub fn gpu_owner(&self) -> Option<Activity> {
        self.gpu_owner
    }

    /// 最近一次轮询是否拿到 rvc 状态（可接受 LoadModel/StartStream）。
    pub fn rvc_ready(&self) -> bool {
        self.rvc.is_some()
    }

    pub fn mute(&self) -> bool {
        self.mute
    }

    /// 只记录路由意图，不动 active 与 GPU 归属。
    /// 文件任务占用 GPU 时 route set 走这里：rvc 挂等待槽（单槽，后覆盖前），
    /// direct 清等待意图；任务结束后由 poller 应用。
    pub fn set_route_desired(&mut self, route: Route) {
        if self.route_desired != route {
            self.route_since_unix_ms = now_unix_ms();
            self.route_desired = route;
        }
    }

    /// 实时路由切换落地：desired/active/GPU 归属一次写齐。
    /// 不变量：gpu_owner 为 File 时不得调用（route_set 已先行分流），
    /// 且 gpu_owner 为 None 当且仅当 route_active 为 Direct。
    pub fn commit_route(&mut self, route: Route) {
        if self.route_active != route || self.route_desired != route {
            self.route_since_unix_ms = now_unix_ms();
        }
        self.route_desired = route;
        self.route_active = route;
        self.gpu_owner = match route {
            Route::Rvc => Some(Activity::Realtime),
            Route::Direct => None,
        };
    }

    /// 文件任务启动时 claim GPU。调用方（poller 仲裁）持 transition 锁
    /// 且已确认 gpu_owner 为 None。
    pub fn claim_gpu_file(&mut self) {
        self.gpu_owner = Some(Activity::File);
    }

    /// 文件任务收尾时释放 GPU。调用方持 transition 锁；
    /// 任务执行期间路由切换只动 desired，不会改走 File 归属。
    pub fn release_gpu_file(&mut self) {
        self.gpu_owner = None;
    }

    pub fn commit_mute(&mut self, mute: bool) {
        self.mute = mute;
    }

    pub fn set_pre_status(&mut self, status: Option<PreStatus>) {
        self.pre = status;
    }

    pub fn set_rvc_status(&mut self, status: Option<RvcStatus>) {
        self.rvc = status;
    }

    pub fn set_asr_status(&mut self, status: Option<AsrStatus>) {
        self.asr = status;
    }

    pub fn set_tts_status(&mut self, status: Option<TtsStatus>) {
        self.tts = status;
    }

    pub fn set_bridge_status(&mut self, status: Option<BridgeStatus>) {
        self.bridge = status;
    }

    pub fn set_hw(&mut self, hw: Option<HardwareStatus>) {
        self.hw = hw;
    }

    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    pub fn set_degraded(&mut self, reasons: Vec<String>) {
        self.degraded = reasons;
    }

    pub fn push_degraded(&mut self, reason: String) {
        if !self.degraded.contains(&reason) {
            self.degraded.push(reason);
        }
    }

    pub fn process_starting(&mut self, role: Role) {
        let info = self.process_mut(role);
        info.state = ProcessState::Starting;
        info.pid = None;
    }

    pub fn process_running(&mut self, role: Role, pid: u32) {
        let info = self.process_mut(role);
        info.state = ProcessState::Running;
        info.pid = Some(pid);
        info.last_error = None;
    }

    pub fn process_exited(&mut self, role: Role, exit: ExitInfo) {
        let info = self.process_mut(role);
        info.state = ProcessState::Restarting;
        info.pid = None;
        info.restarts += 1;
        info.last_exit = Some(exit);
    }

    pub fn process_failed(&mut self, role: Role, error: String) {
        let info = self.process_mut(role);
        info.state = ProcessState::Restarting;
        info.pid = None;
        info.restarts += 1;
        info.last_error = Some(error);
    }

    pub fn process_stopped(&mut self, role: Role) {
        let info = self.process_mut(role);
        info.state = ProcessState::Stopped;
        info.pid = None;
    }

    fn process_mut(&mut self, role: Role) -> &mut ProcessInfo {
        match role {
            Role::Pre => &mut self.processes.pre,
            Role::Rvc => &mut self.processes.rvc,
            Role::Asr => &mut self.processes.asr,
            Role::Tts => &mut self.processes.tts,
            Role::Bridge => &mut self.processes.bridge,
            Role::Llama => &mut self.processes.llama,
        }
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
