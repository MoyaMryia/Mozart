//! 引擎共享状态：命令目标与计数全部走原子，控制线程写、音频线程只读；
//! `last_error` 用互斥锁保护，只在降级事件写入，不跨任何 I/O 持有。

use mozart_core::config::{PreConsumerConfig, PreDspConfig};
use mozart_core::ctl;
use mozart_core::ctl::schema::{PlayoutPolicy, PreStatus, Route};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

const SWITCH_POLL: Duration = Duration::from_millis(20);

struct SwitchRequest {
    target: String,
    reply: mpsc::Sender<Result<String, String>>,
}

struct DeviceSwitchInner {
    current: String,
    /// 当前是否已实际持有打开的 ALSA 句柄；启动打开失败待机时为 false，
    /// 允许后续对同设备名发起 PRE_SET_DEVICE 重试打开。
    active: bool,
    locked_reason: Option<String>,
    request: Option<SwitchRequest>,
}

/// 音频设备热切换槽（采集/播放各持一槽）：
/// 音频线程稳态热路径只读 `pending` 原子，不加锁；只有收到切换命令时才短暂拿锁取目标，
/// 并在锁外执行 ALSA open，避免阻塞 GetStatus 与其他命令。
pub struct DeviceSwitchSlot {
    pending: AtomicBool,
    request_lock: Mutex<()>,
    inner: Mutex<DeviceSwitchInner>,
}

impl DeviceSwitchSlot {
    pub fn new(initial: String) -> Self {
        Self {
            pending: AtomicBool::new(false),
            request_lock: Mutex::new(()),
            inner: Mutex::new(DeviceSwitchInner {
                current: initial,
                active: true,
                locked_reason: None,
                request: None,
            }),
        }
    }

    pub fn mark_standby(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active = false;
    }

    pub fn is_active(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active
    }

    pub fn lock(&self, reason: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .locked_reason = Some(reason.to_string());
    }

    pub fn current(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current
            .clone()
    }

    pub fn pending_flag(&self) -> &AtomicBool {
        &self.pending
    }

    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// 控制线程发起热切换并等待音频线程在循环边界完成；返回旧设备名或拒绝原因。
    pub fn request_switch(
        &self,
        target: &str,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> Result<String, (String, String)> {
        let _guard = self
            .request_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stop.load(Ordering::Relaxed) {
            return Err((
                ctl::CODE_BUSY.to_string(),
                "pre is shutting down".to_string(),
            ));
        }
        let rx = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(reason) = &inner.locked_reason {
                return Err((ctl::CODE_INVALID_ARGUMENT.to_string(), reason.clone()));
            }
            if inner.active && inner.current == target {
                return Ok(inner.current.clone());
            }
            let (tx, rx) = mpsc::channel();
            inner.request = Some(SwitchRequest {
                target: target.to_string(),
                reply: tx,
            });
            self.pending.store(true, Ordering::Release);
            rx
        };
        let deadline = Instant::now() + timeout;
        loop {
            if stop.load(Ordering::Relaxed) {
                self.clear_pending();
                return Err((
                    ctl::CODE_BUSY.to_string(),
                    "pre is shutting down".to_string(),
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.clear_pending();
                return Err((
                    ctl::CODE_NOT_READY.to_string(),
                    "audio device switch timed out".to_string(),
                ));
            }
            match rx.recv_timeout(remaining.min(SWITCH_POLL)) {
                Ok(Ok(previous)) => return Ok(previous),
                Ok(Err(message)) => {
                    return Err((ctl::CODE_INVALID_ARGUMENT.to_string(), message));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.clear_pending();
                    return Err((
                        ctl::CODE_NOT_READY.to_string(),
                        "audio thread stopped during device switch".to_string(),
                    ));
                }
            }
        }
    }

    fn clear_pending(&self) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.request = None;
        self.pending.store(false, Ordering::Release);
    }

    /// 音频线程在循环边界调用：取走待切目标，锁外打开新设备；成功才替换当前设备与句柄。
    pub fn apply_pending<S>(
        &self,
        opener: impl FnOnce(&str) -> Result<S, String>,
        on_success: impl FnOnce(&str, S),
    ) {
        let req = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let req = inner.request.take();
            self.pending.store(false, Ordering::Release);
            req
        };
        let Some(req) = req else {
            return;
        };
        match opener(&req.target) {
            Ok(stream) => {
                let prev = {
                    let mut inner = self
                        .inner
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    inner.active = true;
                    std::mem::replace(&mut inner.current, req.target.clone())
                };
                on_success(&req.target, stream);
                let _ = req.reply.send(Ok(prev));
            }
            Err(err) => {
                let _ = req.reply.send(Err(err));
            }
        }
    }
}

