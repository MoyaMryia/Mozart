use mozart_core::ctl::schema::{
    Activity, ArbiterState, DaemonInfo, ExitInfo, GpuState, ProcessInfo, ProcessState, Processes,
    Route, RouteState, Status,
};
use mozart_core::ctl::{self, CODE_NOT_READY};
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Pre,
    Rvc,
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
    degraded: Vec<String>,
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
            },
            degraded: Vec::new(),
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
            pre: None,
            rvc: None,
            hw: None,
            degraded: self.degraded.clone(),
        }
    }

    pub fn set_route(&mut self, route: Route) -> Result<Route, (String, String)> {
        match route {
            Route::Direct => {
                if self.route_active != Route::Direct {
                    self.route_active = Route::Direct;
                    self.route_since_unix_ms = now_unix_ms();
                }
                self.route_desired = Route::Direct;
                self.gpu_owner = None;
                Ok(self.route_active)
            }
            Route::Rvc => Err((
                CODE_NOT_READY.to_string(),
                "rvc process not started".to_string(),
            )),
        }
    }

    pub fn set_mute(&mut self, _mute: bool) -> Result<bool, (String, String)> {
        Err((
            CODE_NOT_READY.to_string(),
            "pre process not started".to_string(),
        ))
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
        }
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
