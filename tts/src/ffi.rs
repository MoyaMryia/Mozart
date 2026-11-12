//! sherpa-onnx C API 声明与合成器抽象（SPEC-05：播报 TTS 链 sherpa-onnx TTS 库）。
//!
//! build.rs 在设置 SHERPA_ONNX_LIB_DIR 时置 `has_sherpa` 并链接 sherpa-onnx-c-api；
//! 此时编译 `real` 模块（PocketTTS 真实 FFI）。未设置时编译 `stub` 模块：
//! 返回固定正弦波，供本机无库构建与测试（覆盖重采样与输出路径）。
//!
//! 结构体布局逐字段对应 sherpa-onnx v1.13.6 `sherpa-onnx/c-api/c-api.h`；
//! 配置结构体先零初始化再填所需字段，未用字段保持零值（C API 约定）。
//! 句柄与结果对象的释放经 Drop / 显式 Destroy 完成。
//!
//! 生成走 SherpaOnnxOfflineTtsGenerateWithConfig：旧 SherpaOnnxOfflineTtsGenerate
//! 只带 sid/speed，无法携带 reference_audio 与 num_steps（上游已弃用）。

use crate::state::Abort;
use std::path::Path;

/// 离线合成器。synth 线程独占使用，跨线程移动需 Send。
pub trait Synthesizer: Send {
    /// 引擎原生输出采样率（Hz）。
    fn sample_rate(&self) -> u32;
    /// 合成整句文本，返回原生采样率单声道 f32 PCM（[-1, 1]）。
    /// reference 为空表示无参考音频。abort 为中止判定：进程停止或 TTS_JOB_STOP
    /// 推进任务代际时置真，真实 FFI 经生成进度回调据此中止在途合成。
    fn synthesize(
        &self,
        text: &str,
        reference: &[f32],
        ref_rate: u32,
        abort: &Abort<'_>,
    ) -> Result<Vec<f32>, String>;
}

/// 加载 PocketTTS 引擎。model_dir 为空或加载失败返回 Err（进程启动路径据此退出）。
/// 测试注入口：MOZART_TTS_STUB=1 时即使链接了 sherpa 也用 stub（进程级集成
/// 测试不依赖模型文件）。
pub fn load(
    model_dir: &Path,
    threads: u32,
    provider: &str,
) -> Result<Box<dyn Synthesizer>, String> {
    if std::env::var("MOZART_TTS_STUB").map(|v| v == "1").unwrap_or(false) {
        return stub::load(model_dir, threads, provider);
    }
    imp::load(model_dir, threads, provider)
}

#[cfg(has_sherpa)]
use real as imp;
#[cfg(not(has_sherpa))]
use stub as imp;

// ===========================================================================
// 真实 FFI（has_sherpa）
// ===========================================================================
#[cfg(has_sherpa)]
mod sys {
    use std::os::raw::{c_char, c_int, c_void};