/// 版本号，Hello 与 GetStatus 一致上报。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn route_code(route: Route) -> u8 {
    match route {
        Route::Direct => 0,
        Route::Rvc => 1,
    }
}

fn route_from_code(code: u8) -> Route {
    match code {
        0 => Route::Direct,
        1 => Route::Rvc,
        // 写入只经 route_code，编码封闭。
        _ => unreachable!("route encoding is closed over 0..=1"),
    }
}

/// 扬声器分时状态（SPEC-05 播报投递）：实时与播报共用扬声器，播报优先，
/// 播报空窗 holdoff 后恢复实时。播放线程写、控制线程只读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Realtime,
    Broadcast,
}

fn speaker_code(speaker: Speaker) -> u8 {
    match speaker {
        Speaker::Realtime => 0,
        Speaker::Broadcast => 1,
    }
}

fn speaker_from_code(code: u8) -> Speaker {
    match code {
        0 => Speaker::Realtime,
        1 => Speaker::Broadcast,
        // 写入只经 speaker_code，编码封闭。
        _ => unreachable!("speaker encoding is closed over 0..=1"),
    }
}

fn speaker_name(speaker: Speaker) -> &'static str {
    match speaker {
        Speaker::Realtime => "realtime",
        Speaker::Broadcast => "broadcast",
    }
}

/// 状态快照需要的队列读数；控制线程组装 status 时传入。
pub struct QueueReadout {
    /// 采集 → DSP 环深度（帧）。
    pub depth: u64,
    /// 采集环与主干环满丢最旧的合计（帧）。
    pub dropped: u64,
    /// AI 接收环满丢最旧（帧）。
    pub rvc_dropped: u64,
    /// 播报接收环满丢最旧（帧）。
    pub tts_dropped: u64,
}

/// 消费者运行时槽位（SPEC-02 分发配置化）：声明启动时固定，发送丢弃
/// 计数由处理线程发布绝对值。一个消费者一个 socket，计数互相独立。
pub struct Consumer {
    /// 消费者名：GetStatus consumer_drops 的键。
    pub name: String,
    /// 是否受文件接管影响：true 按 rvc_ai_active 门控发送（rvc 语义），
    /// false 恒发（asr 语义）。
    pub file_mode_affected: bool,
    send_dropped: AtomicU64,
}

impl Consumer {
    fn from_config(config: PreConsumerConfig) -> Self {
        Self {
            name: config.name,
            file_mode_affected: config.file_mode_affected,
            send_dropped: AtomicU64::new(0),
        }
    }

    /// 发布输入帧发送丢弃的累计绝对值（处理线程调用）。
    pub fn set_send_dropped(&self, total: u64) {
        self.send_dropped.store(total, Ordering::Relaxed);
    }
}

/// DSP 级开关目标（[pre.dsp] 基线 / SetDsp 运行时命令）。打包为一个字节
/// 原子发布：四个开关作为一致快照生效，处理线程逐帧读、变化时应用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DspSwitches {
    pub denoise: bool,
    pub gate: bool,
    pub agc: bool,
    pub limiter: bool,
}

impl DspSwitches {
    pub fn from_config(dsp: &PreDspConfig) -> Self {
        Self {
            denoise: dsp.denoise,
            gate: dsp.gate,
            agc: dsp.agc,
            limiter: dsp.limiter,
        }
    }

    fn code(self) -> u8 {
        u8::from(self.denoise)
            | (u8::from(self.gate) << 1)
            | (u8::from(self.agc) << 2)
            | (u8::from(self.limiter) << 3)
    }

    fn from_code(code: u8) -> Self {
        Self {
            denoise: code & 1 != 0,
            gate: code & 2 != 0,
            agc: code & 4 != 0,
            limiter: code & 8 != 0,
        }
    }
}

#[derive(Clone, Copy)]
pub enum OutputBinding {
    Released,
    Binding,
    Bound,
}

pub struct Shared {
    pub speech_cutoff: AtomicU64,
    pub playback_request: AtomicU64,
    pub playback_ack: AtomicU64,
    playout_policy: AtomicU8,
    route_target: AtomicU8,
    mute_target: AtomicBool,
    /// 文件接管状态（SPEC-03 状态门控交接）：控制线程写，处理线程与
    /// rvc 接收线程只读。true 时 rvc AI 发送暂停、output_socket 释放给 daemon。
    file_gate: AtomicU64,
    in_flight: AtomicUsize,
    /// daemon 权威断言标志（SPEC-04 单一权威）：启动为 false，rvc AI 支路
    /// （发 input_socket + 绑 output_socket）静默，等 daemon 首个 PRE_SET_FILE_MODE
    /// 断言才激活。控制线程写，处理线程与 rvc 接收线程只读。pre 不单边 reclaim。

