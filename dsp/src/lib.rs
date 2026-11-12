//! mozart-dsp：48 kHz 单声道主干增强 + 16 kHz 契约帧打包（SPEC-02）。
//! pre 实时链路与 daemon 离线文件降噪共用本 crate，不做两套实现。
//! 阶段顺序沿用旧 dsp.c `mozart_dsp_enhance`（L104-121）：FL 取单声道（采集侧
//! 已完成）→ 80 Hz 高通 → WOLA 谱降噪 → gate/AGC/limiter → 主干。
//! 禁用降噪时整级跳过：不走旧实现的旁路 WOLA 重建，无切换收敛
//! 期，无发射延迟，置信度恒 0（SPEC-02 已确认差异）。
//!
//! 帧对齐：WOLA 输出与输入索引对齐但发射滞后 64 样本（首块只出 896）。为保持
//! 20 ms 帧节拍，首块缺口补零——语义同旧 dsp.c `mozart_dsp_process` 的
//! wrapper（L219-223）：不足 960 补零打包、跨帧不丢样本。960 样本定块下发射
//! 数恒 ≤ 960（896/960/960…），不会出现旧注释里 period 分块时代的溢出滚动。
//! 整形级作用于发射样本（旧 enhance：stages 在 NS 发射之后、wrapper 补零之前）。
//!
//! 打包（旧 `mozart_dsp_pack_frame` L150-201）：主干 960 帧 → 3:1 FIR → 320，
//! meta 时间轴与主干帧同窗（Task 3 已对齐发射滞后），FIR 历史跨帧连续。
//!
//! 全部缓冲为定长数组：构造后零分配，`process_frame`/`pack_frame`
//! 可在实时线程逐帧调用。

mod decimator;
mod fft;
#[cfg(test)]
mod golden;
mod highpass;
mod noise_suppressor;
mod stages;
mod vad;

use decimator::Decimator;
use highpass::Highpass80;
use mozart_core::{FrameMeta, InputFrame, RawFrame, INPUT_SAMPLES};
use noise_suppressor::NoiseSuppressor;
use stages::{Agc, Gate, Limiter};
use vad::Vad;

/// 主干帧长：48 kHz × 20 ms = 960，唯一定义点在 core。
use mozart_core::RAW_SAMPLES;

/// RMS → 0..255 能量字节（旧 dsp.c `energy_to_db`：-100..0 dB → 0..255，截断）。
fn energy_to_db(rms: f32) -> u8 {
    let db = 20.0 * (rms + 1e-12).log10();
    let v = (db + 100.0) * 2.55;
    v.clamp(0.0, 255.0) as u8
}

pub struct Dsp {
    denoise: bool,
    hp: Highpass80,
    ns: NoiseSuppressor,
    gate: Gate,
    agc: Agc,
    limiter: Limiter,
    /// WOLA 发射暂存（发射数可能小于帧长，尾部由调用侧补零）。
    ns_out: [f32; RAW_SAMPLES],
    decimator: Decimator,
    vad: Vad,
    /// 打包序号（旧 `++frame_idx`，从 1 起）。
    frame_idx: u32,
    /// 降采样暂存。
    dec: [f32; INPUT_SAMPLES],
}

impl Dsp {
    /// 级开关沿用旧 `mozart_dsp_config_t`；整形级默认关（旧 main.c L411）。
    pub fn new(denoise: bool, gate: bool, agc: bool, limiter: bool) -> Self {
        Self {
            denoise,
            hp: Highpass80::new(),
            ns: NoiseSuppressor::new(),
            gate: Gate::new(gate),
            agc: Agc::new(agc),
            limiter: Limiter::new(limiter),
            ns_out: [0.0; RAW_SAMPLES],
            decimator: Decimator::new(),
            vad: Vad::new(),
            frame_idx: 0,
            dec: [0.0; INPUT_SAMPLES],
        }
    }

