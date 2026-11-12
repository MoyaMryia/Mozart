//! 整形级：噪声门 / AGC / 峰值限幅（旧 stages.c 等价重写，SPEC-02）。
//! 约定：`enabled = false` 时 process 立即返回、不触碰样本——新增级默认
//! 关闭即 AI 路径逐位恒等（旧 stages.c L4 注释，测试固定该性质）。
//! 全部在 48 kHz 单声道 f32 块上就地处理；调用方（Dsp）以 960 定块驱动
//!（WOLA 首块发射 896），无空块路径。参数固定为旧实现出厂默认，无配置键
//!（同 Task 3 降噪器先例）。
//! 遥测字段（*_db、开合计数）不移植：新 PreStatus 无 DSP 字段，无消费方。

use mozart_core::RAW_SAMPLE_RATE;

/// 数字静音电平（dBFS）：均方为零块的返回值（旧 MOZART_SILENCE_DB）。
const SILENCE_DB: f32 = -120.0;

/// 块均方根电平（dBFS）；静音块返回 -120（旧 mozart_rms_db）。
/// 旧 mozart_peak_db（遥测用）不移植：PreStatus 无 DSP 字段，无消费方。
fn rms_db(x: &[f32]) -> f32 {
    let mut power = 0.0f32;
    for &sample in x {
        power += sample * sample;
    }
    let rms = (power / x.len() as f32).sqrt();
    if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        SILENCE_DB
    }
}

/// 噪声门（旧 mozart_gate）：开关门按块判定，保持时长折算为块数，
/// 增益按 attack/release 线性爬升。
pub struct Gate {
    /// false 时 process 为无操作（逐位恒等）。
    enabled: bool,
    /// 开门限（块 RMS 高于此值开门）。
    threshold_db: f32,
    attack_ms: f32,
    hold_ms: f32,
    release_ms: f32,
    open: bool,
    /// 剩余保持块数。
    hold_blocks: i32,
    /// 当前增益 0..1。
    gain: f32,
}

impl Gate {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            threshold_db: -45.0,
            attack_ms: 5.0,
            hold_ms: 60.0,
            release_ms: 150.0,
            open: false,
            hold_blocks: 0,
            // 旧实现增益从 0 起爬升（首次开门前输出静音）。
            gain: 0.0,
        }
    }

    /// 就地处理一块；禁用时无操作。
    pub fn process(&mut self, x: &mut [f32]) {
        if !self.enabled {
            return;
        }
        let rms = rms_db(x);
        // 保持时长折算为块数（向上取整，至少 1 块；20 ms 块下 60 ms → 4 块）。
        let hold_blocks =
            ((self.hold_ms / 1000.0) * RAW_SAMPLE_RATE as f32 / x.len() as f32) as i32 + 1;
        if rms >= self.threshold_db {
            self.open = true;
            self.hold_blocks = hold_blocks;
        } else if self.open {
            self.hold_blocks -= 1;
            if self.hold_blocks < 0 {
                self.open = false;
            }
        }

        let target = if self.open { 1.0 } else { 0.0 };
        let span_s = if self.open {
            self.attack_ms
        } else {
            self.release_ms
        } / 1000.0;
        let step = if span_s > 0.0 {
            1.0 / (span_s * RAW_SAMPLE_RATE as f32)
        } else {
            1.0
        };
        for sample in x.iter_mut() {
            if self.gain < target {
                self.gain += step;
                if self.gain > target {
                    self.gain = target;
                }
            } else if self.gain > target {
                self.gain -= step;
                if self.gain < target {
                    self.gain = target;
                }
            }
            *sample *= self.gain;
        }
    }
}

/// 自动增益（旧 mozart_agc）：每块一阶平滑逼近目标 RMS，只放大不衰减。
pub struct Agc {
    enabled: bool,
    /// 目标输出 RMS 电平（dBFS）。
    target_db: f32,
    /// 增益上限（dB）。
    max_gain_db: f32,
    /// 增益上升的每块平滑系数（慢，约 1 s 收敛）。
    up_coeff: f32,
    /// 增益下降的每块平滑系数（快，约 100 ms 收敛）。
    down_coeff: f32,
    /// 当前增益（dB）。
    gain: f32,
}