    /// 输出 socket 交接握手：rvc 接收线程解绑后置 true、重绑后清 false；
    /// 控制线程在 PRE_SET_FILE_MODE{active:true} 应答前等它置位。
    output_binding: AtomicU8,
    frames_captured: AtomicU64,
    frames_played: AtomicU64,
    /// 消费者列表（启动时按配置固定）：每个消费者一个输入发送丢弃计数槽，
    /// 处理线程发布绝对值，控制线程组装 status 时读取。
    consumers: Vec<Consumer>,
    /// DSP 级开关目标：控制线程（SetDsp）写，处理线程逐帧读、变化时应用。
    dsp_target: AtomicU64,
    /// 采集设备切换代次：采集热切换成功后递增，触发处理线程重置 WOLA 与增强会话。
    capture_epoch: AtomicU64,
    pub enhancement: Option<std::sync::Arc<crate::enhancement::Enhancement>>,
    /// 在线 AEC 开关与遥测（控制线程读写开关，处理线程维护遥测）。
    aec_enabled: AtomicBool,
    aec: std::sync::Arc<crate::aec::AecTelemetry>,
    /// AI 输出帧坏包丢弃，接收线程发布绝对值。
    ai_recv_dropped: AtomicU64,
    /// TTS 播报帧坏包丢弃，接收线程发布绝对值。
    tts_recv_dropped: AtomicU64,
    /// 输出帧尺寸不符计数（新旧进程混跑），接收线程发布绝对值；
    /// 控制线程首次见到非零时记录原因，实时线程只写原子。
    ai_recv_size_mismatch: AtomicU64,
    tts_recv_size_mismatch: AtomicU64,
    /// direct 路由期间丢弃的 rvc 输出帧（旧 skipped_replies），播放线程累加。
    ai_skipped: AtomicU64,
    /// 扬声器分时状态，播放线程在帧边界写、控制线程只读。
    speaker_state: AtomicU8,
    /// 播报帧播放累计，播放线程累加。
    tts_frames_played: AtomicU64,
    rvc_playout_gap_frames: AtomicU64,
    playback_underruns: AtomicU64,
    last_error: Mutex<Option<String>>,
    pub capture_switch: DeviceSwitchSlot,
    pub playback_switch: DeviceSwitchSlot,
    /// 进程启动号：每次启动随机生成，GetStatus 上报供 daemon 识别重启。
    startup_id: String,
}

pub struct SendPermit<'a>(&'a Shared);

