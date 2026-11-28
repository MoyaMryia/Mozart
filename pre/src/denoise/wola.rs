//! 48 kHz 主干 WOLA 谱降噪后端适配器（与 UL-UNAS 同层，实现 `TrunkDenoiser`）。

use super::TrunkDenoiser;
use mozart_core::denoise::DenoiseBackend;
use mozart_core::RAW_SAMPLES;
use mozart_dsp::WolaDenoiser;

pub struct WolaBackend(WolaDenoiser);

impl Default for WolaBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WolaBackend {
    pub fn new() -> Self {
        Self(WolaDenoiser::new())
    }
}

impl TrunkDenoiser for WolaBackend {
    fn id(&self) -> DenoiseBackend {
        DenoiseBackend::Wola
    }

    fn reset(&mut self) {
        self.0.reset();
    }

    fn process(
        &mut self,
        input: &[f32; RAW_SAMPLES],
        out: &mut [f32; RAW_SAMPLES],
    ) -> (usize, Option<f32>) {
        self.0.process(input, out)
    }
}