impl Agc {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            target_db: -20.0,
            max_gain_db: 24.0,
            up_coeff: 0.02,
            down_coeff: 0.20,
            gain: 0.0,
        }
    }

    /// 就地处理一块；禁用时无操作。
    pub fn process(&mut self, x: &mut [f32]) {
        if !self.enabled {
            return;
        }
        let rms = rms_db(x);
        let mut desired = self.target_db - rms;
        if desired > self.max_gain_db {
            desired = self.max_gain_db;
        }
        if desired < 0.0 {
            desired = 0.0;
        }
        let coeff = if desired > self.gain {
            self.up_coeff
        } else {
            self.down_coeff
        };
        self.gain += coeff * (desired - self.gain);
        let gain = 10.0f32.powf(self.gain / 20.0);
        for sample in x.iter_mut() {
            *sample *= gain;
        }
    }
}

/// 峰值限幅（旧 mozart_limiter）：包络立即上、指数下；增益恢复同系数。
pub struct Limiter {
    enabled: bool,
    /// 输出峰值上限（dBFS）。
    ceiling_db: f32,
    release_ms: f32,
    /// 峰值包络。
    env: f32,
    /// 当前增益 ≤ 1。
    gain: f32,
}

impl Limiter {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ceiling_db: -1.0,
            release_ms: 80.0,
            env: 0.0,
            gain: 1.0,
        }
    }

    /// 就地处理一块；禁用时无操作。
    pub fn process(&mut self, x: &mut [f32]) {
        if !self.enabled {
            return;
        }
        let ceiling = 10.0f32.powf(self.ceiling_db / 20.0);
        let rel_s = self.release_ms / 1000.0;
        // 包络指数衰减系数与增益恢复系数：e^{-1/(t·rate)}（旧实现单点公式）。
        let decay = (-1.0 / (rel_s * RAW_SAMPLE_RATE as f32)).exp();
        for sample in x.iter_mut() {
            let a = sample.abs();
            // 立即 attack：包络取当前峰值；否则按 release 指数回落。
            self.env = if a > self.env * decay {
                a
            } else {
                self.env * decay
            };
            if self.env < ceiling {
                self.gain += (1.0 - self.gain) * (1.0 - decay);
            } else {
                let need = ceiling / self.env;
                if need < self.gain {
                    self.gain = need;
                }
            }
            *sample *= self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = mozart_core::RAW_SAMPLES;

    /// 固定种子 xorshift32 噪声，与 golden 夹具同法（跨运行确定）。
    fn noise(seed: u32, n: usize) -> Vec<f32> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 9) as f32 / 8388608.0 - 1.0
            })
            .collect()
    }

    /// 20 ms 块的满幅正弦。
    fn sine(freq: f32, amplitude: f32) -> Vec<f32> {
        (0..BLOCK)
            .map(|i| {
                amplitude * (2.0 * std::f32::consts::PI * freq * i as f32 / 48000.0).sin()
            })
            .collect()
    }

    #[test]
    fn disabled_stages_are_bitwise_identity() {
        // 旧 stages.c 约定：enabled=false 时 process 立即返回，不触碰样本。
        // 禁用态是「新增级默认关闭 = AI 路径逐位恒等」的依据，逐位断言。
        for mut buffer in [noise(0x4D5A5254, BLOCK), sine(440.0, 0.3)] {
            let expect = buffer.clone();
            Gate::new(false).process(&mut buffer);
            assert_eq!(buffer, expect, "gate 禁用必须逐位恒等");
            Agc::new(false).process(&mut buffer);
            assert_eq!(buffer, expect, "agc 禁用必须逐位恒等");
            Limiter::new(false).process(&mut buffer);
            assert_eq!(buffer, expect, "limiter 禁用必须逐位恒等");
        }
        // 静音块也是恒等：不因零输入改写。
        let mut silence = vec![0.0f32; BLOCK];
        let expect = silence.clone();
        Gate::new(false).process(&mut silence);
        Agc::new(false).process(&mut silence);
        Limiter::new(false).process(&mut silence);
        assert_eq!(silence, expect);
    }

    #[test]
    fn gate_opens_holds_then_releases() {
        // 门限 -45 dB：幅度 0.5 的正弦 RMS ≈ -9 dB，远高于门限；静音块低于门限。
        let mut gate = Gate::new(true);
        // 初始关：静音块增益向零收敛后保持零。
        let mut block = vec![0.0f32; BLOCK];
        gate.process(&mut block);
        assert!(block.iter().all(|&v| v == 0.0), "初始关门期静音块应保持全零");

        // 开门：attack 5 ms → 步长 1/240；960 样本内增益到 1（第 240 个样本起满增益）。
        let loud = sine(440.0, 0.5);
        let mut block = loud.clone();
        gate.process(&mut block);
        assert_eq!(block[0], 0.0, "增益从零起，首样本必须为零");
        // 字面曲线：第 i 个样本增益 = min((i+1)/240, 1)。
        for i in 0..BLOCK {
            let expect = loud[i] * ((i + 1) as f32 / 240.0).min(1.0);
            assert!(
                (block[i] - expect).abs() < 1e-6,
                "样本 {i}：attack 斜坡错配"
            );
        }

        // 保持：60 ms @ 20 ms 块折算 4 块；第 4 个静音块内 hold 计数到 0 仍开门，
        // 第 5 个静音块才关门（旧 `--g->hold_blocks < 0` 语义）。
        for _ in 0..5 {
            gate.process(&mut vec![0.0f32; BLOCK]);
        }

        // 关门后：低于门限的直流块（RMS -46 dB）按 release 150 ms 降增益。
        // 第 5 个静音块已降 960 步，本块首样本再降一步共 961 步（先推进后相乘）。
        let mut block = vec![0.005f32; BLOCK];
        gate.process(&mut block);
        let expect = 0.005f32 * (1.0f32 - 961.0 / 7200.0);
        // 960+ 步 f32 累积的最坏误差约 3e-5（增益），容差 1e-6 留一个量级余量
        // （同 route.rs 淡变曲线的累积容差先例）。
        assert!(
            (block[0] - expect).abs() < 1e-6,
            "release 首样本 {}，期望 {expect}",
            block[0]
        );
        assert!(block[0] < 0.005, "release 必须已把增益降到 1 以下");
        // 块内继续下降：末样本增益低于首样本。
        assert!(block[BLOCK - 1] < block[0], "release 增益必须逐样本下降");
    }

    #[test]
    fn agc_rises_toward_target_only_amplifying() {
        // 幅度 0.05 的正弦：RMS = 0.05/√2 ≈ -29.03 dB；目标 -20 dB → 期望增益
        // 约 9.03 dB。首块一阶平滑：gain = 0.02 × desired。
        let mut agc = Agc::new(true);
        let block = sine(440.0, 0.05);
        let mut processed = block.clone();
        agc.process(&mut processed);
        let rms = rms_db(&block);
        let desired = (-20.0 - rms).clamp(0.0, 24.0);
        let gain_db = 0.02 * desired;
        let gain = 10.0f32.powf(gain_db / 20.0);
        for (i, (&actual, &input)) in processed.iter().zip(block.iter()).enumerate() {
            assert!(
                (actual - input * gain).abs() < 1e-6,
                "样本 {i}：首块增益错配"
            );
        }

        // 只放大不衰减：满幅输入（RMS ≈ -3 dB > 目标）期望增益 0，样本不变。
        let mut agc = Agc::new(true);
        let mut loud = sine(440.0, 1.0);
        let expect = loud.clone();
        agc.process(&mut loud);
        assert_eq!(loud, expect, "期望增益为 0，不得衰减");
    }

    #[test]
    fn limiter_clamps_to_ceiling_and_releases() {
        let ceiling = 10.0f32.powf(-1.0 / 20.0);
        let mut limiter = Limiter::new(true);
        // 满幅余弦：首样本即峰值，高于 -1 dB 上限 → 立即 attack 并钳位。
        let mut block: Vec<f32> = (0..BLOCK)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).cos())
            .collect();
        limiter.process(&mut block);
        let peak = block.iter().fold(0.0f32, |acc, &v| acc.max(v.abs()));
        assert!(
            (peak - ceiling).abs() < 1e-5,
            "峰值 {peak} 必须钳到上限 {ceiling}"
        );
        // 首样本即被钳（立即 attack 语义）。
        assert!((block[0] - ceiling).abs() < 1e-5);

        // 静音块包络指数回落，增益向 1 恢复但不超 1。
        let mut quiet = vec![0.0f32; BLOCK];
        limiter.process(&mut quiet);
        assert!(quiet.iter().all(|&v| v == 0.0));
        // 连续静音后处理弱信号：增益部分恢复，输出大于钳位值、小于原值×1。
        let weak = sine(440.0, 0.2);
        let mut block = weak.clone();
        limiter.process(&mut block);
        let peak = block.iter().fold(0.0f32, |acc, &v| acc.max(v.abs()));
        assert!(peak > 0.0 && peak <= 0.2 + 1e-6, "恢复期输出 {peak} 应在 (0, 0.2]");
    }

    #[test]
    fn silence_level_is_minus_120_db() {
        // 旧约定：零块的均方电平返回 -120 dBFS，不产生 -inf。
        let silence = [0.0f32; 8];
        assert_eq!(rms_db(&silence), SILENCE_DB);
        let loud = [1.0f32, -1.0];
        assert_eq!(rms_db(&loud), 0.0);
    }
}