impl Drop for SendPermit<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    pub fn new(
        capture_device: String,
        playback_device: String,
        startup_id: String,
        consumers: Vec<PreConsumerConfig>,
        dsp: DspSwitches,
    ) -> Self {
        Self {
            speech_cutoff: AtomicU64::new(0),
            playback_request: AtomicU64::new(0),
            playback_ack: AtomicU64::new(0),
            playout_policy: AtomicU8::new(PlayoutPolicy::Legacy as u8),
            route_target: AtomicU8::new(route_code(Route::Direct)),
            mute_target: AtomicBool::new(false),
            file_gate: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            output_binding: AtomicU8::new(OutputBinding::Released as u8),
            frames_captured: AtomicU64::new(0),
            frames_played: AtomicU64::new(0),
            consumers: consumers.into_iter().map(Consumer::from_config).collect(),
            dsp_target: AtomicU64::new(dsp.code().into()),
            capture_epoch: AtomicU64::new(0),
            enhancement: None,
            aec_enabled: AtomicBool::new(false),
            aec: std::sync::Arc::new(crate::aec::AecTelemetry::new(
                crate::aec::default_backend_name(),
            )),
            ai_recv_dropped: AtomicU64::new(0),
            tts_recv_dropped: AtomicU64::new(0),
            ai_recv_size_mismatch: AtomicU64::new(0),
            tts_recv_size_mismatch: AtomicU64::new(0),
            ai_skipped: AtomicU64::new(0),
            speaker_state: AtomicU8::new(speaker_code(Speaker::Realtime)),
            tts_frames_played: AtomicU64::new(0),
            rvc_playout_gap_frames: AtomicU64::new(0),
            playback_underruns: AtomicU64::new(0),
            last_error: Mutex::new(None),
            capture_switch: DeviceSwitchSlot::new(capture_device),
            playback_switch: DeviceSwitchSlot::new(playback_device),
            startup_id,
        }
    }

    /// 命令目标路由；播放线程在帧边界读入状态机。
    pub fn route_target(&self) -> Route {
        route_from_code(self.route_target.load(Ordering::Relaxed))
    }

    pub fn set_route_target(&self, route: Route) {
        self.route_target
            .store(route_code(route), Ordering::Relaxed);
    }

    pub fn mute_target(&self) -> bool {
        self.mute_target.load(Ordering::Relaxed)
    }

    pub fn set_mute_target(&self, mute: bool) {
        self.mute_target.store(mute, Ordering::Relaxed);
    }

    /// 文件接管状态；处理线程据此门控 rvc AI 发送，接收线程据此交接绑定。
    pub fn file_mode(&self) -> bool {
        self.file_gate.load(Ordering::SeqCst) & 1 != 0
    }

    pub fn set_file_mode(&self, active: bool) {
        self.update_file_gate(1, active);
    }

    /// daemon 是否已权威断言模式；启动为 false，首个 PRE_SET_FILE_MODE 置 true。
    pub fn mode_asserted(&self) -> bool {
        self.file_gate.load(Ordering::SeqCst) & 2 != 0
    }

    pub fn set_mode_asserted(&self, asserted: bool) {
        self.update_file_gate(2, asserted);
    }

    /// rvc AI 支路是否活跃（SPEC-04 单一权威不变量）：仅当 daemon 已断言
    /// 且非文件接管时为 true。处理线程据此门控发送、接收线程据此门控绑定。
    /// 读取一致门控，热路径不分配不阻塞。asr 支路与主干直出不受此门控。
    pub fn rvc_ai_active(&self) -> bool {
        self.file_gate.load(Ordering::SeqCst) & 3 == 2
    }

    fn update_file_gate(&self, bit: u64, enabled: bool) {
        self.file_gate
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                if (old & bit != 0) == enabled {
                    None
                } else {
                    Some(((old + 4) & !bit) | if enabled { bit } else { 0 })
                }
            })
            .ok();
    }

    #[cfg(test)]
    pub fn file_generation(&self) -> u64 {
        self.file_gate.load(Ordering::SeqCst) >> 2
    }

    /// 采集侧一次读取许可与代次，避免交接期间将旧许可标为新代次。
    pub fn file_snapshot(&self) -> (bool, u64) {
        let gate = self.file_gate.load(Ordering::SeqCst);
        (gate & 3 == 2, gate >> 2)
    }

    /// 先登记再复核；关闭门控后控制线程等待已获许可的发送退出。
    pub fn begin_ai_send(&self, source_generation: u64) -> Option<SendPermit<'_>> {
        let expected = (source_generation << 2) | 2;
        if self.file_gate.load(Ordering::SeqCst) != expected {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let permit = SendPermit(self);
        if self.file_gate.load(Ordering::SeqCst) != expected {
            return None;
        }
        Some(permit)
    }

    pub fn sends_idle(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0
    }

    pub fn set_playout_policy(&self, policy: PlayoutPolicy) {
        self.playout_policy.store(policy as u8, Ordering::SeqCst);
    }

    pub fn playout_policy(&self) -> PlayoutPolicy {
        match self.playout_policy.load(Ordering::SeqCst) {
            p if p == PlayoutPolicy::RealtimeOnly as u8 => PlayoutPolicy::RealtimeOnly,
            p if p == PlayoutPolicy::BroadcastOnly as u8 => PlayoutPolicy::BroadcastOnly,
            p if p == PlayoutPolicy::Silent as u8 => PlayoutPolicy::Silent,
            _ => PlayoutPolicy::Legacy,
        }
    }

    pub fn output_released(&self) -> bool {
        self.output_binding.load(Ordering::SeqCst) == OutputBinding::Released as u8
    }

    pub fn output_bound(&self) -> bool {
        self.output_binding.load(Ordering::SeqCst) == OutputBinding::Bound as u8
    }

    pub fn set_output_binding(&self, binding: OutputBinding) {
        self.output_binding.store(binding as u8, Ordering::SeqCst);
    }

    pub fn add_captured(&self, frames: u64) {
        self.frames_captured.fetch_add(frames, Ordering::Relaxed);
    }

    pub fn add_played(&self, frames: u64) {
        self.frames_played.fetch_add(frames, Ordering::Relaxed);
    }

    /// 消费者列表（启动时固定）：处理线程按索引读文件接管语义、发布
    /// 发送丢弃计数；控制线程读名字与计数组装 status。
    pub fn consumers(&self) -> &[Consumer] {
        &self.consumers
    }

    /// DSP 级开关目标；处理线程逐帧读，与已应用值比较，变化时应用。
    pub fn dsp_target(&self) -> DspSwitches {
        self.dsp_snapshot().0
    }

    /// SetDsp 命令目标：控制线程写，处理线程在一帧内应用。
    pub fn set_dsp_target(&self, dsp: DspSwitches) {
        self.dsp_target
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                let epoch = (old >> 4) + u64::from((old as u8 & 1) != (dsp.code() & 1));
                Some((epoch << 4) | u64::from(dsp.code()))
            })
            .unwrap();
    }

    /// 开关与 epoch 同一原子快照，避免快速关开漏掉会话重置。
    pub fn dsp_snapshot(&self) -> (DspSwitches, u64) {
        let value = self.dsp_target.load(Ordering::Acquire);
        (DspSwitches::from_code(value as u8), value >> 4)
    }

    pub fn aec_enabled(&self) -> bool {
        self.aec_enabled.load(Ordering::Acquire)
    }

    pub fn set_aec_enabled(&self, enabled: bool) {
        self.aec_enabled.store(enabled, Ordering::Release);
    }

    pub fn aec_telemetry(&self) -> std::sync::Arc<crate::aec::AecTelemetry> {
        self.aec.clone()
    }

    pub fn enhancement_epoch(&self) -> u64 {
        self.dsp_snapshot().1.wrapping_add(self.capture_epoch())
    }

    pub fn capture_epoch(&self) -> u64 {
        self.capture_epoch.load(Ordering::Acquire)
    }

    pub fn bump_capture_epoch(&self) -> u64 {
        self.capture_epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn configure_enhancement(
        &mut self,
        config: &mozart_core::config::EnhanceConfig,
        run_dir: &std::path::Path,
    ) {
        self.enhancement = Some(std::sync::Arc::new(crate::enhancement::Enhancement::new(
            config, run_dir,
        )));
    }

    /// 发布 AI 输出帧坏包丢弃的累计绝对值（接收线程调用）。
    pub fn set_ai_recv_dropped(&self, total: u64) {
        self.ai_recv_dropped.store(total, Ordering::Relaxed);
    }

    /// 发布 TTS 播报帧坏包丢弃的累计绝对值（接收线程调用）。
    pub fn set_tts_recv_dropped(&self, total: u64) {
        self.tts_recv_dropped.store(total, Ordering::Relaxed);
    }

    /// 发布输出帧尺寸不符的累计绝对值（接收线程调用，只写原子）。
    pub fn set_ai_recv_size_mismatch(&self, total: u64) {
        self.ai_recv_size_mismatch.store(total, Ordering::Relaxed);
    }

    pub fn set_tts_recv_size_mismatch(&self, total: u64) {
        self.tts_recv_size_mismatch.store(total, Ordering::Relaxed);
    }

    /// 尺寸不符计数（控制线程读，首次非零时记录混跑原因）。
    pub fn ai_recv_size_mismatch(&self) -> u64 {
        self.ai_recv_size_mismatch.load(Ordering::Relaxed)
    }

    pub fn tts_recv_size_mismatch(&self) -> u64 {
        self.tts_recv_size_mismatch.load(Ordering::Relaxed)
    }

    /// direct 路由期间丢弃的 rvc 输出帧计数（播放线程调用）。
    pub fn add_ai_skipped(&self, frames: u64) {
        self.ai_skipped.fetch_add(frames, Ordering::Relaxed);
    }

    /// 扬声器分时状态；播放线程在帧边界写，控制线程只读。
    pub fn speaker_state(&self) -> Speaker {
        speaker_from_code(self.speaker_state.load(Ordering::Relaxed))
    }

    pub fn set_speaker_state(&self, speaker: Speaker) {
        self.speaker_state
            .store(speaker_code(speaker), Ordering::Relaxed);
    }

    /// 播报帧播放计数（播放线程调用）。
    pub fn add_rvc_playout_gap(&self) {
        self.rvc_playout_gap_frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_playback_underruns(&self, underruns: u64) {
        self.playback_underruns.store(underruns, Ordering::Relaxed);
    }

    pub fn add_tts_frames_played(&self, frames: u64) {
        self.tts_frames_played.fetch_add(frames, Ordering::Relaxed);
    }

    /// 记录最近降级原因（AGENTS：降级可经状态接口查询）。
    /// 只在降级路径调用：错误分支允许分配，稳态热路径不触碰。
    pub fn set_last_error(&self, message: &str) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message.to_string());
    }

    /// 采集与播放双侧均已活跃时，清理启动阶段留下的 `playback/capture open (...)` 待机告警。
    pub fn clear_startup_open_error_if_ready(&self) {
        if !(self.capture_switch.is_active() && self.playback_switch.is_active()) {
            return;
        }
        let mut slot = self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot
            .as_deref()
            .is_some_and(|msg| msg.starts_with("playback open (") || msg.starts_with("capture open ("))
        {
            *slot = None;
        }
    }

    /// 组装 GetStatus 快照：只读原子与队列读数，不阻塞音频线程。
    pub fn status(&self, queue: QueueReadout) -> PreStatus {
        // consumer_drops 口径（SPEC-02 / SPEC-05）：键随配置消费者列表动态
        // 生成，每个消费者报告输入发送丢弃（含未建链期）。名为 "rvc" 的
        // 消费者额外聚合输出链路：
        // - "rvc"：AI 链路异常丢弃 = 输入发送丢弃 + 输出坏包丢弃 + 接收环满；
        // - "rvc.skipped"：direct 路由期间丢弃的输出帧（旧 skipped_replies，
        //   设计内丢弃，恒发链路在 direct 下必然增长）；
        // - "tts"：播报链路固定键 = 播报帧坏包丢弃 + 播报接收环满。
        let mut consumer_drops = BTreeMap::new();
        for consumer in &self.consumers {
            let mut dropped = consumer.send_dropped.load(Ordering::Relaxed);
            if consumer.name == "rvc" {
                dropped += self.ai_recv_dropped.load(Ordering::Relaxed) + queue.rvc_dropped;
                consumer_drops.insert(
                    "rvc.skipped".to_string(),
                    self.ai_skipped.load(Ordering::Relaxed),
                );
            }
            consumer_drops.insert(consumer.name.clone(), dropped);
        }
        consumer_drops.insert(
            "tts".to_string(),
            self.tts_recv_dropped.load(Ordering::Relaxed) + queue.tts_dropped,
        );
        PreStatus {
            playout_policy: self.playout_policy(),
            state: "running".to_string(),
            denoise_backend: if self.enhancement.is_some() {
                "ulunas"
            } else {
                "wola"
            }
            .to_string(),
            enhancement: self
                .enhancement
                .as_ref()
                .map(|enhance| enhance.status(self.dsp_target().denoise)),
            aec: Some(self.aec.status(self.aec_enabled())),
            // 路由与静音上报命令目标；淡变在 20 ms 内完成，属执行细节。
            route: self.route_target(),
            mute: self.mute_target(),
            capture_device: self.capture_switch.current(),
            playback_device: self.playback_switch.current(),
            frames_captured: self.frames_captured.load(Ordering::Relaxed),
            frames_played: self.frames_played.load(Ordering::Relaxed),
            frames_dropped: queue.dropped,
            queue_depth: queue.depth,
            consumer_drops,
            speaker_state: speaker_name(self.speaker_state()).to_string(),
            tts_frames_played: self.tts_frames_played.load(Ordering::Relaxed),
            rvc_playout_gap_frames: self.rvc_playout_gap_frames.load(Ordering::Relaxed),
            playback_underruns: self.playback_underruns.load(Ordering::Relaxed),
            file_mode: self.file_mode(),
            mode_asserted: self.mode_asserted(),
            startup_id: self.startup_id.clone(),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            version: VERSION.to_string(),
        }
    }
}