    // 不透明句柄。
    #[repr(C)]
    pub struct SherpaOnnxOfflineTts {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsVitsModelConfig {
        pub model: *const c_char,
        pub lexicon: *const c_char,
        pub tokens: *const c_char,
        pub data_dir: *const c_char,
        pub noise_scale: f32,
        pub noise_scale_w: f32,
        pub length_scale: f32,
        pub dict_dir: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsMatchaModelConfig {
        pub acoustic_model: *const c_char,
        pub vocoder: *const c_char,
        pub lexicon: *const c_char,
        pub tokens: *const c_char,
        pub data_dir: *const c_char,
        pub noise_scale: f32,
        pub length_scale: f32,
        pub dict_dir: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsKokoroModelConfig {
        pub model: *const c_char,
        pub voices: *const c_char,
        pub tokens: *const c_char,
        pub data_dir: *const c_char,
        pub length_scale: f32,
        pub dict_dir: *const c_char,
        pub lexicon: *const c_char,
        pub lang: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsKittenModelConfig {
        pub model: *const c_char,
        pub voices: *const c_char,
        pub tokens: *const c_char,
        pub data_dir: *const c_char,
        pub length_scale: f32,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsZipvoiceModelConfig {
        pub tokens: *const c_char,
        pub encoder: *const c_char,
        pub decoder: *const c_char,
        pub vocoder: *const c_char,
        pub data_dir: *const c_char,
        pub lexicon: *const c_char,
        pub feat_scale: f32,
        pub t_shift: f32,
        pub target_rms: f32,
        pub guidance_scale: f32,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsPocketModelConfig {
        pub lm_flow: *const c_char,
        pub lm_main: *const c_char,
        pub encoder: *const c_char,
        pub decoder: *const c_char,
        pub text_conditioner: *const c_char,
        pub vocab_json: *const c_char,
        pub token_scores_json: *const c_char,
        pub voice_embedding_cache_capacity: c_int,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsSupertonicModelConfig {
        pub duration_predictor: *const c_char,
        pub text_encoder: *const c_char,
        pub vector_estimator: *const c_char,
        pub vocoder: *const c_char,
        pub tts_json: *const c_char,
        pub unicode_indexer: *const c_char,
        pub voice_style: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsModelConfig {
        pub vits: SherpaOnnxOfflineTtsVitsModelConfig,
        pub num_threads: c_int,
        pub debug: c_int,
        pub provider: *const c_char,
        pub matcha: SherpaOnnxOfflineTtsMatchaModelConfig,
        pub kokoro: SherpaOnnxOfflineTtsKokoroModelConfig,
        pub kitten: SherpaOnnxOfflineTtsKittenModelConfig,
        pub zipvoice: SherpaOnnxOfflineTtsZipvoiceModelConfig,
        pub pocket: SherpaOnnxOfflineTtsPocketModelConfig,
        pub supertonic: SherpaOnnxOfflineTtsSupertonicModelConfig,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTtsConfig {
        pub model: SherpaOnnxOfflineTtsModelConfig,
        pub rule_fsts: *const c_char,
        pub max_num_sentences: c_int,
        pub rule_fars: *const c_char,
        pub silence_scale: f32,
    }
    #[repr(C)]
    pub struct SherpaOnnxGenerationConfig {
        pub silence_scale: f32,
        pub speed: f32,
        pub sid: c_int,
        pub reference_audio: *const f32,
        pub reference_audio_len: c_int,
        pub reference_sample_rate: c_int,
        pub reference_text: *const c_char,
        pub num_steps: c_int,
        pub extra: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxGeneratedAudio {
        pub samples: *const f32,
        pub n: c_int,
        pub sample_rate: c_int,
    }

    pub type SherpaOnnxGeneratedAudioProgressCallbackWithArg =
        Option<extern "C" fn(*const f32, c_int, f32, *mut c_void) -> c_int>;

    extern "C" {
        pub fn SherpaOnnxCreateOfflineTts(
            config: *const SherpaOnnxOfflineTtsConfig,
        ) -> *const SherpaOnnxOfflineTts;
        pub fn SherpaOnnxDestroyOfflineTts(tts: *const SherpaOnnxOfflineTts);
        pub fn SherpaOnnxOfflineTtsSampleRate(tts: *const SherpaOnnxOfflineTts) -> c_int;
        pub fn SherpaOnnxOfflineTtsGenerateWithConfig(
            tts: *const SherpaOnnxOfflineTts,
            text: *const c_char,
            config: *const SherpaOnnxGenerationConfig,
            callback: SherpaOnnxGeneratedAudioProgressCallbackWithArg,
            arg: *mut c_void,
        ) -> *const SherpaOnnxGeneratedAudio;
        pub fn SherpaOnnxDestroyOfflineTtsGeneratedAudio(p: *const SherpaOnnxGeneratedAudio);
    }
}

#[cfg(has_sherpa)]
mod real {
    use super::{sys, Synthesizer};
    use crate::state::Abort;
    use std::ffi::CString;
    use std::mem;
    use std::os::raw::{c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// 生成参数（brief 固定）：flow-matching 步数 5、原速、seed 42 经 extra JSON。
    const NUM_STEPS: i32 = 5;
    const SPEED: f32 = 1.0;
    const EXTRA: &str = r#"{"seed":42}"#;
    /// 每次生成按单句批处理，限制 Orin 上的峰值内存。
    const MAX_NUM_SENTENCES: i32 = 1;

    fn cstring_path(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("路径含 NUL 字节：{}", path.display()))
    }

    pub fn load(
        model_dir: &Path,
        threads: u32,
        provider: &str,
    ) -> Result<Box<dyn Synthesizer>, String> {
        if model_dir.as_os_str().is_empty() {
            return Err("tts.model_dir 未配置，无法加载 PocketTTS 模型".to_string());
        }
        // 模型文件名沿用旧 clone_worker.py 的目录布局；int8 与 fp32 按 lm_flow 存在性选择。
        let suffix = if model_dir.join("lm_flow.int8.onnx").exists() {
            ".int8.onnx"
        } else {
            ".onnx"
        };
        let lm_flow = cstring_path(&model_dir.join(format!("lm_flow{suffix}")))?;
        let lm_main = cstring_path(&model_dir.join(format!("lm_main{suffix}")))?;
        let encoder = cstring_path(&model_dir.join("encoder.onnx"))?;
        let decoder = cstring_path(&model_dir.join(format!("decoder{suffix}")))?;
        let text_conditioner = cstring_path(&model_dir.join("text_conditioner.onnx"))?;
        let vocab_json = cstring_path(&model_dir.join("vocab.json"))?;
        let token_scores_json = cstring_path(&model_dir.join("token_scores.json"))?;
        let provider = CString::new(provider).map_err(|_| "tts.provider 含 NUL 字节".to_string())?;

        // SAFETY: 配置结构体由指针与整数构成，全零位模式合法；随后逐字段赋值。
        let mut config: sys::SherpaOnnxOfflineTtsConfig = unsafe { mem::zeroed() };
        config.model.pocket.lm_flow = lm_flow.as_ptr();
        config.model.pocket.lm_main = lm_main.as_ptr();
        config.model.pocket.encoder = encoder.as_ptr();
        config.model.pocket.decoder = decoder.as_ptr();
        config.model.pocket.text_conditioner = text_conditioner.as_ptr();
        config.model.pocket.vocab_json = vocab_json.as_ptr();
        config.model.pocket.token_scores_json = token_scores_json.as_ptr();
        config.model.num_threads = threads as i32;
        config.model.provider = provider.as_ptr();
        config.max_num_sentences = MAX_NUM_SENTENCES;

        // SAFETY: config 字段指向的 CString 在本调用期间存活；C API 在创建时拷贝配置。
        let handle = unsafe { sys::SherpaOnnxCreateOfflineTts(&config) };
        if handle.is_null() {
            return Err(format!("加载 PocketTTS 模型失败：{}", model_dir.display()));
        }
        Ok(Box::new(PocketTts { handle }))
    }

    /// 生成进度回调：中止判定置真（进程停止或 TTS_JOB_STOP）时返回 0，
    /// 让引擎中止在途合成。
    extern "C" fn abort_on_stop(
        _samples: *const f32,
        _n: c_int,
        _progress: f32,
        arg: *mut c_void,
    ) -> c_int {
        // SAFETY: arg 指向 synthesize 栈上的 Abort，生成调用期间始终有效。
        let abort = unsafe { &*(arg as *const Abort<'_>) };
        if abort.aborted() {
            0
        } else {
            1
        }
    }

    struct PocketTts {
        handle: *const sys::SherpaOnnxOfflineTts,
    }
    // SAFETY: 句柄由 synth 线程独占使用；sherpa-onnx 引擎可跨线程移动。
    unsafe impl Send for PocketTts {}

    impl Drop for PocketTts {
        fn drop(&mut self) {
            // SAFETY: handle 由 CreateOfflineTts 返回且未释放。
            unsafe { sys::SherpaOnnxDestroyOfflineTts(self.handle) };
        }
    }

    impl Synthesizer for PocketTts {
        fn sample_rate(&self) -> u32 {
            // SAFETY: handle 有效。
            unsafe { sys::SherpaOnnxOfflineTtsSampleRate(self.handle) as u32 }
        }

        fn synthesize(
            &self,
            text: &str,
            reference: &[f32],
            ref_rate: u32,
            abort: &Abort<'_>,
        ) -> Result<Vec<f32>, String> {
            let text = CString::new(text).map_err(|_| "文本含 NUL 字节".to_string())?;
            let extra = CString::new(EXTRA).unwrap();
            // SAFETY: 全零位模式合法；随后逐字段赋值。
            let mut config: sys::SherpaOnnxGenerationConfig = unsafe { mem::zeroed() };
            config.speed = SPEED;
            config.num_steps = NUM_STEPS;
            config.extra = extra.as_ptr();
            if !reference.is_empty() {
                config.reference_audio = reference.as_ptr();
                config.reference_audio_len = reference.len() as i32;
                config.reference_sample_rate = ref_rate as i32;
            }
            // SAFETY: handle 有效；text/config 指向的缓冲在本调用期间存活；
            // 回调 arg 指向本栈帧的 abort（生成调用期间存活）；结果对象用后释放。
            unsafe {
                let audio = sys::SherpaOnnxOfflineTtsGenerateWithConfig(
                    self.handle,
                    text.as_ptr(),
                    &config,
                    Some(abort_on_stop),
                    (abort as *const Abort<'_>).cast::<c_void>().cast_mut(),
                );
                if audio.is_null() {
                    return Err("PocketTTS 生成失败".to_string());
                }
                let n = (*audio).n;
                if n <= 0 {
                    sys::SherpaOnnxDestroyOfflineTtsGeneratedAudio(audio);
                    return Err("PocketTTS 返回空音频".to_string());
                }
                let samples = std::slice::from_raw_parts((*audio).samples, n as usize).to_vec();
                sys::SherpaOnnxDestroyOfflineTtsGeneratedAudio(audio);
                Ok(samples)
            }
        }
    }
}

// ===========================================================================
// Stub：返回 0.1 s 440 Hz 正弦（2400 samples @ 24 kHz →
// 重采样后 4800 samples @ 48 kHz，恰 5 契约帧），覆盖重采样与输出路径。
// 链接 sherpa 时仅经上方 load 注入测试路径。
// ===========================================================================
mod stub {
    use super::Synthesizer;
    use crate::resample::TTS_NATIVE_SAMPLE_RATE;
    use crate::state::Abort;
    use std::path::Path;

    /// stub 音频长度：0.1 s。
    const STUB_SAMPLES: usize = (TTS_NATIVE_SAMPLE_RATE as usize) / 10;
    const STUB_FREQ_HZ: f64 = 440.0;
    const STUB_AMPLITUDE: f32 = 0.5;

    pub fn load(
        _model_dir: &Path,
        _threads: u32,
        _provider: &str,
    ) -> Result<Box<dyn Synthesizer>, String> {
        Ok(Box::new(StubTts))
    }

    struct StubTts;

    impl Synthesizer for StubTts {
        fn sample_rate(&self) -> u32 {
            TTS_NATIVE_SAMPLE_RATE
        }

        fn synthesize(
            &self,
            _text: &str,
            _reference: &[f32],
            _ref_rate: u32,
            _abort: &Abort<'_>,
        ) -> Result<Vec<f32>, String> {
            Ok((0..STUB_SAMPLES)
                .map(|i| {
                    let t = i as f64 / TTS_NATIVE_SAMPLE_RATE as f64;
                    STUB_AMPLITUDE * (2.0 * std::f64::consts::PI * STUB_FREQ_HZ * t).sin() as f32
                })
                .collect())
        }
    }
}