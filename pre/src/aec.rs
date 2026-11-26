//! 在线声学回声消除（AEC）参考环与后端封装（SPEC-02 步骤 2–3）。
//!
//! - 播放线程经定长无锁 SPSC 环（`rtrb`）向处理线程投递最终送往 ALSA 的
//!   单声道参考帧（`fade_in` 与 `mute` 之后），零分配、不阻塞；
//! - 处理线程在 80 Hz 高通之后、降噪（WOLA／UL-UNAS）与整形级之前执行
//!   `ProcessReverseStream` → `ProcessStream` 因果消除；
//! - 启用/禁用或采集、播放发生断流切换（`epoch` 跳变）时自动重置滤波器。

use mozart_core::ctl::schema::PreAecStatus;
use mozart_core::RAW_SAMPLES;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const REF_RING_CAPACITY: usize = 16;
const REF_MAX_BACKLOG: usize = 4;
const REF_TARGET_BACKLOG: usize = 2;
const DEFAULT_DELAY_MS: u16 = 45;
const TELEMETRY_SYNC_INTERVAL: u32 = 25;
const CLIP_THRESHOLD: f32 = 0.99;

/// 播放侧送入参考环的单帧。
#[derive(Clone, Copy)]
pub struct RefFrame {
    pub pcm: [f32; RAW_SAMPLES],
    pub epoch: u64,
    pub delay_ms: u16,
}

/// 由播放线程独占的参考帧发送端：热路径零分配、非阻塞。
pub struct RefSender {
    producer: Producer<RefFrame>,
    epoch: u64,
    ref_dropped: Arc<AtomicU64>,
}