/// 测试便利构造：默认消费者列表（rvc+asr）与默认 DSP 开关。
/// 默认列表的唯一定义源是 Config::pre_consumers，此处不重复定义。
#[cfg(test)]
pub(crate) fn test_shared(capture: &str, playback: &str, startup: &str) -> Shared {
    use mozart_core::config::Config;
    Shared::new(
        capture.to_string(),
        playback.to_string(),
        startup.to_string(),
        Config::default().pre_consumers(),
        DspSwitches::from_config(&PreDspConfig::default()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_code_roundtrip_is_closed() {
        for route in [Route::Direct, Route::Rvc] {
            assert_eq!(route_from_code(route_code(route)), route);
        }
    }

    #[test]
    fn speaker_code_roundtrip_is_closed() {
        for speaker in [Speaker::Realtime, Speaker::Broadcast] {
            assert_eq!(speaker_from_code(speaker_code(speaker)), speaker);
        }
    }

    #[test]
    fn status_reports_speaker_state_and_tts_frames() {
        let shared = test_shared("cap", "play", "startup");
        // 默认实时、零播报帧。
        let status = shared.status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
        });
        assert_eq!(status.speaker_state, "realtime");
        assert_eq!(status.tts_frames_played, 0);

        shared.set_speaker_state(Speaker::Broadcast);
        shared.add_tts_frames_played(5);
        let status = shared.status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
        });
        assert_eq!(status.speaker_state, "broadcast");
        assert_eq!(status.tts_frames_played, 5);
        assert_eq!(shared.speaker_state(), Speaker::Broadcast);
    }

    #[test]
    fn status_reports_targets_and_counters() {
        let shared = test_shared("cap", "play", "startup");
        shared.set_route_target(Route::Rvc);
        shared.set_mute_target(true);
        shared.add_captured(3);
        shared.add_played(2);
        shared.set_last_error("boom");
        let status = shared.status(QueueReadout {
            depth: 5,
            dropped: 7,
            rvc_dropped: 11,
            tts_dropped: 0,
        });
        assert_eq!(status.route, Route::Rvc);
        assert!(status.mute);
        assert_eq!(status.state, "running");
        assert_eq!(status.capture_device, "cap");
        assert_eq!(status.playback_device, "play");
        assert_eq!(status.frames_captured, 3);
        assert_eq!(status.frames_played, 2);
        assert_eq!(status.frames_dropped, 7);
        assert_eq!(status.queue_depth, 5);
        assert_eq!(status.last_error.as_deref(), Some("boom"));
        assert_eq!(status.version, VERSION);
    }

    /// 启动门控不变量（SPEC-04 单一权威）：未断言时 rvc AI 支路静默；
    /// daemon 断言实时态后激活；文件接管断言后释放；startup_id 随快照上报。
    #[test]
    fn rvc_ai_active_gated_by_assertion_and_file_mode() {
        let shared = test_shared("cap", "play", "boot-1");
        let readout = QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
        };
        // 启动：未断言，AI 支路静默（即便 file_mode=false）。
        assert!(!shared.mode_asserted());
        assert!(!shared.rvc_ai_active(), "未断言不得激活 rvc AI 支路");
        let status = shared.status(readout);
        assert!(!status.mode_asserted);
        assert_eq!(status.startup_id, "boot-1");

        // daemon 断言实时态（active:false）：激活。
        shared.set_mode_asserted(true);
        shared.set_file_mode(false);
        assert!(shared.rvc_ai_active(), "断言实时态后必须激活");

        // 文件接管断言（active:true）：释放，但 mode_asserted 仍为 true。
        shared.set_file_mode(true);
        assert!(!shared.rvc_ai_active(), "文件接管期间必须释放");
        assert!(shared.mode_asserted());

        // 交还：恢复激活。
        shared.set_file_mode(false);
        assert!(shared.rvc_ai_active());
    }

    #[test]
    fn consumer_drops_aggregate_rvc_path_and_skipped_replies() {
        // 口径（SPEC-02 / SPEC-05）："rvc" = 发送丢弃 + 坏包丢弃 + 接收环满（异常）；
        // "rvc.skipped" = direct 路由丢弃的输出帧（设计内，旧 skipped_replies）；
        // "asr" = 识别支路输入发送丢弃；"tts" = 播报坏包丢弃 + 播报接收环满。
        let shared = test_shared("cap", "play", "startup");
        shared.consumers()[0].set_send_dropped(2);
        shared.set_ai_recv_dropped(3);
        shared.add_ai_skipped(4);
        shared.consumers()[1].set_send_dropped(6);
        shared.set_tts_recv_dropped(7);
        let status = shared.status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 5,
            tts_dropped: 8,
        });
        assert_eq!(status.consumer_drops["rvc"], 10, "2+3+5 三源合计");
        assert_eq!(status.consumer_drops["rvc.skipped"], 4);
        assert_eq!(status.consumer_drops["asr"], 6);
        assert_eq!(status.consumer_drops["tts"], 15, "7+8 坏包与环满合计");

        // 默认零值：默认列表的四个键恒存在，daemon 可稳定解析。
        let status = test_shared("c", "p", "startup").status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
        });
        assert_eq!(
            status.consumer_drops,
            BTreeMap::from([
                ("rvc".to_string(), 0),
                ("rvc.skipped".to_string(), 0),
                ("asr".to_string(), 0),
                ("tts".to_string(), 0),
            ])
        );
    }

    #[test]
    fn consumer_drops_follow_configured_list() {
        // 键随配置列表动态生成：自定义消费者出现在快照，未配置的不出现；
        // "rvc" 口径（聚合输出链路 + rvc.skipped）只跟随名为 rvc 的消费者。
        let consumers = vec![
            PreConsumerConfig {
                name: "rvc".to_string(),
                socket: "rvc-in.sock".to_string(),
                file_mode_affected: true,
            },
            PreConsumerConfig {
                name: "extra".to_string(),
                socket: "extra-in.sock".to_string(),
                file_mode_affected: false,
            },
        ];
        let shared = Shared::new(
            "cap".to_string(),
            "play".to_string(),
            "startup".to_string(),
            consumers,
            DspSwitches::from_config(&PreDspConfig::default()),
        );
        shared.consumers()[1].set_send_dropped(9);
        shared.set_ai_recv_dropped(1);
        let status = shared.status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 2,
            tts_dropped: 0,
        });
        assert_eq!(
            status.consumer_drops,
            BTreeMap::from([
                ("rvc".to_string(), 3),
                ("rvc.skipped".to_string(), 0),
                ("extra".to_string(), 9),
                ("tts".to_string(), 0),
            ]),
            "自定义列表：无 asr 键，extra 独立计数"
        );

        // 空列表：只剩播报链路固定键。
        let empty = Shared::new(
            "cap".to_string(),
            "play".to_string(),
            "startup".to_string(),
            Vec::new(),
            DspSwitches::from_config(&PreDspConfig::default()),
        );
        let status = empty.status(QueueReadout {
            depth: 0,
            dropped: 0,
            rvc_dropped: 0,
            tts_dropped: 0,
        });
        assert_eq!(
            status.consumer_drops,
            BTreeMap::from([("tts".to_string(), 0)])
        );
    }

    #[test]
    fn dsp_switches_code_roundtrip_and_target() {
        // 打包编码封闭：16 种组合往返一致。
        for code in 0u8..16 {
            let switches = DspSwitches::from_code(code);
            assert_eq!(switches.code(), code);
        }
        // SetDsp 目标：控制线程写、处理线程读。
        let shared = test_shared("cap", "play", "startup");
        assert_eq!(
            shared.dsp_target(),
            DspSwitches::from_config(&PreDspConfig::default()),
            "启动目标来自 [pre.dsp] 基线"
        );
        let target = DspSwitches {
            denoise: false,
            gate: true,
            agc: true,
            limiter: false,
        };
        shared.set_dsp_target(target);
        assert_eq!(shared.dsp_target(), target);
    }

    #[test]
    fn device_switch_slot_succeeds_rejects_failure_and_respects_lock() {
        let stop = AtomicBool::new(false);
        let slot = std::sync::Arc::new(DeviceSwitchSlot::new("dev-a".to_string()));

        // 同设备切换幂等返回，不触发 pending。
        assert_eq!(
            slot.request_switch("dev-a", Duration::from_millis(100), &stop)
                .unwrap(),
            "dev-a"
        );
        assert!(!slot.has_pending());

        // 打开失败：保留旧设备，返回 InvalidArgument。
        let worker_slot = slot.clone();
        let handle = std::thread::spawn(move || {
            while !worker_slot.has_pending() {
                std::thread::sleep(Duration::from_millis(1));
            }
            worker_slot.apply_pending::<()>(
                |target| Err(format!("open {target} failed")),
                |_, _| unreachable!(),
            );
        });
        let err = slot
            .request_switch("dev-bad", Duration::from_secs(1), &stop)
            .unwrap_err();
        assert_eq!(err.0, ctl::CODE_INVALID_ARGUMENT);
        assert!(err.1.contains("open dev-bad failed"));
        assert_eq!(slot.current(), "dev-a");
        handle.join().unwrap();

        // 打开成功：更新 current 并返回旧设备。
        let worker_slot = slot.clone();
        let handle = std::thread::spawn(move || {
            while !worker_slot.has_pending() {
                std::thread::sleep(Duration::from_millis(1));
            }
            worker_slot.apply_pending(
                |target| Ok(format!("handle:{target}")),
                |target, stream| {
                    assert_eq!(target, "dev-b");
                    assert_eq!(stream, "handle:dev-b");
                },
            );
        });
        let prev = slot
            .request_switch("dev-b", Duration::from_secs(1), &stop)
            .unwrap();
        assert_eq!(prev, "dev-a");
        assert_eq!(slot.current(), "dev-b");
        handle.join().unwrap();

        // 锁定时（capture_file 模式）直接拒绝，even if same target。
        slot.lock("capture_file active");
        let locked = slot
            .request_switch("dev-b", Duration::from_millis(50), &stop)
            .unwrap_err();
        assert_eq!(locked.0, ctl::CODE_INVALID_ARGUMENT);
        assert!(locked.1.contains("capture_file"));

        // 待机态下对同设备名发起切换不得幂等跳过，必须触发 apply_pending 试开恢复。
        let standby_slot = std::sync::Arc::new(DeviceSwitchSlot::new("dev-a".to_string()));
        standby_slot.mark_standby();
        assert!(!standby_slot.is_active());
        let worker_slot = standby_slot.clone();
        let handle = std::thread::spawn(move || {
            while !worker_slot.has_pending() {
                std::thread::sleep(Duration::from_millis(1));
            }
            worker_slot.apply_pending(
                |target| Ok(format!("reopened:{target}")),
                |target, stream| {
                    assert_eq!(target, "dev-a");
                    assert_eq!(stream, "reopened:dev-a");
                },
            );
        });
        let recovered = standby_slot
            .request_switch("dev-a", Duration::from_secs(1), &stop)
            .unwrap();
        assert_eq!(recovered, "dev-a");
        assert!(standby_slot.is_active());
        handle.join().unwrap();
    }
}