    /// 就地处理一帧主干（960 样本），返回本帧语音置信度（0..1）。
    /// 置信度语义同旧实现：自上次查询以来的功率加权平均，查询即清零；禁用
    /// 降噪时恒 0。VAD 与 16 kHz 打包经 `pack_frame` 按帧消费该值。
    pub fn process_frame(&mut self, frame: &mut RawFrame) -> f32 {
        // packed 字段按值整块搬出，不取引用（同 playback.rs 的约定）。
        let mut pcm = frame.pcm;
        self.hp.process(&mut pcm);
        let written = if self.denoise {
            self.ns.push(&pcm, &mut self.ns_out)
        } else {
            self.ns_out = pcm;
            RAW_SAMPLES
        };
        // 整形级作用于发射样本；禁用级逐位恒等跳过（stages.rs 约定）。
        self.gate.process(&mut self.ns_out[..written]);
        self.agc.process(&mut self.ns_out[..written]);
        self.limiter.process(&mut self.ns_out[..written]);
        // 发射滞后首块缺口补零，保持节拍与输出索引对齐（见模块说明）。
        // written ≤ 960 由 960 定块发射计数保证；越界即 panic，不静默截断。
        self.ns_out[written..].fill(0.0);
        frame.pcm = self.ns_out;
        if self.denoise {
            self.ns.speech_confidence() as f32
        } else {
            0.0
        }
    }

    /// 打包 16 kHz 契约帧（旧 `mozart_dsp_pack_frame` 等价）。
    /// 输入为 `process_frame` 对齐后的 960 样本主干帧；`conf` 为该帧置信度。
    /// `meta.pts_ns` 取主干帧的采集时间戳（frame.h 语义；旧 main.c L826 在
    /// 打包时刻改写为 now−20 ms，不沿用）。`meta.frame_idx` 为打包序号。
    pub fn pack_frame(&mut self, trunk: &RawFrame, conf: f32) -> InputFrame {
        // packed 字段按值整块搬出，不取引用。
        let meta = trunk.meta;
        let trunk_pcm = trunk.pcm;
        self.decimator.process(&trunk_pcm, &mut self.dec);
        for sample in self.dec.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }

        // 能量取降采样并钳位后的契约帧（旧 dsp.c L162-165）。
        let mut power = 0.0f32;
        for &sample in &self.dec {
            power += sample * sample;
        }
        let energy_db = energy_to_db((power / INPUT_SAMPLES as f32).sqrt());

        // 电平门限：主干 RMS > -60 dB 才可闻（旧 dsp.c L170-174）。
        let mut trunk_power = 0.0f32;
        for &sample in &trunk_pcm {
            trunk_power += sample * sample;
        }
        let trunk_rms = (trunk_power / RAW_SAMPLES as f32).sqrt();
        let audible = 20.0 * (trunk_rms + 1e-12).log10() > -60.0;

