//! 24 kHz → 16 kHz 线性插值重采样（SPEC-05 播报投递：纯算术、逐块，算法沿用旧实现）。
//! TTS 声码器原生输出 24 kHz 单声道 f32；RVC 输入统一 16 kHz 契约帧。

use mozart_core::INPUT_SAMPLE_RATE;

/// PocketTTS 声码器原生采样率。
pub const TTS_NATIVE_SAMPLE_RATE: u32 = 24_000;

/// 线性插值降采样 24 kHz → 16 kHz（比率 3:2）。空输入返回空。
pub fn resample_24k_to_16k(input: &[f32]) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let ratio = TTS_NATIVE_SAMPLE_RATE as f64 / INPUT_SAMPLE_RATE as f64;
    let out_len = (input.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input[idx.min(input.len() - 1)];
            let b = input[(idx + 1).min(input.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已知 24 kHz 正弦（440 Hz，0.1 s）：长度收缩 3:2，偶数位精确落在输入采样点，
    /// 奇数位是相邻两输入点的均值。
    #[test]
    fn sine_length_and_interpolated_values() {
        let input: Vec<f32> = (0..2400)
            .map(|i| {
                let t = i as f64 / TTS_NATIVE_SAMPLE_RATE as f64;
                (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32
            })
            .collect();
        let out = resample_24k_to_16k(&input);
        assert_eq!(out.len(), 1600);
        for (i, &value) in out.iter().enumerate() {
            let pos = i as f64 * 1.5;
            let idx = pos as usize;
            let expected = if i % 2 == 0 {
                input[idx]
            } else {
                (input[idx] + input[idx + 1]) / 2.0
            };
            assert!(
                (value - expected).abs() < 1e-6,
                "样本 {i}：{value} != {expected}"
            );
        }
    }

    /// 线性斜坡经线性插值精确重现：out[i] = 1.5 * i。
    #[test]
    fn ramp_is_exact() {
        let input: Vec<f32> = (0..900).map(|i| i as f32).collect();
        let out = resample_24k_to_16k(&input);
        assert_eq!(out.len(), 600);
        for (i, &value) in out.iter().enumerate() {
            assert!((value - 1.5 * i as f32).abs() < 1e-4, "样本 {i}：{value}");
        }
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(resample_24k_to_16k(&[]).is_empty());
    }

    /// 不足一个输出样本时输出为空，不越界读输入。
    #[test]
    fn single_sample_yields_no_output() {
        assert!(resample_24k_to_16k(&[0.5]).is_empty());
    }
}