impl RefSender {
    /// 播放侧设备切换或 ALSA xrun 恢复时推进代次，触发 AEC 重置。
    pub fn discontinuity(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// 非阻塞投递一帧最终混音后的单声道参考帧；环满丢弃并计数。
    pub fn push(&mut self, mono: &[f32; RAW_SAMPLES], delay_ms: u16) {
        if self
            .producer
            .push(RefFrame {
                pcm: *mono,
                epoch: self.epoch,
                delay_ms,
            })
            .is_err()
        {
            self.ref_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 由处理线程独占的参考帧接收端。
pub struct RefReceiver {
    consumer: Consumer<RefFrame>,
    ref_dropped: Arc<AtomicU64>,
}

/// AEC 可观测遥测：热路径只更新原子，仅在错误状态变化时短暂加锁记录原因。
pub struct AecTelemetry {
    backend_name: &'static str,
    active: AtomicBool,
    delay_ms: AtomicI32,
    erle_bits: AtomicU64,
    ref_dropped: Arc<AtomicU64>,
    ref_underruns: AtomicU64,
    resets: AtomicU64,
    capture_clips: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl AecTelemetry {
    pub fn new(backend_name: &'static str) -> Self {
        Self {
            backend_name,
            active: AtomicBool::new(false),
            delay_ms: AtomicI32::new(-1),
            erle_bits: AtomicU64::new(f64::NAN.to_bits()),
            ref_dropped: Arc::new(AtomicU64::new(0)),
            ref_underruns: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            capture_clips: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    pub fn status(&self, enabled: bool) -> PreAecStatus {
        let raw_delay = self.delay_ms.load(Ordering::Relaxed);
        let delay_ms = (raw_delay >= 0).then_some(raw_delay);
        let erle = f64::from_bits(self.erle_bits.load(Ordering::Relaxed));
        let erle_db = erle.is_finite().then_some(erle);
        let last_error = self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        PreAecStatus {
            enabled,
            backend: self.backend_name.to_string(),
            active: enabled && self.active.load(Ordering::Relaxed),
            delay_ms,
            erle_db,
            ref_dropped: self.ref_dropped.load(Ordering::Relaxed),
            ref_underruns: self.ref_underruns.load(Ordering::Relaxed),
            resets: self.resets.load(Ordering::Relaxed),
            capture_clips: self.capture_clips.load(Ordering::Relaxed),
            last_error,
        }
    }

    fn set_inactive(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.delay_ms.store(-1, Ordering::Relaxed);
        self.erle_bits.store(f64::NAN.to_bits(), Ordering::Relaxed);
    }

    fn set_stats(&self, stats: BackendStats) {
        self.delay_ms
            .store(stats.delay_ms.unwrap_or(-1), Ordering::Relaxed);
        let bits = stats
            .erle_db
            .filter(|v| v.is_finite())
            .unwrap_or(f64::NAN)
            .to_bits();
        self.erle_bits.store(bits, Ordering::Relaxed);
    }

    fn set_last_error(&self, error: Option<&str>) {
        let mut guard = self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = error.map(ToOwned::to_owned);
    }
}

/// 创建播放 → 处理线程的 SPSC 参考通道，并与给定遥测共享丢弃计数。
pub fn channel(telemetry: &Arc<AecTelemetry>) -> (RefSender, RefReceiver) {
    let (producer, consumer) = RingBuffer::new(REF_RING_CAPACITY);
    let ref_dropped = telemetry.ref_dropped.clone();
    (
        RefSender {
            producer,
            epoch: 0,
            ref_dropped: ref_dropped.clone(),
        },
        RefReceiver {
            consumer,
            ref_dropped,
        },
    )
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BackendStats {
    pub delay_ms: Option<i32>,
    pub erle_db: Option<f64>,
}

pub trait AecBackend {
    fn backend_name(&self) -> &'static str;
    fn is_available(&self) -> bool;
    fn reset(&mut self) -> Result<(), String>;
    fn process_reverse(&mut self, pcm: &[f32; RAW_SAMPLES]) -> Result<(), String>;
    fn process_capture(&mut self, pcm: &mut [f32; RAW_SAMPLES], delay_ms: i32) -> Result<(), String>;
    fn stats(&self) -> BackendStats;
}

#[cfg(has_aec3)]
mod ffi {
    use super::{AecBackend, BackendStats};
    use mozart_core::{RAW_SAMPLES, RAW_SAMPLE_RATE};
    use std::ptr::NonNull;

    #[repr(C)]
    struct MozartAec3 {
        _private: [u8; 0],
    }

    #[repr(C)]
    #[derive(Default)]
    struct MozartAec3Stats {
        has_delay: libc::c_int,
        delay_ms: libc::c_int,
        has_erle: libc::c_int,
        erle_db: f64,
        has_erl: libc::c_int,
        erl_db: f64,
    }

    extern "C" {
        fn mozart_aec3_create(sample_rate_hz: libc::c_int) -> *mut MozartAec3;
        fn mozart_aec3_destroy(handle: *mut MozartAec3);
        fn mozart_aec3_reset(handle: *mut MozartAec3) -> libc::c_int;
        fn mozart_aec3_process_reverse(
            handle: *mut MozartAec3,
            pcm: *const f32,
            samples: usize,
        ) -> libc::c_int;
        fn mozart_aec3_process_capture(
            handle: *mut MozartAec3,
            pcm: *mut f32,
            samples: usize,
            stream_delay_ms: libc::c_int,
        ) -> libc::c_int;
        fn mozart_aec3_get_stats(
            handle: *const MozartAec3,
            out: *mut MozartAec3Stats,
        ) -> libc::c_int;
    }

    pub struct WebRtcAec3Backend {
        handle: Option<NonNull<MozartAec3>>,
    }

    // SAFETY: 句柄只由创建后的处理线程独占访问。
    unsafe impl Send for WebRtcAec3Backend {}

    impl WebRtcAec3Backend {
        pub fn new() -> Self {
            // SAFETY: 传入受支持的 48 kHz；返回空指针时由 is_available 报告未就绪。
            let ptr = unsafe { mozart_aec3_create(RAW_SAMPLE_RATE as libc::c_int) };
            Self {
                handle: NonNull::new(ptr),
            }
        }
    }

    impl Drop for WebRtcAec3Backend {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                // SAFETY: handle 由 mozart_aec3_create 分配且仅销毁一次。
                unsafe { mozart_aec3_destroy(handle.as_ptr()) };
            }
        }
    }

    impl AecBackend for WebRtcAec3Backend {
        fn backend_name(&self) -> &'static str {
            "webrtc-aec3"
        }

        fn is_available(&self) -> bool {
            self.handle.is_some()
        }

        fn reset(&mut self) -> Result<(), String> {
            let Some(handle) = self.handle else {
                return Err("mozart_aec3 handle unavailable".to_string());
            };
            // SAFETY: handle 有效。
            let rc = unsafe { mozart_aec3_reset(handle.as_ptr()) };
            if rc == 0 {
                Ok(())
            } else {
                Err("mozart_aec3_reset failed".to_string())
            }
        }

        fn process_reverse(&mut self, pcm: &[f32; RAW_SAMPLES]) -> Result<(), String> {
            let Some(handle) = self.handle else {
                return Err("mozart_aec3 handle unavailable".to_string());
            };
            // SAFETY: pcm 长度固定为 960（48 kHz 下 2 个 10 ms 子帧）。
            let rc = unsafe {
                mozart_aec3_process_reverse(handle.as_ptr(), pcm.as_ptr(), RAW_SAMPLES)
            };
            if rc == 0 {
                Ok(())
            } else {
                Err("mozart_aec3_process_reverse failed".to_string())
            }
        }

        fn process_capture(
            &mut self,
            pcm: &mut [f32; RAW_SAMPLES],
            delay_ms: i32,
        ) -> Result<(), String> {
            let Some(handle) = self.handle else {
                return Err("mozart_aec3 handle unavailable".to_string());
            };
            // SAFETY: pcm 为 960 样本独占可变缓冲。
            let rc = unsafe {
                mozart_aec3_process_capture(
                    handle.as_ptr(),
                    pcm.as_mut_ptr(),
                    RAW_SAMPLES,
                    delay_ms as libc::c_int,
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                Err("mozart_aec3_process_capture failed".to_string())
            }
        }

        fn stats(&self) -> BackendStats {
            let Some(handle) = self.handle else {
                return BackendStats::default();
            };
            let mut raw = MozartAec3Stats::default();
            // SAFETY: handle 与 out 均有效。
            if unsafe { mozart_aec3_get_stats(handle.as_ptr(), &mut raw) } != 0 {
                return BackendStats::default();
            }
            BackendStats {
                delay_ms: (raw.has_delay != 0).then_some(raw.delay_ms as i32),
                erle_db: (raw.has_erle != 0).then_some(raw.erle_db),
            }
        }
    }
}

#[cfg(not(has_aec3))]
struct UnavailableBackend;

#[cfg(not(has_aec3))]
impl AecBackend for UnavailableBackend {
    fn backend_name(&self) -> &'static str {
        "unavailable"
    }

    fn is_available(&self) -> bool {
        false
    }

    fn reset(&mut self) -> Result<(), String> {
        Err("AEC3 library not linked (build without MOZART_AEC3_LIB_DIR)".to_string())
    }

    fn process_reverse(&mut self, _pcm: &[f32; RAW_SAMPLES]) -> Result<(), String> {
        Err("AEC3 library not linked".to_string())
    }

    fn process_capture(
        &mut self,
        _pcm: &mut [f32; RAW_SAMPLES],
        _delay_ms: i32,
    ) -> Result<(), String> {
        Err("AEC3 library not linked".to_string())
    }

    fn stats(&self) -> BackendStats {
        BackendStats::default()
    }
}

/// 创建当前编译配置对应的 AEC 后端（无库环境返回不可用桩）。
pub fn default_backend() -> Box<dyn AecBackend + Send> {
    #[cfg(has_aec3)]
    {
        Box::new(ffi::WebRtcAec3Backend::new())
    }
    #[cfg(not(has_aec3))]
    {
        Box::new(UnavailableBackend)
    }
}

pub fn default_backend_name() -> &'static str {
    #[cfg(has_aec3)]
    {
        "webrtc-aec3"
    }
    #[cfg(not(has_aec3))]
    {
        "unavailable"
    }
}

/// 处理线程持有的 AEC 状态机：管理参考环消费、因果重置与遥测同步。
pub struct AecProcessor {
    backend: Box<dyn AecBackend + Send>,
    ref_rx: RefReceiver,
    telemetry: Arc<AecTelemetry>,
    was_enabled: bool,
    last_capture_epoch: u64,
    last_playback_epoch: Option<u64>,
    last_delay_ms: u16,
    frames_since_stats: u32,
    reported_error: bool,
}

impl AecProcessor {
    pub fn new(
        backend: Box<dyn AecBackend + Send>,
        ref_rx: RefReceiver,
        telemetry: Arc<AecTelemetry>,
    ) -> Self {
        Self {
            backend,
            ref_rx,
            telemetry,
            was_enabled: false,
            last_capture_epoch: 0,
            last_playback_epoch: None,
            last_delay_ms: DEFAULT_DELAY_MS,
            frames_since_stats: 0,
            reported_error: false,
        }
    }

    pub fn process_capture(
        &mut self,
        pcm: &mut [f32; RAW_SAMPLES],
        capture_epoch: u64,
        enabled: bool,
        mut on_error: impl FnMut(&str),
    ) {
        if pcm.iter().any(|&s| s.abs() >= CLIP_THRESHOLD) {
            self.telemetry.capture_clips.fetch_add(1, Ordering::Relaxed);
        }

        if !enabled {
            while self.ref_rx.consumer.pop().is_ok() {}
            if self.was_enabled || self.reported_error {
                self.was_enabled = false;
                self.reported_error = false;
                self.frames_since_stats = 0;
                self.telemetry.set_inactive();
                self.telemetry.set_last_error(None);
            }
            return;
        }

        if !self.backend.is_available() {
            while self.ref_rx.consumer.pop().is_ok() {}
            self.was_enabled = true;
            self.telemetry.set_inactive();
            if !self.reported_error {
                let message = format!(
                    "AEC enabled but backend {} is unavailable",
                    self.backend.backend_name()
                );
                self.telemetry.set_last_error(Some(&message));
                on_error(&message);
                self.reported_error = true;
            }
            return;
        }

        let mut playback_epoch_changed = false;
        let backlog = self.ref_rx.consumer.slots();
        if backlog > REF_MAX_BACKLOG {
            let to_drop = backlog - REF_TARGET_BACKLOG;
            let mut dropped_now = 0u64;
            for _ in 0..to_drop {
                let Ok(discarded) = self.ref_rx.consumer.pop() else {
                    break;
                };
                dropped_now += 1;
                if self
                    .last_playback_epoch
                    .is_some_and(|prev| prev != discarded.epoch)
                {
                    playback_epoch_changed = true;
                }
                self.last_playback_epoch = Some(discarded.epoch);
            }
            if dropped_now > 0 {
                self.ref_rx
                    .ref_dropped
                    .fetch_add(dropped_now, Ordering::Relaxed);
            }
        }

        let ref_frame = self.ref_rx.consumer.pop().ok();
        if let Some(rf) = ref_frame {
            if self.last_playback_epoch.is_some_and(|prev| prev != rf.epoch) {
                playback_epoch_changed = true;
            }
            self.last_playback_epoch = Some(rf.epoch);
            self.last_delay_ms = rf.delay_ms;
        } else {
            self.telemetry.ref_underruns.fetch_add(1, Ordering::Relaxed);
        }

        let turning_on = !self.was_enabled;
        let capture_epoch_changed = capture_epoch != self.last_capture_epoch;
        self.was_enabled = true;
        self.last_capture_epoch = capture_epoch;

        if turning_on || capture_epoch_changed || playback_epoch_changed {
            if let Err(err) = self.backend.reset() {
                self.record_failure(&err, &mut on_error);
                return;
            }
            self.telemetry.resets.fetch_add(1, Ordering::Relaxed);
        }

        let silence = [0.0f32; RAW_SAMPLES];
        let (ref_pcm, delay_ms) = match &ref_frame {
            Some(rf) => (&rf.pcm, rf.delay_ms as i32),
            None => (&silence, self.last_delay_ms as i32),
        };
        if let Err(err) = self.backend.process_reverse(ref_pcm) {
            self.record_failure(&err, &mut on_error);
            return;
        }
        if let Err(err) = self.backend.process_capture(pcm, delay_ms) {
            self.record_failure(&err, &mut on_error);
            return;
        }

        if self.reported_error {
            self.reported_error = false;
            self.telemetry.set_last_error(None);
        }
        self.telemetry.active.store(true, Ordering::Relaxed);
        self.frames_since_stats += 1;
        if turning_on || self.frames_since_stats >= TELEMETRY_SYNC_INTERVAL {
            self.frames_since_stats = 0;
            self.telemetry.set_stats(self.backend.stats());
        }
    }

    fn record_failure(&mut self, err: &str, on_error: &mut impl FnMut(&str)) {
        self.telemetry.set_inactive();
        if !self.reported_error {
            self.telemetry.set_last_error(Some(err));
            on_error(err);
            self.reported_error = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct FakeBackend {
        available: bool,
        resets: Arc<AtomicUsize>,
        reverse_calls: Arc<AtomicUsize>,
        capture_calls: Arc<AtomicUsize>,
        last_delay: Arc<AtomicI32>,
    }

    impl FakeBackend {
        fn new(available: bool) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicI32>) {
            let resets = Arc::new(AtomicUsize::new(0));
            let rev = Arc::new(AtomicUsize::new(0));
            let cap = Arc::new(AtomicUsize::new(0));
            let delay = Arc::new(AtomicI32::new(-1));
            (
                Self {
                    available,
                    resets: resets.clone(),
                    reverse_calls: rev.clone(),
                    capture_calls: cap.clone(),
                    last_delay: delay.clone(),
                },
                resets,
                rev,
                cap,
                delay,
            )
        }
    }

    impl AecBackend for FakeBackend {
        fn backend_name(&self) -> &'static str {
            if self.available { "fake-aec3" } else { "unavailable" }
        }
        fn is_available(&self) -> bool {
            self.available
        }
        fn reset(&mut self) -> Result<(), String> {
            self.resets.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn process_reverse(&mut self, _pcm: &[f32; RAW_SAMPLES]) -> Result<(), String> {
            self.reverse_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn process_capture(
            &mut self,
            pcm: &mut [f32; RAW_SAMPLES],
            delay_ms: i32,
        ) -> Result<(), String> {
            self.capture_calls.fetch_add(1, Ordering::Relaxed);
            self.last_delay.store(delay_ms, Ordering::Relaxed);
            for s in pcm.iter_mut() {
                *s *= 0.5;
            }
            Ok(())
        }
        fn stats(&self) -> BackendStats {
            BackendStats {
                delay_ms: Some(48),
                erle_db: Some(22.5),
            }
        }
    }

    #[test]
    fn ref_ring_drops_oldest_overrun_and_trims_excess_backlog() {
        let telemetry = Arc::new(AecTelemetry::new("fake-aec3"));
        let (mut tx, rx) = channel(&telemetry);
        let (backend, _, rev_calls, _, _) = FakeBackend::new(true);
        let mut proc = AecProcessor::new(Box::new(backend), rx, telemetry.clone());

        // 写满 16 帧后再多推 2 帧：发送侧应计 2 次 ref_dropped。
        let ref_pcm = [0.1f32; RAW_SAMPLES];
        for _ in 0..(REF_RING_CAPACITY + 2) {
            tx.push(&ref_pcm, 45);
        }
        assert_eq!(telemetry.status(true).ref_dropped, 2);

        // 积压 16 帧（> 4）：单帧处理应裁剪到剩 2 帧再弹出 1 帧，共丢弃 14 帧。
        let mut cap = [0.2f32; RAW_SAMPLES];
        proc.process_capture(&mut cap, 0, true, |_| {});
        let st = telemetry.status(true);
        assert_eq!(st.ref_dropped, 2 + (REF_RING_CAPACITY as u64 - REF_TARGET_BACKLOG as u64));
        assert_eq!(rev_calls.load(Ordering::Relaxed), 1);
        assert_eq!(st.resets, 1);
        assert!(st.active);
        assert_eq!(st.delay_ms, Some(48));
        assert_eq!(st.erle_db, Some(22.5));
    }

    #[test]
    fn disabled_mode_passes_pcm_unchanged_and_drains_ring() {
        let telemetry = Arc::new(AecTelemetry::new("fake-aec3"));
        let (mut tx, rx) = channel(&telemetry);
        let (backend, _, rev_calls, cap_calls, _) = FakeBackend::new(true);
        let mut proc = AecProcessor::new(Box::new(backend), rx, telemetry.clone());

        tx.push(&[0.3; RAW_SAMPLES], 45);
        let orig = [0.42f32; RAW_SAMPLES];
        let mut cap = orig;
        proc.process_capture(&mut cap, 0, false, |_| {});
        assert_eq!(cap, orig, "禁用时采集帧必须逐位保留");
        assert_eq!(rev_calls.load(Ordering::Relaxed), 0);
        assert_eq!(cap_calls.load(Ordering::Relaxed), 0);
        assert!(!telemetry.status(false).active);

        // 切回启用且环已被排空：应触发一次 ref_underruns 并喂静音参考。
        proc.process_capture(&mut cap, 0, true, |_| {});
        assert_eq!(telemetry.status(true).ref_underruns, 1);
        assert_eq!(rev_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn resets_on_capture_and_playback_epoch_changes_and_counts_clips() {
        let telemetry = Arc::new(AecTelemetry::new("fake-aec3"));
        let (mut tx, rx) = channel(&telemetry);
        let (backend, resets, _, _, delay) = FakeBackend::new(true);
        let mut proc = AecProcessor::new(Box::new(backend), rx, telemetry.clone());

        tx.push(&[0.1; RAW_SAMPLES], 42);
        let mut cap = [0.2f32; RAW_SAMPLES];
        proc.process_capture(&mut cap, 0, true, |_| {});
        assert_eq!(resets.load(Ordering::Relaxed), 1);
        assert_eq!(delay.load(Ordering::Relaxed), 42);

        // 同一 epoch：不重置。
        tx.push(&[0.1; RAW_SAMPLES], 44);
        proc.process_capture(&mut cap, 0, true, |_| {});
        assert_eq!(resets.load(Ordering::Relaxed), 1);
        assert_eq!(delay.load(Ordering::Relaxed), 44);

        // capture_epoch 跳变：触发重置。
        tx.push(&[0.1; RAW_SAMPLES], 44);
        proc.process_capture(&mut cap, 1, true, |_| {});
        assert_eq!(resets.load(Ordering::Relaxed), 2);

        // playback_epoch 跳变 + 削波：触发重置且记录削波。
        tx.discontinuity();
        tx.push(&[0.1; RAW_SAMPLES], 46);
        cap[10] = -0.995;
        proc.process_capture(&mut cap, 1, true, |_| {});
        assert_eq!(resets.load(Ordering::Relaxed), 3);
        let st = telemetry.status(true);
        assert_eq!(st.resets, 3);
        assert_eq!(st.capture_clips, 1);
    }

    #[test]
    fn unavailable_backend_reports_explicit_error_without_modifying_pcm() {
        let telemetry = Arc::new(AecTelemetry::new("unavailable"));
        let (mut tx, rx) = channel(&telemetry);
        let (backend, _, _, _, _) = FakeBackend::new(false);
        let mut proc = AecProcessor::new(Box::new(backend), rx, telemetry.clone());

        tx.push(&[0.5; RAW_SAMPLES], 45);
        let orig = [0.25f32; RAW_SAMPLES];
        let mut cap = orig;
        let mut errors = Vec::new();
        proc.process_capture(&mut cap, 0, true, |err| errors.push(err.to_string()));
        proc.process_capture(&mut cap, 0, true, |err| errors.push(err.to_string()));
        assert_eq!(cap, orig);
        assert_eq!(errors.len(), 1, "同一失败状态只记录一次诊断原因");
        let st = telemetry.status(true);
        assert!(!st.active);
        assert!(st.last_error.is_some());
    }

    #[cfg(has_aec3)]
    #[test]
    fn real_aec3_cancels_65ms_echo_with_100ppm_drift() {
        use mozart_core::RAW_SAMPLE_RATE;

        let telemetry = Arc::new(AecTelemetry::new(default_backend_name()));
        let (mut tx, rx) = channel(&telemetry);
        let mut proc = AecProcessor::new(default_backend(), rx, telemetry.clone());
        assert!(default_backend().is_available());

        // 12 s @ 48 kHz = 600 帧；65 ms 延迟（3120 样本）+ 100 ppm 时钟漂移。
        let total_frames = 600usize;
        let total_samples = total_frames * RAW_SAMPLES;
        let mut far = vec![0.0f32; total_samples];
        let mut rng = 0x2468ACE1u32;
        let mut lp = 0.0f32;
        for (i, slot) in far.iter_mut().enumerate().skip(RAW_SAMPLE_RATE as usize) {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let white = (rng >> 9) as f32 / 8388608.0 - 1.0;
            lp = 0.85 * lp + 0.15 * white;
            let env = 0.35
                * (0.6
                    + 0.4
                        * (2.0 * std::f32::consts::PI * 2.5 * i as f32 / RAW_SAMPLE_RATE as f32)
                            .sin());
            *slot = env * lp;
        }

        let base_delay = 3120usize; // 65 ms
        let mut echo_in = vec![0.0f32; total_samples];
        for i in (base_delay + 16)..total_samples {
            let drift = (i as f64) * 100e-6;
            let pos = i as f64 - base_delay as f64 - drift;
            if pos >= 1.0 {
                let idx0 = pos as usize;
                let frac = (pos - idx0 as f64) as f32;
                let s0 = far[idx0] + (far[idx0 + 1] - far[idx0]) * frac;
                let s1 = far[idx0 - 1] + (far[idx0] - far[idx0 - 1]) * frac;
                echo_in[i] = 0.30 * s0 + 0.12 * s1;
            }
        }

        let mut out_all = Vec::with_capacity(total_samples);
        let mut errs = Vec::new();
        for f in 0..total_frames {
            let off = f * RAW_SAMPLES;
            let ref_chunk: [f32; RAW_SAMPLES] = far[off..off + RAW_SAMPLES].try_into().unwrap();
            let mut cap_chunk: [f32; RAW_SAMPLES] =
                echo_in[off..off + RAW_SAMPLES].try_into().unwrap();
            tx.push(&ref_chunk, 0);
            proc.process_capture(&mut cap_chunk, 0, true, |e| errs.push(e.to_string()));
            out_all.extend_from_slice(&cap_chunk);
        }
        assert!(errs.is_empty(), "真实 AEC3 不应报错：{errs:?}");

        let start = 4 * RAW_SAMPLE_RATE as usize;
        let p_in: f64 = echo_in[start..].iter().map(|&v| (v as f64) * (v as f64)).sum();
        let p_out: f64 = out_all[start..].iter().map(|&v| (v as f64) * (v as f64)).sum();
        let erle_db = 10.0 * ((p_in + 1e-24) / (p_out + 1e-24)).log10();
        assert!(
            erle_db > 15.0,
            "65 ms 延迟 + 100 ppm 漂移下 ERLE 必须 > 15 dB，实际 {erle_db:.2} dB"
        );
        let st = telemetry.status(true);
        assert!(st.active);
        assert!(st.delay_ms.is_some());
    }
}