        let (vad_flag, segment_id) = self.vad.update(conf, audible);
        self.frame_idx = self.frame_idx.wrapping_add(1);
        InputFrame {
            meta: FrameMeta {
                pts_ns: meta.pts_ns,
                frame_idx: self.frame_idx,
                vad_flag,
                energy_db,
                conf: (conf * 255.0 + 0.5) as u8,
                segment_id,
            },
            pcm: self.dec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(seed: u32) -> RawFrame {
        let mut frame = RawFrame {
            meta: FrameMeta {
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

        let mut dsp = Dsp::new(false, false, false, false);
        let conf = dsp.process_frame(&mut input);
        assert_eq!(conf, 0.0, "禁用降噪时置信度必须恒 0");
        let (actual, expected) = (input.pcm, expected.pcm);
        assert_eq!(actual, expected, "禁用降噪时必须整级跳过");
        // 对照：启用时首块尾部有 64 个补零样本（发射滞后），两者流不同
        let mut enabled = test_frame(0x4D5A5254);
        let mut dsp_on = Dsp::new(true, false, false, false);
        dsp_on.process_frame(&mut enabled);
        let enabled = enabled.pcm;
        assert_eq!(enabled[896..], [0.0; 64], "启用时首块尾部应为补零");
        assert_ne!(enabled[..896], expected[..896]);
    }

    #[test]
    fn enabled_denoise_emits_full_frames_after_first() {
        let mut dsp = Dsp::new(true, false, false, false);
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

    #[test]
    fn limiter_applies_to_emitted_part_and_zero_fill_stays() {
        // 整形级在 NS 发射之后、wrapper 补零之前（旧 enhance 次序）：
        // 首帧发射 896 样本被限幅，尾部 64 样本保持补零零值。
        let mut input = test_frame(0x4D5A5254);
        // packed 字段按值整块搬出改写，不取引用。
        let mut pcm = input.pcm;
        for (i, slot) in pcm.iter_mut().enumerate() {
            // 满幅两倍正弦：过高通与降噪后仍超 -1 dB 上限。
            *slot = 2.0 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin();
        }
        input.pcm = pcm;
        let mut dsp = Dsp::new(true, false, false, true);
        dsp.process_frame(&mut input);
        let pcm = input.pcm;
        let ceiling = 10.0f32.powf(-1.0 / 20.0);
        let peak = pcm[..896].iter().fold(0.0f32, |acc, &v| acc.max(v.abs()));
        assert!(
            peak <= ceiling * 1.001,
            "发射段峰值 {peak} 必须钳到上限 {ceiling}"
        );
        assert!(peak > ceiling * 0.9, "发射段必须实际被限幅到峰值附近");
        assert_eq!(pcm[896..], [0.0; 64], "补零段不得被整形级改写");
    }

    #[test]
    fn pack_frame_clamps_and_fills_meta() {
        let mut dsp = Dsp::new(true, false, false, false);
        // 直接驱动 pack：满幅直流主干，降采样输出越界被钳到 1.0。
        // 高通不参与（pack 输入即主干帧），直流直流增益恒为有限正值。
        let mut trunk = test_frame(1);
        trunk.pcm = [100.0; RAW_SAMPLES];
        trunk.meta.pts_ns = 0x1234_5678_9ABC_DEF0;
        // 第一帧：FIR 历史暖机，输出为部分和，未全部钳位。
        let first = dsp.pack_frame(&trunk, 0.0);
        let (meta, pcm) = (first.meta, first.pcm);
        let FrameMeta {
            pts_ns,
            frame_idx,
            vad_flag,
            conf,
            segment_id,
            ..
        } = meta;
        assert_eq!(pts_ns, 0x1234_5678_9ABC_DEF0, "pts 取采集时间戳");
        assert_eq!(frame_idx, 1, "打包序号从 1 起（旧 ++frame_idx）");
        assert_eq!(conf, 0);
        assert_eq!(vad_flag, 0, "conf=0 不构成候选");
        assert_eq!(segment_id, 0);
        assert!(
            pcm.iter().all(|&v| (-1.0..=1.0).contains(&v)),
            "降采样输出必须钳位到 ±1"
        );

        // 第二帧：历史已暖，全部输出钳到 1.0 → RMS=1 → 能量字节 255。
        let frame = dsp.pack_frame(&trunk, 0.5019608);
        let (meta, pcm) = (frame.meta, frame.pcm);
        let FrameMeta {
            frame_idx,
            energy_db,
            conf,
            vad_flag,
            segment_id,
            ..
        } = meta;
        assert_eq!(frame_idx, 2, "打包序号单调推进");
        assert_eq!(conf, 128, "0.5019608×255+0.5 截断为 128");
        assert_eq!(energy_db, 255, "满幅 RMS 的能量字节为 255");
        assert!(
            pcm.iter().all(|&v| v == 1.0),
            "暖机后 100×直流增益恒钳到 1.0"
        );
        // conf ≥ 候选门限且主干可闻 → vad_flag 由候选点亮（滞回未完成）。
        assert_eq!(vad_flag, 1, "conf ≥ 0.5 且可闻必须点亮候选");
        assert_eq!(segment_id, 0, "未滞回进入前分段号保持 0");
    }

    #[test]
    fn silence_energy_byte_is_zero() {
        let mut dsp = Dsp::new(true, false, false, false);
        let mut trunk = test_frame(7);
        trunk.pcm = [0.0; RAW_SAMPLES];
        let frame = dsp.pack_frame(&trunk, 0.0);
        let (meta, pcm) = (frame.meta, frame.pcm);
        assert_eq!(meta.energy_db, 0, "静音帧能量字节为 0（-100 dB 以下截断）");
        assert!(pcm.iter().all(|&v| v == 0.0));
    }
}
