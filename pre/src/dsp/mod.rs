//! pre DSP：48 kHz 单声道主干增强（SPEC-02）。
//! 阶段顺序沿用旧 dsp.c `mozart_dsp_enhance`（L104-121）：FL 取单声道（采集侧
//! 已完成）→ 80 Hz 高通 → WOLA 谱降噪 → gate/AGC/limiter（Task 4）→ 主干。
//! 本任务覆盖前两级。禁用降噪时整级跳过：不走旧实现的旁路重建，无切换收敛
//! 期，无发射延迟，置信度恒 0（SPEC-02 已确认差异）。
//!
//! 帧对齐：WOLA 输出与输入索引对齐但发射滞后 64 样本（首块只出 896）。为保持
//! 20 ms 帧节拍，首块缺口补零——语义同旧 dsp.c `mozart_dsp_process` 的
//! wrapper（L219-223）：不足 960 补零打包、跨帧不丢样本。960 样本定块下发射
//! 数恒 ≤ 960（896/960/960…），不会出现旧注释里 period 分块时代的溢出滚动。
//!
//! 全部缓冲为定长数组：构造后零分配，`process_frame` 可在实时线程逐帧调用。

mod fft;
#[cfg(test)]
mod golden;
mod highpass;
mod noise_suppressor;

use highpass::Highpass80;
use mozart_core::RawFrame;
use noise_suppressor::NoiseSuppressor;

/// 主干帧长：48 kHz × 20 ms = 960，唯一定义点在 core。
use mozart_core::RAW_SAMPLES;

pub struct Dsp {
    denoise: bool,
    hp: Highpass80,
    ns: NoiseSuppressor,
    /// WOLA 发射暂存（发射数可能小于帧长，尾部由调用侧补零）。
    ns_out: [f32; RAW_SAMPLES],
}

impl Dsp {
    /// `denoise = false` 时 WOLA 整级跳过（SPEC-02 改进点）。
    pub fn new(denoise: bool) -> Self {
        Self {
            denoise,
            hp: Highpass80::new(),
            ns: NoiseSuppressor::new(),
            ns_out: [0.0; RAW_SAMPLES],
        }
    }

    /// 就地处理一帧主干（960 样本），返回本帧语音置信度（0..1）。
    /// 置信度语义同旧实现：自上次查询以来的功率加权平均，查询即清零；禁用
    /// 降噪时恒 0。Task 4 的 VAD 与 16 kHz 打包按帧消费该值。
    pub fn process_frame(&mut self, frame: &mut RawFrame) -> f32 {
        // packed 字段按值整块搬出，不取引用（同 playback.rs 的约定）。
        let mut pcm = frame.pcm;
        self.hp.process(&mut pcm);
        if self.denoise {
            let written = self.ns.push(&pcm, &mut self.ns_out);
            // 发射滞后首块缺口补零，保持节拍与输出索引对齐（见模块说明）。
            // written ≤ 960 由 960 定块发射计数保证；越界即 panic，不静默截断。
            self.ns_out[written..].fill(0.0);
            pcm.copy_from_slice(&self.ns_out);
        }
        frame.pcm = pcm;
        self.ns.speech_confidence() as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(seed: u32) -> RawFrame {
        let mut frame = RawFrame {
            meta: mozart_core::FrameMeta {
                pts_ns: 0,
                frame_idx: 0,
                vad_flag: 0,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm: [0.0; RAW_SAMPLES],
        };
        let mut x = seed | 1;
        let mut pcm = [0.0f32; RAW_SAMPLES];
        for v in pcm.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *v = (x >> 9) as f32 / 8388608.0 - 1.0;
        }
        frame.pcm = pcm;
        frame
    }

    #[test]
    fn disabled_denoise_is_pure_highpass_with_zero_delay() {
        // 禁用即跳过：输出 = 高通输出（无 WOLA 发射延迟带来的补零），置信度恒 0
        let mut input = test_frame(0x4D5A5254);
        let mut expected = input;
        let mut expected_pcm = expected.pcm;
        Highpass80::new().process(&mut expected_pcm);
        expected.pcm = expected_pcm;

        let mut dsp = Dsp::new(false);
        let conf = dsp.process_frame(&mut input);
        assert_eq!(conf, 0.0, "禁用降噪时置信度必须恒 0");
        let (actual, expected) = (input.pcm, expected.pcm);
        assert_eq!(actual, expected, "禁用降噪时必须整级跳过");
        // 对照：启用时首块尾部有 64 个补零样本（发射滞后），两者流不同
        let mut enabled = test_frame(0x4D5A5254);
        let mut dsp_on = Dsp::new(true);
        dsp_on.process_frame(&mut enabled);
        let enabled = enabled.pcm;
        assert_eq!(enabled[896..], [0.0; 64], "启用时首块尾部应为补零");
        assert_ne!(enabled[..896], expected[..896]);
    }

    #[test]
    fn enabled_denoise_emits_full_frames_after_first() {
        let mut dsp = Dsp::new(true);
        // 首块消耗掉发射滞后（尾部补零）；其后每块 960 进 960 出，无补零缺口
        let mut first = test_frame(0x4D5A5254);
        assert!((0.0..=1.0).contains(&dsp.process_frame(&mut first)));
        for block in 1..4 {
            let mut frame = test_frame(0x4D5A5254u32.wrapping_add(block));
            let conf = dsp.process_frame(&mut frame);
            assert!((0.0..=1.0).contains(&conf), "置信度范围 0..1");
            let pcm = frame.pcm;
            assert!(
                pcm[896..].iter().any(|v| *v != 0.0),
                "块 {block}：噪声输入下帧尾不得是整段补零"
            );
        }
    }
}
