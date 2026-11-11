//! 80 Hz 二阶高通（Butterworth，f0=80 Hz @ 48 kHz）：旧 dsp.c `highpass_80hz`
//! （L90-102）的逐行移植。f32 递归滤波、同运算顺序与精度，对拍目标为逐位一致
//! （两端 Rust 均不做 FMA 收缩，SSE2/NEON 的 mul+add 即 IEEE 语义）。

/// 系数来自旧 dsp.c L16-20，f32 字面值逐位同值。
const HP_B0: f32 = 0.99262254;
const HP_B1: f32 = -1.98524509;
const HP_B2: f32 = 0.99262254;
const HP_A1: f32 = -1.98519066;
const HP_A2: f32 = 0.98529951;

pub(super) struct Highpass80 {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Highpass80 {
    pub(super) const fn new() -> Self {
        Self { x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    /// 就地滤波一块；表达式与旧实现同序（先算 y，再滚状态）。
    pub(super) fn process(&mut self, samples: &mut [f32]) {
        for x in samples.iter_mut() {
            let y = HP_B0 * *x + HP_B1 * self.x1 + HP_B2 * self.x2
                - HP_A1 * self.y1 - HP_A2 * self.y2;
            self.x2 = self.x1;
            self.x1 = *x;
            self.y2 = self.y1;
            self.y1 = y;
            *x = y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 稳态正弦幅度（截尾段最大绝对值）。
    fn steady_amplitude(freq: f32, samples: usize) -> f32 {
        let mut hp = Highpass80::new();
        let mut buf = [0f32; 960];
        let mut peak = 0.0f32;
        for block in 0..(samples / 960) {
            for (i, v) in buf.iter_mut().enumerate() {
                let t = (block * 960 + i) as f32;
                *v = (2.0 * core::f32::consts::PI * freq * t / 48000.0).sin();
            }
            hp.process(&mut buf);
            if block >= samples / 960 - 4 {
                peak = peak.max(buf.iter().fold(0.0f32, |m, v| m.max(v.abs())));
            }
        }
        peak
    }

    #[test]
    fn dc_input_decays_to_zero() {
        let mut hp = Highpass80::new();
        let mut buf = [1.0f32; 960];
        for _ in 0..50 {
            // process 就地写输出，必须每块重灌常数输入，不能把输出反馈当输入
            buf = [1.0f32; 960];
            hp.process(&mut buf);
        }
        // 旧 C 同输入收敛到 f32 非规格化小量（~5e-44）；门限取宽松的 1e-6
        for v in buf.iter() {
            assert!(v.abs() < 1e-6, "直流残差 {v}");
        }
    }

    #[test]
    fn cutoff_frequency_attenuates_to_3db() {
        // Butterworth 高通在截止频率幅度 = 1/√2。80 Hz 周期 600 样本 < 960，
        // 每块采样覆盖全部相位，峰值法无相位缺口。
        let amp = steady_amplitude(80.0, 48000);
        assert!((0.68..=0.73).contains(&amp), "80 Hz 幅度 {amp}");
    }

    #[test]
    fn passband_stays_flat() {
        // 8 kHz 周期 6 样本，峰值法有相位缺口；改用 RMS 比值（每块恰 160 个
        // 整周期，RMS 比即精确增益 |H|）。
        let mut hp = Highpass80::new();
        let mut in_energy = 0.0f64;
        let mut out_energy = 0.0f64;
        for block in 0..50 {
            let mut buf = [0f32; 960];
            for (i, v) in buf.iter_mut().enumerate() {
                let t = (block * 960 + i) as f32;
                *v = (2.0 * core::f32::consts::PI * 8000.0 * t / 48000.0).sin();
            }
            let measure = block >= 5; // 弃 5 块预热，瞬态 |pole|^4800 已消失
            if measure {
                for v in buf {
                    in_energy += (v * v) as f64;
                }
            }
            hp.process(&mut buf);
            if measure {
                for v in buf {
                    out_energy += (v * v) as f64;
                }
            }
        }
        let gain = (out_energy / in_energy).sqrt();
        assert!((0.99..=1.01).contains(&gain), "8 kHz 增益 {gain}");
    }

    #[test]
    fn chunking_does_not_change_output() {
        let mut whole = Vec::with_capacity(48000);
        for i in 0..48000 {
            whole.push((i as f32 * 0.001).sin() * 0.5);
        }
        let mut a = Highpass80::new();
        let mut out_a = whole.clone();
        a.process(&mut out_a);
        let mut b = Highpass80::new();
        let mut out_b = Vec::with_capacity(48000);
        for chunk in whole.chunks(313) {
            let mut c = chunk.to_vec();
            b.process(&mut c);
            out_b.extend_from_slice(&c);
        }
        assert_eq!(out_a, out_b);
    }
}
