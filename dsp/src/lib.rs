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

fn rms(samples: &[f32]) -> f32 {
    let power: f32 = samples.iter().map(|&sample| sample * sample).sum();
    (power / samples.len() as f32).sqrt()
}

fn audible(rms: f32) -> bool {
    20.0 * (rms + 1e-12).log10() > -60.0
}

/// 统一驱动状态滞回 VAD：有谱置信度时走门限候选，无光谱置信度时退回纯能量活动检测且 `conf` 写 0。
fn resolve_vad(vad: &mut Vad, conf: Option<f32>, is_audible: bool) -> (u8, u8, u8) {
    match conf {
        Some(conf) => {
            let (vad_flag, segment_id) = vad.update(conf, is_audible);
            let conf_byte = (conf * 255.0 + 0.5) as u8;
            (vad_flag, conf_byte, segment_id)
        }
        None => {
            let (vad_flag, segment_id) = vad.update_activity(is_audible);
            (vad_flag, 0, segment_id)
        }
    }
}

/// 契约帧分析器：增强与旁路共用能量型 VAD，时间轴原样保留。
pub struct InputFrameAnalyzer {
    vad: Vad,
}

impl Default for InputFrameAnalyzer {
    fn default() -> Self {
        Self { vad: Vad::new() }
    }
}

impl InputFrameAnalyzer {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn analyze(&mut self, frame: &mut InputFrame) {
        let pcm = frame.pcm;
        let frame_rms = rms(&pcm);
        let (vad_flag, conf, segment_id) = resolve_vad(&mut self.vad, None, audible(frame_rms));
        frame.meta.energy_db = energy_to_db(frame_rms);
        frame.meta.vad_flag = vad_flag;
        frame.meta.segment_id = segment_id;
        frame.meta.conf = conf;
    }
}

/// 48 kHz WOLA 谱降噪器包装：与主干其余级解耦，供上层按需热插拔调用。
pub struct WolaDenoiser(NoiseSuppressor);

impl Default for WolaDenoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl WolaDenoiser {
    pub fn new() -> Self {
        Self(NoiseSuppressor::new())
    }

    pub fn reset(&mut self) {
        self.0.reset();
    }

    pub fn process(
        &mut self,
        input: &[f32; RAW_SAMPLES],
        out: &mut [f32; RAW_SAMPLES],
    ) -> (usize, Option<f32>) {
        let written = self.0.push(input, out);
        (written, Some(self.0.speech_confidence() as f32))
    }
}

/// 不绑定具体降噪实现的 48 kHz 主干与 16 kHz 打包管线：
/// 80 Hz 高通 → `post_hp`（AEC）→ 可插拔 48 kHz 降噪回调 → gate/AGC/limiter → 3:1 降采样与 VAD。
pub struct TrunkProcessor {
    hp: Highpass80,
    gate: Gate,
    agc: Agc,
    limiter: Limiter,
    ns_out: [f32; RAW_SAMPLES],
    decimator: Decimator,
    vad: Vad,
    frame_idx: u32,
    dec: [f32; INPUT_SAMPLES],
}

