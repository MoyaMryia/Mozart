use mozart_core::ctl::schema::{
    Activity, ArbiterState, AsrStatus, BridgeStatus, DaemonInfo, EnhanceStatus, ExitInfo, GpuState,
    HardwareStatus, OutputModeStatus, PreStatus, ProcessInfo, ProcessState, Processes, Route, RouteState, RvcStatus,
    Status, TtsStatus,
};
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Enhance,
    Pre,
    Rvc,
    Asr,
    Tts,
    Bridge,
    Llama,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Enhance => "enhance",
            Role::Pre => "pre",
            Role::Rvc => "rvc",
            Role::Asr => "asr",
            Role::Tts => "tts",
            Role::Bridge => "bridge",
            Role::Llama => "llama",
        }
    }

    /// 控制面服务名 → Role；未知名返回 None，由入口拒绝（InvalidArgument）。
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "enhance" => Some(Role::Enhance),
            "pre" => Some(Role::Pre),
            "rvc" => Some(Role::Rvc),
            "asr" => Some(Role::Asr),
            "tts" => Some(Role::Tts),
            "bridge" => Some(Role::Bridge),
            "llama" => Some(Role::Llama),
            _ => None,
        }
    }
}

pub fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RvcLease {
    Idle,
    Realtime,
    ReturningRealtime,
    File,
    ReturningFile,
}

const REALTIME_RETURN_REASON: &str = "rvc realtime return:";
const FILE_RETURN_REASON: &str = "rvc file return:";

pub struct State {
    started: Instant,
    route_desired: Route,
    route_active: Route,
    pub(crate) output_mode: Option<OutputModeStatus>,
    /// True only after supervised startup or a fully confirmed speech stop.
    /// Cleared before any potentially successful admission request.
    pub(crate) speech_drained: bool,
    pub(crate) playout_policy: mozart_core::ctl::schema::PlayoutPolicy,
    pub(crate) playout_startup_id: Option<String>,
    route_since_unix_ms: u64,
    mute: bool,
    rvc_lease: RvcLease,
    processes: Processes,
    enhance: Option<EnhanceStatus>,
    pre: Option<PreStatus>,
    /// 最近一次对账见过的 pre 启动号；变化即 pre 重启，须重新断言模式
    /// （SPEC-04 单一权威：pre 不单边 reclaim，daemon 据 startup_id 重断言）。
    pre_startup_id: Option<String>,
    rvc: Option<RvcStatus>,
    asr: Option<AsrStatus>,
    tts: Option<TtsStatus>,
    bridge: Option<BridgeStatus>,
    hw: Option<HardwareStatus>,
    /// 轮询来源降级：set_degraded 每轮整体替换。
    degraded: Vec<String>,
    /// 持久降级：push_degraded 追加去重，跨轮询存活。停流/恢复失败等
    /// 一次性事件的原因走这里，不被下一秒轮询覆写（AGENTS「降级记可查原因」）。
    degraded_persistent: Vec<String>,
    /// 写了重启键但尚未重启的服务（SPEC-04 待重启显示）：config set 重启键加入，
    /// restart 该服务后清除。崩溃重启复用旧配置快照，不清（分歧仍在）。
    pending_restart: std::collections::BTreeSet<String>,
    /// 键级待重启：键 → 尚未重启的受影响服务集合。config get 读到的是基线新值，
    /// 靠这份映射标出该值尚未生效（服务级集合看不出是哪个键）。
    /// 一个键影响多个服务时，全部服务都重启后才算生效。
    pending_restart_keys: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    shutting_down: bool,
}

