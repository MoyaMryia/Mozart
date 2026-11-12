//! 3:1 FIR 降采样到 16 kHz（旧 dsp.c `decimate_3x` L123-139 等价重写）。
//! 33 抽头 7.2 kHz Kaiser 低通抑制 16 kHz Nyquist 以上混叠；环形历史跨帧连续，
//! 每帧 960 样本 → 320 样本，取第 3、6、… 个输入样本的时间点。
//! 纯 f32 乘加、无超越函数：同序运算逐位可复现（对拍断言按位相等）。

use mozart_core::{INPUT_SAMPLES, RAW_SAMPLES};

/// 抽头数（旧 dsp.c `DECIMATOR_TAPS`）。
const TAPS: usize = 33;

/// Kaiser 窗系数（旧 dsp.c L23-35 照搬，逐位同值）。
#[rustfmt::skip]
const COEFFS: [f32; TAPS] = [
     4.2914726915e-04,  1.4895510869e-03,  1.5189746882e-03,
    -1.2652814003e-03, -5.8142304759e-03, -7.0800492720e-03,
     0.0000000000e+00,  1.3328130710e-02,  2.0914031264e-02,
     8.9839746220e-03, -2.2538271381e-02, -5.0922926864e-02,
    -4.0588192824e-02,  3.0283637766e-02,  1.4611043959e-01,
     2.5519851986e-01,  2.9990509072e-01,  2.5519851986e-01,
     1.4611043959e-01,  3.0283637766e-02, -4.0588192824e-02,
    -5.0922926864e-02, -2.2538271381e-02,  8.9839746220e-03,
     2.0914031264e-02,  1.3328130710e-02,  0.0000000000e+00,
    -7.0800492720e-03, -5.8142304759e-03, -1.2652814003e-03,
     1.5189746882e-03,  1.4895510869e-03,  4.2914726915e-04,
];

pub struct Decimator {
    /// 环形历史：history[pos] 为最旧样本。
    history: [f32; TAPS],
    pos: usize,
}

impl Decimator {
    pub fn new() -> Self {
        Self {
            history: [0.0; TAPS],
            pos: 0,
        }
    }

    /// 960 样本 → 320 样本；输出对应输入样本 2、5、8、… 的时间点。
    pub fn process(&mut self, input: &[f32; RAW_SAMPLES], out: &mut [f32; INPUT_SAMPLES]) {
        let mut out_pos = 0;
        for (i, &sample) in input.iter().enumerate() {
            self.history[self.pos] = sample;
            self.pos = (self.pos + 1) % TAPS;
            if i % 3 == 2 {
                let mut sum = 0.0f32;
                let mut pos = self.pos;
                for tap in 0..TAPS {
                    pos = if pos == 0 { TAPS - 1 } else { pos - 1 };
                    sum += COEFFS[tap] * self.history[pos];
                }
                out[out_pos] = sum;
                out_pos += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单位冲激经降采样：输出只含抽头 2、5、8、…（相位与系数取向的字面证明）。
    #[test]
    fn impulse_response_taps_coefficients_two_five_eight() {
        let mut dec = Decimator::new();
        let mut input = [0.0f32; RAW_SAMPLES];
        input[0] = 1.0;
        let mut out = [0.0f32; INPUT_SAMPLES];
        dec.process(&input, &mut out);
        // 冲激后 11 个输出点非零（3k+2 ≤ 32），其后历史排空。
        let mut expect = [0.0f32; INPUT_SAMPLES];
        for k in 0..11 {
            expect[k] = COEFFS[3 * k + 2];
        }
        assert_eq!(out, expect, "冲激响应必须命中抽头 2、5、8、…");
        assert!(out[11..].iter().all(|&v| v == 0.0));
    }

    /// 与直接卷积逐位一致：out[k] = Σ_tap c_tap · in[3k+2−tap]（负索引补零）。
    /// 直接卷积用自然顺序累加，与环形历史的 tap 升序累加同序。
    #[test]
    fn matches_direct_convolution_bitwise() {
        // 3 帧正弦（1 kHz 全幅 + 首样本冲激扰动），跨帧验证历史连续性。
        let mut stream = [0.0f32; RAW_SAMPLES * 3];
        stream[0] = 1.0;
        for (i, slot) in stream.iter_mut().enumerate() {
            *slot += (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin() * 0.8;
        }
        let mut dec = Decimator::new();
        let mut chunked = [0.0f32; INPUT_SAMPLES * 3];
        for (frame, slot) in stream
            .chunks(RAW_SAMPLES)
            .zip(chunked.chunks_mut(INPUT_SAMPLES))
        {
            let input: &[f32; RAW_SAMPLES] = frame.try_into().unwrap();
            let out: &mut [f32; INPUT_SAMPLES] = slot.try_into().unwrap();
            dec.process(input, out);
        }

        let flat_in: Vec<f32> = stream.to_vec();
        for (k, &actual) in chunked.iter().enumerate() {
            let mut sum = 0.0f32;
            let index = 3 * k + 2;
            for tap in 0..TAPS {
                // 历史未填满时环形槽位保持 0.0：零延拓同样参与乘加（含 -0.0 符号行为）。
                let src = index as isize - tap as isize;
                let sample = if src >= 0 { flat_in[src as usize] } else { 0.0 };
                sum += COEFFS[tap] * sample;
            }
            assert_eq!(
                actual.to_bits(),
                sum.to_bits(),
                "输出 {k}：环形历史必须与直接卷积逐位一致"
            );
        }
    }

    /// 输出样本数恰为 1/3：960 → 320。
    #[test]
    fn frame_lengths_match_contract() {
        assert_eq!(RAW_SAMPLES / INPUT_SAMPLES, 3);
        assert_eq!(RAW_SAMPLES % INPUT_SAMPLES, 0);
    }
}
