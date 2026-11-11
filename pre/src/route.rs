//! direct/rvc 路由状态机与静音淡变（SPEC-02：路由与静音由 pre 执行淡变，策略归 daemon）。
//! 增益曲线为线性，长度 10 ms = 480 样本（旧实现 main.c:417,643 的 fade_ms=10）。
//! 切换次序沿用旧实现（main.c:704-713,768-770）：
//! - rvc→direct：立即切源，从零淡入；
//! - direct→rvc：先淡出完成再切源，随后淡入（统一增益路径下新源必须淡入，否则从零硬起）。
//! - 淡出途中重新要求当前源：中止淡出并回淡入（重入保护，SPEC-02 改进点）。
//! 本模块是纯逻辑，不触碰设备与共享状态，逐分支单测覆盖。

use mozart_core::ctl::schema::Route;

/// 淡变时长（毫秒）。
const FADE_MS: u32 = 10;
/// 淡变样本数 @ 48 kHz；唯一定义点，路由与静音共用。
pub const FADE_SAMPLES: usize = (mozart_core::RAW_SAMPLE_RATE * FADE_MS / 1000) as usize;

/// 单样本增益步长。
fn fade_step() -> f32 {
    1.0 / FADE_SAMPLES as f32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 增益稳定在 1，route == target。
    Stable,
    /// 当前源淡出到 0；完成后切到 target 并转淡入。
    FadingOut,
    /// 当前源从 0 淡入到 1，route == target。
    FadingIn,
}

/// 路由状态机。`route` 是当前可听源，`target` 是命令目标；二者切换必经淡变。
pub struct Router {
    route: Route,
    target: Route,
    phase: Phase,
    gain: f32,
}

impl Router {
    /// 初始路由按满增益直通（旧 fade_init 的 start_active 语义）。
    pub fn new(route: Route) -> Self {
        Self {
            route,
            target: route,
            phase: Phase::Stable,
            gain: 1.0,
        }
    }

    /// 当前可听源，播放侧据此选帧。
    pub fn route(&self) -> Route {
        self.route
    }

    /// 设置命令目标；同目标幂等无操作。
    pub fn request(&mut self, target: Route) {
        if target == self.target {
            return;
        }
        match (self.route, target) {
            // direct→rvc：先淡出（从 Stable 或淡入途中都是当前增益起降）。
            (Route::Direct, Route::Rvc) => self.phase = Phase::FadingOut,
            // 淡出途中重设 direct：中止淡出，回淡入。
            (Route::Direct, Route::Direct) => self.phase = Phase::FadingIn,
            // rvc→direct：立即切源，从零淡入。
            (Route::Rvc, Route::Direct) => {
                self.route = Route::Direct;
                self.gain = 0.0;
                self.phase = Phase::FadingIn;
            }
            // route == Rvc 且 target == Rvc 只会在淡出中翻转后到达，此时 route 已是 Direct。
            (Route::Rvc, Route::Rvc) => unreachable!("same-target request returned early"),
        }
        self.target = target;
    }

    /// 对一帧样本应用当前增益并推进状态；先推进后相乘（旧 fade_apply 语义）。
    pub fn apply(&mut self, samples: &mut [f32]) {
        let step = fade_step();
        for sample in samples {
            match self.phase {
                Phase::Stable => {}
                Phase::FadingIn => {
                    self.gain = (self.gain + step).min(1.0);
                    if self.gain >= 1.0 {
                        self.phase = Phase::Stable;
                    }
                }
                Phase::FadingOut => {
                    self.gain = (self.gain - step).max(0.0);
                    if self.gain <= 0.0 {
                        self.route = self.target;
                        self.phase = Phase::FadingIn;
                    }
                }
            }
            *sample *= self.gain;
        }
    }
}

/// 静音淡变：淡到零并保持，解除时淡回满增益；与路由增益相乘生效。
pub struct Mute {
    target: bool,
    gain: f32,
}

impl Mute {
    pub fn new() -> Self {
        Self {
            target: false,
            gain: 1.0,
        }
    }

    pub fn request(&mut self, mute: bool) {
        self.target = mute;
    }

