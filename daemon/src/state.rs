use mozart_core::ctl::schema::{
    ArbiterState, DaemonInfo, GpuState, ProcessInfo, Processes, Route, RouteState, Status,
};
use mozart_core::ctl::{self, CODE_NOT_READY};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct State {
    started: Instant,
    route_desired: Route,
    route_active: Route,
    route_since_unix_ms: u64,
    mute: bool,
    gpu_owner: Option<ctl::schema::Activity>,
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
            processes: Processes {
                pre: ProcessInfo::stopped(),
                rvc: ProcessInfo::stopped(),
            },
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

    pub fn set_mute(&mut self, mute: bool) -> Result<bool, (String, String)> {
        let _ = mute;
        Err((
            CODE_NOT_READY.to_string(),
            "pre process not started".to_string(),
        ))
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