impl TrunkProcessor {
    pub fn new(gate: bool, agc: bool, limiter: bool) -> Self {
        Self {
            hp: Highpass80::new(),
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

    pub fn set_shaping(&mut self, gate: bool, agc: bool, limiter: bool) {
        self.gate.set_enabled(gate);
        self.agc.set_enabled(agc);
        self.limiter.set_enabled(limiter);
    }

    /// 就地处理一帧主干（960 样本），由调用方注入可选的 48 kHz 降噪级。
    pub fn process_frame<F, D>(
        &mut self,
        frame: &mut RawFrame,
        mut post_hp: F,
        mut denoise_stage: D,
    ) -> Option<f32>
    where
        F: FnMut(&mut [f32; RAW_SAMPLES]),
        D: FnMut(&[f32; RAW_SAMPLES], &mut [f32; RAW_SAMPLES]) -> (usize, Option<f32>),
    {
        // packed 字段按值整块搬出，不取引用（同 playback.rs 的约定）。
        let mut pcm = frame.pcm;
        self.hp.process(&mut pcm);
        post_hp(&mut pcm);
        let (written, conf) = denoise_stage(&pcm, &mut self.ns_out);
        // 整形级作用于发射样本；禁用级逐位恒等跳过（stages.rs 约定）。
        self.gate.process(&mut self.ns_out[..written]);
        self.agc.process(&mut self.ns_out[..written]);
        self.limiter.process(&mut self.ns_out[..written]);
        // 发射滞后首块缺口补零，保持节拍与输出索引对齐（见模块说明）。
        // written ≤ 960 由定块发射计数保证；越界即 panic，不静默截断。
        self.ns_out[written..].fill(0.0);
        frame.pcm = self.ns_out;
        conf
    }

    /// 打包 16 kHz 契约帧（旧 `mozart_dsp_pack_frame` 等价）。
    /// `conf` 为 `Some` 时驱动谱置信度 VAD，为 `None`（未跑 48 kHz 谱降噪或已关闭）时
    /// 退回主干可闻度能量 VAD 并保持 `meta.conf = 0`。
    pub fn pack_frame(&mut self, trunk: &RawFrame, conf: Option<f32>) -> InputFrame {
        // packed 字段按值整块搬出，不取引用。
        let meta = trunk.meta;
        let trunk_pcm = trunk.pcm;
        self.decimator.process(&trunk_pcm, &mut self.dec);
        for sample in self.dec.iter_mut() {
            *sample = sample.clamp(
                -mozart_core::INPUT_PCM_ABS_MAX,
                mozart_core::INPUT_PCM_ABS_MAX,
            );
        }

        // 能量取降采样并钳位后的契约帧（旧 dsp.c L162-165）。
        let energy_db = energy_to_db(rms(&self.dec));
        // 电平门限：主干 RMS > -60 dB 才可闻（旧 dsp.c L170-174）。
        let is_audible = audible(rms(&trunk_pcm));
        let (vad_flag, conf_byte, segment_id) = resolve_vad(&mut self.vad, conf, is_audible);
        self.frame_idx = self.frame_idx.wrapping_add(1);
        InputFrame {
            meta: FrameMeta {
                pts_ns: meta.pts_ns,
                frame_idx: self.frame_idx,
                vad_flag,
                energy_db,
                conf: conf_byte,
                segment_id,
            },
            pcm: self.dec,
        }
    }
}

/// WOLA + 主干管线便捷门面：供文件离线降噪与黄金对拍复用。
pub struct Dsp {
    denoise: bool,
    wola: WolaDenoiser,
    pipeline: TrunkProcessor,
}

impl Dsp {
    /// 级开关沿用旧 `mozart_dsp_config_t`；整形级默认关（旧 main.c L411）。
    pub fn new(denoise: bool, gate: bool, agc: bool, limiter: bool) -> Self {
        Self {
            denoise,
            wola: WolaDenoiser::new(),
            pipeline: TrunkProcessor::new(gate, agc, limiter),
        }
    }

    /// 运行时级开关（SPEC-02 SetDsp）。gate/agc/limiter 直接切换；
    /// denoise 重新启用时重置 WOLA 流式状态，禁用整级跳过。
    pub fn set_switches(&mut self, denoise: bool, gate: bool, agc: bool, limiter: bool) {
        if denoise && !self.denoise {
            self.wola.reset();
        }
        self.denoise = denoise;
        self.pipeline.set_shaping(gate, agc, limiter);
    }

    /// 采集源热切换时重置 WOLA 降噪状态；禁用时无操作。
    pub fn reset_denoise(&mut self) {
        if self.denoise {
            self.wola.reset();
        }
    }

    /// 就地处理一帧主干（960 样本），启用降噪时返回 `Some(conf)`，禁用时返回 `None`。
    pub fn process_frame(&mut self, frame: &mut RawFrame) -> Option<f32> {
        self.process_frame_with_hook(frame, |_| {})
    }

    /// 就地处理一帧主干（960 样本），并在 80 Hz 高通之后调用 `post_hp` 钩子。
    pub fn process_frame_with_hook<F: FnMut(&mut [f32; RAW_SAMPLES])>(
        &mut self,
        frame: &mut RawFrame,
        post_hp: F,
    ) -> Option<f32> {
        let denoise_enabled = self.denoise;
        let wola = &mut self.wola;
        self.pipeline.process_frame(frame, post_hp, |input, out| {
            if denoise_enabled {
                wola.process(input, out)
            } else {
                *out = *input;
                (RAW_SAMPLES, None)
            }
        })
    }

    pub fn pack_frame(&mut self, trunk: &RawFrame, conf: Option<f32>) -> InputFrame {
        self.pipeline.pack_frame(trunk, conf)
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
        assert_eq!(conf, None, "禁用降噪时应返回 None（无谱置信度）");
        let packed = dsp.pack_frame(&input, conf);
        assert_eq!(packed.meta.conf, 0, "禁用降噪时 conf 字节必须恒 0");
        assert_eq!(packed.meta.vad_flag, 1, "禁用降噪时主干可闻应由能量 VAD 点亮 vad_flag");
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
    fn set_switches_denoise_toggle_resets_wola_state() {
        // 运行中切换 denoise（SPEC-02 SetDsp）：禁用整级跳过（输出 = 高通，
        // 置信度恒 0）；重新启用重置 WOLA 状态，首块重新攒初始化帧。
        let mut dsp = Dsp::new(true, false, false, false);
        let mut hp = Highpass80::new();
        // 暖机：NS 就绪并发射满帧；参考高通与 dsp 内部高通吃同样的输入。
        for i in 0..8u32 {
            let mut frame = test_frame(0x4D5A5254u32.wrapping_add(i));
            let mut reference = frame.pcm;
            hp.process(&mut reference);
            dsp.process_frame(&mut frame);
        }
        // 禁用：输出与参考高通逐位一致，置信度恒 0。
        dsp.set_switches(false, false, false, false);
        let mut frame = test_frame(100);
        let mut expected = frame.pcm;
        hp.process(&mut expected);
        let conf = dsp.process_frame(&mut frame);
        assert_eq!(conf, None, "禁用降噪后置信度必须为 None");
        // packed 字段按值整块搬出比较，不取引用。
        let actual = frame.pcm;
        assert_eq!(actual, expected, "禁用后必须与高通输出逐位一致");
        // 重新启用：WOLA 状态重置，首块发射 896、尾部 64 补零（同启动首块）；
        // 不重置则 NS 就绪立即发射满 960，此断言失败。
        dsp.set_switches(true, false, false, false);
        let mut frame = test_frame(101);
        let mut reference = frame.pcm;
        hp.process(&mut reference);
        let conf = dsp.process_frame(&mut frame).expect("重新启用后应返回谱置信度");
        assert!((0.0..=1.0).contains(&conf));
        let pcm = frame.pcm;
        assert_eq!(
            pcm[896..],
            [0.0; 64],
            "重新启用首块必须重新攒初始化帧（WOLA 状态已重置）"
        );
    }

    #[test]
    fn set_switches_shaping_stages_toggle_at_runtime() {
        // 各整形级：启用时输出与对照不同；禁用后与从未启用的对照实例
        // 逐位一致（同输入序列、同高通历史），证明开关运行时生效。
        for stage in 0..3u8 {
            let (gate, agc, limiter) = (stage == 0, stage == 1, stage == 2);
            // 信号幅度按级选取：AGC 只放大用弱信号；limiter 上限 -1 dB，
            // 高通去直流后峰值仍需超上限；gate 用满幅噪声开门。
            let amplitude = if agc {
                0.05
            } else if limiter {
                2.0
            } else {
                1.0
            };
            let mut dsp = Dsp::new(false, gate, agc, limiter);
            let mut reference = Dsp::new(false, false, false, false);
            let mut differs = false;
            for i in 0..8u32 {
                let mut a = test_frame(0x4D5A5254u32.wrapping_add(i));
                // packed 字段按值整块搬出改写，不取引用。
                let mut pcm = a.pcm;
                for v in pcm.iter_mut() {
                    *v *= amplitude;
                }
                a.pcm = pcm;
                let mut b = a;
                dsp.process_frame(&mut a);
                reference.process_frame(&mut b);
                let (pa, pb) = (a.pcm, b.pcm);
                differs |= pa != pb;
            }
            assert!(differs, "级 {stage}：启用必须改变输出");
            dsp.set_switches(false, false, false, false);
            let mut a = test_frame(999);
            let mut pcm = a.pcm;
            for v in pcm.iter_mut() {
                *v *= amplitude;
            }
            a.pcm = pcm;
            let mut b = a;
            dsp.process_frame(&mut a);
            reference.process_frame(&mut b);
            let (pa, pb) = (a.pcm, b.pcm);
            assert_eq!(pa, pb, "级 {stage}：禁用后必须与从未启用实例逐位一致");
        }
    }

    #[test]
    fn enabled_denoise_emits_full_frames_after_first() {
        let mut dsp = Dsp::new(true, false, false, false);
        // 首块消耗掉发射滞后（尾部补零）；其后每块 960 进 960 出，无补零缺口
        let mut first = test_frame(0x4D5A5254);
        assert!(dsp.process_frame(&mut first).is_some());
        for block in 1..4 {
            let mut frame = test_frame(0x4D5A5254u32.wrapping_add(block));
            let conf = dsp.process_frame(&mut frame).expect("启用后返回谱置信度");
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
        let first = dsp.pack_frame(&trunk, Some(0.0));
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
        let frame = dsp.pack_frame(&trunk, Some(0.5019608));
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
        let frame = dsp.pack_frame(&trunk, Some(0.0));
        let (meta, pcm) = (frame.meta, frame.pcm);
        assert_eq!(meta.energy_db, 0, "静音帧能量字节为 0（-100 dB 以下截断）");
        assert!(pcm.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn post_hp_hook_runs_after_highpass_and_before_denoise() {
        let mut input = test_frame(0x12345678);
        let mut expected_hp = input.pcm;
        Highpass80::new().process(&mut expected_hp);

        let mut seen = [0.0f32; RAW_SAMPLES];
        let mut dsp = Dsp::new(true, false, false, false);
        dsp.process_frame_with_hook(&mut input, |pcm| {
            seen = *pcm;
            pcm.fill(0.0);
        });
        assert_eq!(seen, expected_hp, "钩子输入必须是 80 Hz 高通后的主干");
        let after = input.pcm;
        assert!(
            after.iter().all(|&v| v == 0.0),
            "钩子清零后，下游降噪只应看到全零"
        );
    }
}