    /// 对一帧样本应用当前增益并推进状态；先推进后相乘。
    pub fn apply(&mut self, samples: &mut [f32]) {
        let step = fade_step();
        let end = if self.target { 0.0 } else { 1.0 };
        for sample in samples {
            if self.gain < end {
                self.gain = (self.gain + step).min(end);
            } else if self.gain > end {
                self.gain = (self.gain - step).max(end);
            }
            *sample *= self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐样本推进，返回每样本实际增益；淡变有 480 步 f32 累积，时序有 ±1 样本舍入。
    fn drive_router(router: &mut Router, samples: usize) -> Vec<f32> {
        let mut gains = Vec::with_capacity(samples);
        for _ in 0..samples {
            let mut probe = [1.0f32];
            router.apply(&mut probe);
            gains.push(probe[0]);
        }
        gains
    }

    fn drive_mute(mute: &mut Mute, samples: usize) -> Vec<f32> {
        let mut gains = Vec::with_capacity(samples);
        for _ in 0..samples {
            let mut probe = [1.0f32];
            mute.apply(&mut probe);
            gains.push(probe[0]);
        }
        gains
    }

    /// 相对误差容差：480 步 f32 累积误差的最坏量级约 3e-5。
    const EPS: f32 = 1e-4;

    fn assert_close(actual: f32, expect: f32) {
        assert!(
            (actual - expect).abs() <= EPS * expect.abs().max(1.0),
            "actual {actual}, expect {expect}"
        );
    }

    #[test]
    fn fade_length_is_10ms_at_48k() {
        assert_eq!(FADE_SAMPLES, 480);
    }

    #[test]
    fn switch_to_rvc_fades_out_before_switching() {
        let mut router = Router::new(Route::Direct);
        router.request(Route::Rvc);
        assert_eq!(router.route(), Route::Direct);

        // 前 480 样本：direct 从 1 线性淡出。
        let gains = drive_router(&mut router, FADE_SAMPLES + 8);
        for (i, &gain) in gains.iter().take(FADE_SAMPLES - 2).enumerate() {
            assert_close(gain, 1.0 - (i + 1) as f32 / FADE_SAMPLES as f32);
        }
        // 淡出段单调不增；末对跨越切源点，不计入。
        for pair in gains.windows(2).take(FADE_SAMPLES - 1) {
            assert!(pair[0] >= pair[1], "淡出必须单调不增：{pair:?}");
        }
        // 淡出末尾近零，随后切源并从零淡入。
        assert!(gains[FADE_SAMPLES - 1] <= EPS);
        assert_eq!(router.route(), Route::Rvc);
        assert!(gains[FADE_SAMPLES + 7] > 0.0);

        // 淡入在 480±1 样本内完成并稳态：增益钳位到精确 1.0。
        let gains = drive_router(&mut router, FADE_SAMPLES + 8);
        assert_eq!(gains[FADE_SAMPLES + 7], 1.0);
        assert_eq!(router.route(), Route::Rvc);

        // 稳态输出等于输入。
        let mut probe = [0.5f32; 8];
        router.apply(&mut probe);
        for sample in probe {
            assert_eq!(sample, 0.5);
        }
    }

    #[test]
    fn switch_to_direct_is_immediate_with_fade_in() {
        // 先走到 Stable(Rvc)。
        let mut router = Router::new(Route::Direct);
        router.request(Route::Rvc);
        drive_router(&mut router, FADE_SAMPLES * 2 + 8);
        assert_eq!(router.route(), Route::Rvc);

        // rvc→direct：请求即切源，随后从零淡入。
        router.request(Route::Direct);
        assert_eq!(router.route(), Route::Direct);
        let gains = drive_router(&mut router, FADE_SAMPLES + 8);
        assert_close(gains[0], 1.0 / FADE_SAMPLES as f32);
        assert_close(gains[FADE_SAMPLES - 1], 1.0);
        assert_eq!(gains[FADE_SAMPLES + 7], 1.0);
    }

    #[test]
    fn re_request_direct_during_fade_out_aborts() {
        let mut router = Router::new(Route::Direct);
        router.request(Route::Rvc);
        let gains = drive_router(&mut router, FADE_SAMPLES / 2);
        assert_eq!(router.route(), Route::Direct);
        assert!(gains[FADE_SAMPLES / 2 - 1] < 1.0, "应处于淡出途中");

        // 淡出途中要求回 direct：中止淡出，增益回升，不切源。
        router.request(Route::Direct);
        assert_eq!(router.route(), Route::Direct);
        let gains = drive_router(&mut router, FADE_SAMPLES / 2 + 8);
        assert!(gains[0] < gains[1], "中止后应回淡入");
        assert_eq!(gains[FADE_SAMPLES / 2 + 7], 1.0);
        assert_eq!(router.route(), Route::Direct);
    }

    #[test]
    fn re_request_rvc_during_fade_in_fades_out_first() {
        // rvc→direct 立即切换后处于淡入途中（route=Direct，增益半程）。
        let mut router = Router::new(Route::Direct);
        router.request(Route::Rvc);
        drive_router(&mut router, FADE_SAMPLES * 2 + 8);
        router.request(Route::Direct);
        drive_router(&mut router, FADE_SAMPLES / 2);

        // 此时要求 rvc：direct 侧仍先淡出完成再切源。
        router.request(Route::Rvc);
        assert_eq!(router.route(), Route::Direct);
        let gains = drive_router(&mut router, FADE_SAMPLES + 8);
        // 增益半程起降，240 样本内到零；到零即切源并回淡入。
        assert!(gains.iter().any(|&gain| gain <= EPS), "必须经过零点再切源");
        assert_eq!(router.route(), Route::Rvc);
        assert!(gains[FADE_SAMPLES + 7] > 0.0, "切源后进入淡入");
    }

    #[test]
    fn same_target_request_is_noop() {
        let mut router = Router::new(Route::Direct);
        router.request(Route::Direct);
        let gains = drive_router(&mut router, 4);
        for gain in gains {
            assert_eq!(gain, 1.0);
        }

        // 重复 SetRoute(rvc) 不重启淡变曲线：与单次请求的机器逐样本一致。
        let mut once = Router::new(Route::Direct);
        once.request(Route::Rvc);
        let mut twice = Router::new(Route::Direct);
        twice.request(Route::Rvc);
        twice.request(Route::Rvc);
        let left = drive_router(&mut once, FADE_SAMPLES);
        let right = drive_router(&mut twice, FADE_SAMPLES);
        assert_eq!(left, right);
    }

    #[test]
    fn mute_fades_to_zero_holds_and_recovers() {
        let mut mute = Mute::new();
        mute.request(true);
        let gains = drive_mute(&mut mute, FADE_SAMPLES + 8);
        assert_close(gains[0], 1.0 - 1.0 / FADE_SAMPLES as f32);
        assert!(gains[FADE_SAMPLES - 1] <= EPS);

        // 保持：继续送帧仍全零。
        let gains = drive_mute(&mut mute, FADE_SAMPLES);
        for gain in gains {
            assert!(gain <= EPS, "静音保持期增益必须为零：{gain}");
        }

        // 解除：从零淡回满增益。
        mute.request(false);
        let gains = drive_mute(&mut mute, FADE_SAMPLES + 8);
        assert_close(gains[0], 1.0 / FADE_SAMPLES as f32);
        assert_eq!(gains[FADE_SAMPLES + 7], 1.0);
    }

    #[test]
    fn mute_gain_multiplies_route_gain() {
        // 路由淡出与静音淡出同时在途：总增益 = 两者逐样本增益之积。
        const N: usize = FADE_SAMPLES / 2;
        let mut route_probe = Router::new(Route::Direct);
        route_probe.request(Route::Rvc);
        let route_gains = drive_router(&mut route_probe, N);
        let mut mute_probe = Mute::new();
        mute_probe.request(true);
        let mute_gains = drive_mute(&mut mute_probe, N);

        let mut router = Router::new(Route::Direct);
        router.request(Route::Rvc);
        let mut mute = Mute::new();
        mute.request(true);
        let mut out = [1.0f32; N];
        router.apply(&mut out);
        mute.apply(&mut out);

        // 增益序列与探针完全相同（逐位），乘积可精确断言。
        for i in 0..N {
            assert_eq!(out[i], route_gains[i] * mute_gains[i]);
        }
        assert_eq!(router.route(), Route::Direct);
    }
}
