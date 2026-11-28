//! 同层降噪模块化抽象与运行时调度（SPEC-02）：
//! 所有降噪后端（如 48 kHz 同步的 WOLA、16 kHz IPC 的 UL-UNAS 及后续扩展模型）
//! 在此作为对等模块接入。控制面只更新原子目标，处理线程在帧边界无锁完成切换与状态重置。

#[cfg(test)]
mod tests;
pub mod ulunas;
pub mod wola;

use crate::{ai, state::Shared};
use mozart_core::denoise::{DenoiseBackend, DenoiseMode, DenoiseStage};
use mozart_core::{InputFrame, RawFrame, RAW_SAMPLES};
use mozart_dsp::TrunkProcessor;

/// 48 kHz 主干同步降噪后端统一接口。
/// 未来接入新的本地实时降噪模型（如 DeepFilterNet/RNNoise）只需实现此 trait
/// 并在 `build_trunk_denoisers` 注册，即可自动支持运行时热切换与前端选项。
pub trait TrunkDenoiser: Send {
    fn id(&self) -> DenoiseBackend;
    fn reset(&mut self);
    fn process(
        &mut self,
        input: &[f32; RAW_SAMPLES],
        out: &mut [f32; RAW_SAMPLES],
    ) -> (usize, Option<f32>);
}

fn build_trunk_denoisers() -> Vec<Box<dyn TrunkDenoiser>> {
    vec![Box::new(wola::WolaBackend::new())]
}

#[derive(Clone, Copy)]
pub struct WorkItem {
    pub frame: InputFrame,
    /// 采集序号用于识别上游环丢帧；打包序号在消费后仍连续。
    pub source_idx: u32,
    pub rvc_ai_active: bool,
    pub file_generation: u64,
    pub epoch: u64,
    /// 需交由 `mozart-enhance` IPC 执行增强的 16 kHz 后端；`None` 表示直接扇出。
    pub enhance_backend: Option<DenoiseBackend>,
}

pub fn fanout(item: &WorkItem, shared: &Shared, senders: &mut [ai::Sender]) {
    for (index, sender) in senders.iter_mut().enumerate() {
        let _permit = if shared.consumers()[index].file_mode_affected {
            if !item.rvc_ai_active {
                continue;
            }
            let Some(permit) = shared.begin_ai_send(item.file_generation) else {
                continue;
            };
            Some(permit)
        } else {
            None
        };
        sender.send(&item.frame, shared);
    }
}

/// 实时处理线程持有的降噪执行器：统一承载关闭、本地 48 kHz 模型与远程 16 kHz 模型。
pub struct DenoiseProcessor {
    trunk_denoisers: Vec<Box<dyn TrunkDenoiser>>,
    applied_mode: DenoiseMode,
    last_capture_epoch: u64,
}

impl DenoiseProcessor {
    pub fn new(initial_mode: DenoiseMode) -> Self {
        Self {
            trunk_denoisers: build_trunk_denoisers(),
            applied_mode: initial_mode,
            last_capture_epoch: 0,
        }
    }

    pub fn sync(&mut self, target_mode: DenoiseMode, capture_epoch: u64) {
        if target_mode != self.applied_mode {
            if let DenoiseMode::Backend(backend) = target_mode {
                self.reset_trunk_backend(backend);
            }
            self.applied_mode = target_mode;
        }
        if capture_epoch != self.last_capture_epoch {
            if let DenoiseMode::Backend(backend) = self.applied_mode {
                self.reset_trunk_backend(backend);
            }
            self.last_capture_epoch = capture_epoch;
        }
    }

    fn reset_trunk_backend(&mut self, backend: DenoiseBackend) {
        if let Some(denoiser) = self
            .trunk_denoisers
            .iter_mut()
            .find(|item| item.id() == backend)
        {
            denoiser.reset();
        }
    }

    pub fn process_trunk_frame<F>(
        &mut self,
        pipeline: &mut TrunkProcessor,
        frame: &mut RawFrame,
        post_hp: F,
    ) -> Option<f32>
    where
        F: FnMut(&mut [f32; RAW_SAMPLES]),
    {
        let mode = self.applied_mode;
        let trunk_denoisers = &mut self.trunk_denoisers;
        pipeline.process_frame(frame, post_hp, |input, out| match mode {
            DenoiseMode::Backend(backend) if backend.stage() == DenoiseStage::Trunk48k => {
                let denoiser = trunk_denoisers
                    .iter_mut()
                    .find(|item| item.id() == backend)
                    .expect("48 kHz 降噪后端未注册到 build_trunk_denoisers");
                denoiser.process(input, out)
            }
            _ => {
                *out = *input;
                (RAW_SAMPLES, None)
            }
        })
    }

    pub fn build_work_item(
        &self,
        frame: InputFrame,
        source_idx: u32,
        rvc_ai_active: bool,
        file_generation: u64,
        epoch: u64,
    ) -> WorkItem {
        let enhance_backend = match self.applied_mode {
            DenoiseMode::Backend(backend) if backend.stage() == DenoiseStage::Enhance16k => {
                Some(backend)
            }
            _ => None,
        };
        WorkItem {
            frame,
            source_idx,
            rvc_ai_active,
            file_generation,
            epoch,
            enhance_backend,
        }
    }
}