impl State {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            route_desired: Route::Direct,
            route_active: Route::Direct,
            output_mode: None,
            speech_drained: false,
            playout_policy: mozart_core::ctl::schema::PlayoutPolicy::Legacy,
            playout_startup_id: None,
            route_since_unix_ms: now_unix_ms(),
            mute: false,
            rvc_lease: RvcLease::Idle,
            pre_startup_id: None,
            processes: Processes {
                enhance: ProcessInfo::stopped(),
                pre: ProcessInfo::stopped(),
                rvc: ProcessInfo::stopped(),
                asr: ProcessInfo::stopped(),
                tts: ProcessInfo::stopped(),
                bridge: ProcessInfo::stopped(),
                llama: ProcessInfo::stopped(),
            },
            enhance: None,
            pre: None,
            rvc: None,
            asr: None,
            tts: None,
            bridge: None,
            hw: None,
            degraded: Vec::new(),
            degraded_persistent: Vec::new(),
            pending_restart: std::collections::BTreeSet::new(),
            pending_restart_keys: std::collections::BTreeMap::new(),
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
            output_mode: self.output_mode.clone(),
            mute: self.mute,
            arbiter: ArbiterState {
                file_return_pending: self.file_return_pending(),
                gpu: GpuState {
                    owner: self.gpu_owner(),
                },
            },
            processes: self.processes.clone(),
            enhance: self.enhance.clone(),
            pre: self.pre.clone(),
            rvc: self.rvc.clone(),
            // rvc_params 由 server 生成 DAEMON_GET_STATUS 应答时按生效配置填入。
            rvc_params: None,
            asr: self.asr.clone(),
            tts: self.tts.clone(),
            bridge: self.bridge.clone(),
            hw: self.hw.clone(),
            // 轮询原因在前，持久原因在后；同一原因两边都有时只报一次。
            degraded: self
                .degraded
                .iter()
                .chain(
                    self.degraded_persistent
                        .iter()
                        .filter(|reason| !self.degraded.contains(reason)),
                )
                .cloned()
                .collect(),
            pending_restart: self.pending_restart.iter().cloned().collect(),
            pending_restart_keys: self.pending_restart_keys(),
        }
    }

    /// 待重启的键名，按键排序（BTreeMap 键序）。
    pub fn pending_restart_keys(&self) -> Vec<String> {
        self.pending_restart_keys.keys().cloned().collect()
    }

    pub fn route_desired(&self) -> Route {
        self.route_desired
    }

    pub fn route_active(&self) -> Route {
        self.route_active
    }

    pub fn gpu_owner(&self) -> Option<Activity> {
        match self.rvc_lease {
            RvcLease::Idle => None,
            RvcLease::Realtime | RvcLease::ReturningRealtime => Some(Activity::Realtime),
            RvcLease::File | RvcLease::ReturningFile => Some(Activity::File),
        }
    }

    pub fn realtime_return_pending(&self) -> bool {
        self.rvc_lease == RvcLease::ReturningRealtime
    }

    /// A stop ACK is evidence the RVC worker has finished. A lost or rejected
    /// ACK is not evidence of release, even with the output routed to direct.
    pub fn record_realtime_return(&mut self, failure: Option<String>) {
        self.clear_degraded_prefix(REALTIME_RETURN_REASON);
        self.rvc_lease = match failure {
            None => RvcLease::Idle,
            Some(reason) => {
                self.push_degraded(format!("{REALTIME_RETURN_REASON} {reason}"));
                RvcLease::ReturningRealtime
            }
        };
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

    /// 提交路由意图和已确认的路由。直通不自动释放 RVC 租约。
    pub fn commit_route(&mut self, route: Route) {
        if self.route_active != route || self.route_desired != route {
            self.route_since_unix_ms = now_unix_ms();
        }
        self.route_desired = route;
        self.route_active = route;
        if route == Route::Rvc {
            self.rvc_lease = RvcLease::Realtime;
        }
    }

    /// 文件任务启动时 claim GPU。调用方（poller 仲裁）持 transition 锁
    /// 且已确认 gpu_owner 为 None。
    pub fn claim_gpu_file(&mut self) {
        self.rvc_lease = RvcLease::File;
    }

    /// 文件任务收尾时释放 GPU。调用方持 transition 锁；
    /// 任务执行期间路由切换只动 desired，不会改走 File 归属。
    pub fn release_gpu_file(&mut self) {
        self.clear_degraded_prefix(FILE_RETURN_REASON);
        self.rvc_lease = RvcLease::Idle;
    }

    pub fn record_file_return_pending(&mut self, reason: String) {
        self.clear_degraded_prefix(FILE_RETURN_REASON);
        self.push_degraded(format!("{FILE_RETURN_REASON} {reason}"));
        self.rvc_lease = RvcLease::ReturningFile;
    }

    pub fn file_return_pending(&self) -> bool {
        self.rvc_lease == RvcLease::ReturningFile
    }

    pub fn commit_mute(&mut self, mute: bool) {
        self.mute = mute;
    }

    pub fn set_enhance_status(&mut self, status: Option<EnhanceStatus>) {
        self.enhance = status;
    }

    pub fn set_pre_status(&mut self, status: Option<PreStatus>) {
        self.pre = status;
    }

    /// 最近一次轮询的 pre 文件接管状态（PreStatus.file_mode）。
    /// None 表示本轮未取得 pre 状态，不能作为失接管证据。
    pub fn pre_file_mode(&self) -> Option<bool> {
        self.pre.as_ref().map(|pre| pre.file_mode)
    }

    /// 最近一次对账见过的 pre 启动号（None 表示尚未见过 pre）。
    pub fn pre_startup_id(&self) -> Option<&str> {
        self.pre_startup_id.as_deref()
    }

    /// 记录本轮见到的 pre 启动号，供下一轮识别重启。
    pub fn set_pre_startup_id(&mut self, startup_id: Option<String>) {
        self.pre_startup_id = startup_id;
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

    /// 最近一次监管记录的桥进程 PID（轻量原子/字段读取，避免克隆整份 Status）。
    pub fn bridge_pid(&self) -> Option<u32> {
        self.processes.bridge.pid
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

    /// 轮询来源降级：每轮整体替换。持久原因（push_degraded）不受影响。
    pub fn set_degraded(&mut self, reasons: Vec<String>) {
        self.degraded = reasons;
    }

    /// 持久降级：追加去重，跨轮询存活，status 恒可查。
    pub fn push_degraded(&mut self, reason: String) {
        if !self.degraded_persistent.contains(&reason) {
            self.degraded_persistent.push(reason);
        }
    }

    /// 某类持久降级原因解除后清除（如预设目录恢复可解析）。
    pub fn clear_degraded_prefix(&mut self, prefix: &str) {
        self.degraded_persistent
            .retain(|reason| !reason.starts_with(prefix));
    }

    /// 写重启键后登记受影响服务与该键待重启（SPEC-04）。
    /// affected 由 core::config::set_key 给出，这里不重新推导键的分类。
    pub fn mark_pending_restart(&mut self, key: &str, affected: &[String]) {
        for service in affected {
            self.pending_restart.insert(service.clone());
        }
        self.pending_restart_keys
            .entry(key.to_string())
            .or_default()
            .extend(affected.iter().cloned());
    }

    /// 单服务重启受理后清除其待重启标记（重读 TOML，分歧消除）。
    /// 键级标记只去掉该服务；键影响多个服务时，全部重启后才删键。
    pub fn clear_pending_restart(&mut self, service: &str) {
        self.pending_restart.remove(service);
        self.pending_restart_keys.retain(|_, owners| {
            owners.remove(service);
            !owners.is_empty()
        });
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
            Role::Enhance => &mut self.processes.enhance,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pre_status(file_mode: bool) -> PreStatus {
        serde_json::from_value(json!({
            "state": "ok", "route": "direct", "mute": false,
            "capture_device": "", "playback_device": "",
            "frames_captured": 0, "frames_played": 0, "frames_dropped": 0,
            "queue_depth": 0, "consumer_drops": {},
            "speaker_state": "realtime", "tts_frames_played": 0,
            "file_mode": file_mode, "last_error": null, "version": "0.1.0"
        }))
        .unwrap()
    }

    /// 回归（Important #3）：持久降级原因必须跨轮询存活。
    /// 旧实现 set_degraded 整体替换唯一 vec，push_degraded 的恢复/停流失败
    /// 原因下一秒即被轮询抹掉，Completed 任务上的恢复失败无可查原因。
    #[test]
    fn persistent_degraded_survives_poll_overwrite() {
        let mut state = State::new();
        state.push_degraded("pre file mode restore: NotReady: timed out".to_string());
        state.set_degraded(vec!["pre status: connection refused".to_string()]);
        let degraded = state.status().degraded;
        assert!(
            degraded
                .iter()
                .any(|reason| reason.contains("pre file mode restore")),
            "持久原因必须与轮询原因并存：{degraded:?}"
        );
        assert!(degraded.iter().any(|reason| reason.contains("pre status")));

        // 后续轮询（含空轮询）只替换轮询来源，持久原因仍在。
        state.set_degraded(Vec::new());
        let degraded = state.status().degraded;
        assert!(
            degraded
                .iter()
                .any(|reason| reason.contains("pre file mode restore")),
            "空轮询不得抹掉持久原因：{degraded:?}"
        );
        assert!(!degraded.iter().any(|reason| reason.contains("pre status")));
    }

    #[test]
    fn clear_degraded_prefix_removes_only_matching() {
        let mut state = State::new();
        state.push_degraded("preset: cannot resolve model directory for 'a'".to_string());
        state.push_degraded("preset: cannot resolve model directory for 'b'".to_string());
        state.push_degraded("file stop stream: Busy".to_string());
        state.clear_degraded_prefix("preset: cannot resolve model directory for 'a'");
        let degraded = state.status().degraded;
        assert!(!degraded.iter().any(|reason| reason.contains("for 'a'")));
        assert!(degraded.iter().any(|reason| reason.contains("for 'b'")));
        assert!(degraded
            .iter()
            .any(|reason| reason.contains("file stop stream")));
    }

    #[test]
    fn push_degraded_dedups_and_poll_reasons_replace() {
        let mut state = State::new();
        state.push_degraded("file stop stream: Busy: stream active".to_string());
        state.push_degraded("file stop stream: Busy: stream active".to_string());
        state.set_degraded(vec!["rvc status: old".to_string()]);
        state.set_degraded(vec!["rvc status: new".to_string()]);
        let degraded = state.status().degraded;
        assert_eq!(
            degraded
                .iter()
                .filter(|reason| reason.contains("file stop stream"))
                .count(),
            1,
            "同一持久原因只报一次：{degraded:?}"
        );
        assert!(degraded
            .iter()
            .any(|reason| reason.contains("rvc status: new")));
        assert!(!degraded
            .iter()
            .any(|reason| reason.contains("rvc status: old")));
    }

    #[test]
    fn pending_restart_marks_and_clears_per_service() {
        let mut state = State::new();
        assert!(state.status().pending_restart.is_empty());
        assert!(state.status().pending_restart_keys.is_empty());
        state.mark_pending_restart("pre.capture_device", &["pre".to_string()]);
        // 状态机不分类键：多服务键用合成名，只验服务集合与键映射的清除口径。
        state.mark_pending_restart("multi.service", &["pre".to_string(), "rvc".to_string()]);
        let status = state.status();
        assert_eq!(status.pending_restart, vec!["pre", "rvc"]);
        assert_eq!(
            status.pending_restart_keys,
            vec!["multi.service", "pre.capture_device"],
            "键级标记按键排序"
        );
        // 清 pre 只清 pre 独占的键；rvc 仍待重启的键保留。
        state.clear_pending_restart("pre");
        let status = state.status();
        assert_eq!(status.pending_restart, vec!["rvc"]);
        assert_eq!(status.pending_restart_keys, vec!["multi.service"]);
        state.clear_pending_restart("rvc");
        let status = state.status();
        assert!(status.pending_restart.is_empty());
        assert!(status.pending_restart_keys.is_empty());

        // 反序清除：先重启 rvc，多服务键仍待 pre 重启，不得提前删。
        state.mark_pending_restart("multi.service", &["pre".to_string(), "rvc".to_string()]);
        state.clear_pending_restart("rvc");
        assert_eq!(state.status().pending_restart_keys, vec!["multi.service"]);
        state.clear_pending_restart("pre");
        assert!(state.status().pending_restart_keys.is_empty());
    }

    #[test]
    fn pending_restart_keys_clear_only_for_restarted_service() {
        let mut state = State::new();
        state.mark_pending_restart("pre.capture_device", &["pre".to_string()]);
        state.mark_pending_restart("pre.playback_device", &["pre".to_string()]);
        // 重启别的服务不影响本服务的键。
        state.clear_pending_restart("rvc");
        assert_eq!(
            state.status().pending_restart_keys,
            vec!["pre.capture_device", "pre.playback_device"]
        );
        state.clear_pending_restart("pre");
        assert!(state.status().pending_restart_keys.is_empty());
    }

    #[test]
    fn pre_file_mode_reads_latest_poll() {
        let mut state = State::new();
        assert_eq!(state.pre_file_mode(), None, "未取得 pre 状态时不可判定");
        state.set_pre_status(Some(pre_status(true)));
        assert_eq!(state.pre_file_mode(), Some(true));
        state.set_pre_status(Some(pre_status(false)));
        assert_eq!(state.pre_file_mode(), Some(false));
        state.set_pre_status(None);
        assert_eq!(state.pre_file_mode(), None);
    }
}
