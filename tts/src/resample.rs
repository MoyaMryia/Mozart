//! 播报重采样（SPEC-05 播报投递：纯算术、逐块，算法沿用旧实现）。
//! TTS 声码器原生输出 24 kHz 单声道 f32。v1 不走变声：重采样到播放采样率
//! 48 kHz 直接喂 pre 播放；经变声输出（FUTURE）时重采样到 16 kHz 契约帧喂 RVC。

use mozart_core::INPUT_SAMPLE_RATE;

/// PocketTTS 声码器原生采样率。
pub const TTS_NATIVE_SAMPLE_RATE: u32 = 24_000;

/// 24 kHz → 48 kHz 线性插值。句内回调之间保留尚未收到右邻点的半个采样间隔。
#[derive(Default)]
pub struct Upsample2 {
    last: Option<f32>,
}

impl Upsample2 {
    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity(input.len() * 2);
        for &sample in input {
            if let Some(last) = self.last {
                output.push(last + (sample - last) * 0.5);
            }
            output.push(sample);
            self.last = Some(sample);
        }
        output
    }

    /// 只在整句结束时重复最后一个样本，保留精确的 2:1 输出时长。
    pub fn finish(&mut self) -> Option<f32> {
        self.last.take()
    }
}

#[cfg(test)]
fn resample_24k_to_48k(input: &[f32]) -> Vec<f32> {
    let mut resampler = Upsample2::default();
    let mut pcm = resampler.push(input);
    pcm.extend(resampler.finish());
    pcm
}

/// 线性插值降采样 24 kHz → 16 kHz（比率 3:2）。空输入返回空。
/// FUTURE：播报经变声输出时重采样到 16 kHz 契约帧喂 RVC；v1 不走此路径。
#[allow(dead_code)]
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

    #[test]
    fn interpolates_across_chunk_boundaries_and_only_extends_the_utterance_end() {
        let mut resampler = Upsample2::default();
        assert_eq!(resampler.push(&[0.0]), [0.0]);
        assert!(resampler.push(&[]).is_empty());
        assert_eq!(resampler.push(&[1.0, -1.0]), [0.5, 1.0, 0.0, -1.0]);
        assert_eq!(resampler.finish(), Some(-1.0));
        assert_eq!(resampler.finish(), None);
    }

    #[test]
    fn split_sine_matches_the_independent_midpoint_reference() {
        let wave: Vec<f32> = (0..1037).map(|i| (i as f32 * 0.27).sin()).collect();
        let expected: Vec<f32> = wave.windows(2).flat_map(|w| [w[0], (w[0] + w[1]) / 2.0])
            .chain([wave[1036]; 2]).collect();
        for size in [1, 2, 17, 480, 1037] {
            let mut resampler = Upsample2::default();
            let mut actual: Vec<f32> = wave.chunks(size).flat_map(|p| resampler.push(p)).collect();
            actual.extend(resampler.finish());
            assert_eq!(actual.len(), expected.len());
            assert!(actual.iter().zip(&expected).all(|(a, b)| (a - b).abs() < 1e-6));
        }
    }

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

    /// 已知 24 kHz 正弦（440 Hz，0.1 s）：长度扩张 1:2，偶数位精确落在输入采样点，
    /// 奇数位是相邻两输入点的均值。
    #[test]
    fn upsample_sine_length_and_interpolated_values() {
        let input: Vec<f32> = (0..2400)
            .map(|i| {
                let t = i as f64 / TTS_NATIVE_SAMPLE_RATE as f64;
                (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32
            })
            .collect();
        let out = resample_24k_to_48k(&input);
        assert_eq!(out.len(), 4800);
        let n = input.len();
        for (i, &value) in out.iter().enumerate() {
            let expected = if i % 2 == 0 {
                input[i / 2]
            } else {
                // 末位奇数样本的右邻越界，钳位到最后一个输入点。
                (input[i / 2] + input[(i / 2 + 1).min(n - 1)]) / 2.0
            };
            assert!(
                (value - expected).abs() < 1e-6,
                "样本 {i}：{value} != {expected}"
            );
        }
    }

    /// 线性斜坡经线性插值精确重现：out[i] = 0.5 * i；末位钳位到最后一个输入点。
    #[test]
    fn upsample_ramp_is_exact() {
        let input: Vec<f32> = (0..900).map(|i| i as f32).collect();
        let out = resample_24k_to_48k(&input);
        assert_eq!(out.len(), 1800);
        for (i, &value) in out.iter().enumerate() {
            let expected = if i == out.len() - 1 {
                899.0 // 末位奇数样本右邻越界，钳位到 input[899]。
            } else {
                0.5 * i as f32
            };
            assert!((value - expected).abs() < 1e-4, "样本 {i}：{value}");
        }
    }

    #[test]
    fn upsample_empty_input_yields_empty_output() {
        assert!(resample_24k_to_48k(&[]).is_empty());
    }

    /// 单样本升采样得两个样本：末位钳位到唯一点，不越界读输入。
    #[test]
    fn upsample_single_sample_clamps_tail() {
        assert_eq!(resample_24k_to_48k(&[0.5]), vec![0.5, 0.5]);
    }
}