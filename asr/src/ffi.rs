//! sherpa-onnx C API 声明与识别器抽象（SPEC-05：sherpa-onnx 以库形式链入）。
//!
//! build.rs 在设置 SHERPA_ONNX_LIB_DIR 时置 `has_sherpa` 并链接 sherpa-onnx-c-api；
//! 此时编译 `real` 模块（真实 FFI）。未设置时编译 `stub` 模块：在线识别返回空文本、
//! endpoint 恒 false，离线复核返回空文本，供本机无库构建与测试。
//!
//! 结构体布局逐字段对应 sherpa-onnx v1.13.6 `sherpa-onnx/c-api/c-api.h`；
//! 配置结构体先零初始化再填所需字段，未用字段保持零值（C API 约定）。
//! 句柄与结果对象的释放经 Drop 完成（对应 C API 的 Destroy 系列）。

use std::path::Path;

/// 流式识别的一条解码流。解码线程独占使用，跨线程移动需 Send。
pub trait Stream: Send {
    /// 喂入 16 kHz 单声道 f32 PCM（[-1, 1]）。
    fn accept_waveform(&mut self, samples: &[f32]);
    /// 推进解码直到当前数据耗尽（is_ready 循环）。
    fn decode(&mut self);
    /// 当前累计识别文本（全连接，含已切前缀）。
    fn get_result(&self) -> String;
    /// endpoint 后重置解码状态，保留识别器前端。
    fn reset(&mut self);
    /// 是否触发 endpoint。
    fn is_endpoint(&self) -> bool;
    /// 标记输入结束，吐出帧边界残留。
    fn input_finished(&mut self);
}

/// 流式在线识别器（transducer）。
pub trait Recognizer: Send {
    fn create_stream(&self) -> Box<dyn Stream>;
}

/// 离线整句识别器（SenseVoice 复核）。一次喂入整句 PCM，返回文本或错误原因。
pub trait OfflineRecognizer: Send {
    fn decode(&self, samples: &[f32]) -> Result<String, String>;
}

/// 测试注入口：MOZART_ASR_STUB=1 强制 stub（进程级集成测试不依赖模型文件）；
/// MOZART_ASR_STUB_TEXT 非空时同样走 stub 并注入文本。
fn stub_text_env() -> Option<String> {
    std::env::var("MOZART_ASR_STUB_TEXT").ok().filter(|t| !t.is_empty())
}

fn stub_forced() -> bool {
    std::env::var("MOZART_ASR_STUB").map(|v| v == "1").unwrap_or(false) || stub_text_env().is_some()
}

/// 加载在线识别器。model_dir 为空或加载失败返回 Err（进程启动路径据此退出）。
pub fn load_online(model_dir: &Path, threads: u32) -> Result<Box<dyn Recognizer>, String> {
    if stub_forced() {
        return stub::load_online(model_dir, threads);
    }
    imp::load_online(model_dir, threads)
}

/// 加载离线复核识别器。失败返回 Err；复核线程据此对每句报 failed，不影响在线识别。
pub fn load_offline(model_dir: &Path, threads: u32) -> Result<Box<dyn OfflineRecognizer>, String> {
    if stub_forced() {
        return stub::load_offline(model_dir, threads);
    }
    imp::load_offline(model_dir, threads)
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
    use std::os::raw::{c_char, c_int};

    // 不透明句柄。
    #[repr(C)]
    pub struct SherpaOnnxOnlineRecognizer {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineStream {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineRecognizer {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineStream {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct SherpaOnnxOnlineTransducerModelConfig {
        pub encoder: *const c_char,
        pub decoder: *const c_char,
        pub joiner: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineParaformerModelConfig {
        pub encoder: *const c_char,
        pub decoder: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineZipformer2CtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineNemoCtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineToneCtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineModelConfig {
        pub transducer: SherpaOnnxOnlineTransducerModelConfig,
        pub paraformer: SherpaOnnxOnlineParaformerModelConfig,
        pub zipformer2_ctc: SherpaOnnxOnlineZipformer2CtcModelConfig,
        pub tokens: *const c_char,
        pub num_threads: c_int,
        pub provider: *const c_char,
        pub debug: c_int,
        pub model_type: *const c_char,
        pub modeling_unit: *const c_char,
        pub bpe_vocab: *const c_char,
        pub tokens_buf: *const c_char,
        pub tokens_buf_size: c_int,
        pub nemo_ctc: SherpaOnnxOnlineNemoCtcModelConfig,
        pub t_one_ctc: SherpaOnnxOnlineToneCtcModelConfig,
    }
    #[repr(C)]
    pub struct SherpaOnnxFeatureConfig {
        pub sample_rate: c_int,
        pub feature_dim: c_int,
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineCtcFstDecoderConfig {
        pub graph: *const c_char,
        pub max_active: c_int,
    }
    #[repr(C)]
    pub struct SherpaOnnxHomophoneReplacerConfig {
        pub dict_dir: *const c_char,
        pub lexicon: *const c_char,
        pub rule_fsts: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineRecognizerConfig {
        pub feat_config: SherpaOnnxFeatureConfig,
        pub model_config: SherpaOnnxOnlineModelConfig,
        pub decoding_method: *const c_char,
        pub max_active_paths: c_int,
        pub enable_endpoint: c_int,
        pub rule1_min_trailing_silence: f32,
        pub rule2_min_trailing_silence: f32,
        pub rule3_min_utterance_length: f32,
        pub hotwords_file: *const c_char,
        pub hotwords_score: f32,
        pub ctc_fst_decoder_config: SherpaOnnxOnlineCtcFstDecoderConfig,
        pub rule_fsts: *const c_char,
        pub rule_fars: *const c_char,
        pub blank_penalty: f32,
        pub hotwords_buf: *const c_char,
        pub hotwords_buf_size: c_int,
        pub hr: SherpaOnnxHomophoneReplacerConfig,
    }
    #[repr(C)]
    pub struct SherpaOnnxOnlineRecognizerResult {
        pub text: *const c_char,
        pub tokens: *const c_char,
        pub tokens_arr: *const *const c_char,
        pub timestamps: *mut f32,
        pub count: c_int,
        pub json: *const c_char,
    }

    #[repr(C)]
    pub struct SherpaOnnxOfflineTransducerModelConfig {
        pub encoder: *const c_char,
        pub decoder: *const c_char,
        pub joiner: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineParaformerModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineNemoEncDecCtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineWhisperModelConfig {
        pub encoder: *const c_char,
        pub decoder: *const c_char,
        pub language: *const c_char,
        pub task: *const c_char,
        pub tail_paddings: c_int,
        pub enable_token_timestamps: c_int,
        pub enable_segment_timestamps: c_int,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineTdnnModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineLMConfig {
        pub model: *const c_char,
        pub scale: f32,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineSenseVoiceModelConfig {
        pub model: *const c_char,
        pub language: *const c_char,
        pub use_itn: c_int,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineMoonshineModelConfig {
        pub preprocessor: *const c_char,
        pub encoder: *const c_char,
        pub uncached_decoder: *const c_char,
        pub cached_decoder: *const c_char,
        pub merged_decoder: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineFireRedAsrModelConfig {
        pub encoder: *const c_char,
        pub decoder: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineDolphinModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineZipformerCtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineCanaryModelConfig {
        pub encoder: *const c_char,
        pub decoder: *const c_char,
        pub src_lang: *const c_char,
        pub tgt_lang: *const c_char,
        pub use_pnc: c_int,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineWenetCtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineOmnilingualAsrCtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineMedAsrCtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineFunASRNanoModelConfig {
        pub encoder_adaptor: *const c_char,
        pub llm: *const c_char,
        pub embedding: *const c_char,
        pub tokenizer: *const c_char,
        pub system_prompt: *const c_char,
        pub user_prompt: *const c_char,
        pub max_new_tokens: c_int,
        pub temperature: f32,
        pub top_p: f32,
        pub seed: c_int,
        pub language: *const c_char,
        pub itn: c_int,
        pub hotwords: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineFireRedAsrCtcModelConfig {
        pub model: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineQwen3ASRModelConfig {
        pub conv_frontend: *const c_char,
        pub encoder: *const c_char,
        pub decoder: *const c_char,
        pub tokenizer: *const c_char,
        pub max_total_len: c_int,
        pub max_new_tokens: c_int,
        pub temperature: f32,
        pub top_p: f32,
        pub seed: c_int,
        pub hotwords: *const c_char,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineCohereTranscribeModelConfig {
        pub encoder: *const c_char,
        pub decoder: *const c_char,
        pub language: *const c_char,
        pub use_punct: c_int,
        pub use_itn: c_int,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineModelConfig {
        pub transducer: SherpaOnnxOfflineTransducerModelConfig,
        pub paraformer: SherpaOnnxOfflineParaformerModelConfig,
        pub nemo_ctc: SherpaOnnxOfflineNemoEncDecCtcModelConfig,
        pub whisper: SherpaOnnxOfflineWhisperModelConfig,
        pub tdnn: SherpaOnnxOfflineTdnnModelConfig,
        pub tokens: *const c_char,
        pub num_threads: c_int,
        pub debug: c_int,
        pub provider: *const c_char,
        pub model_type: *const c_char,
        pub modeling_unit: *const c_char,
        pub bpe_vocab: *const c_char,
        pub telespeech_ctc: *const c_char,
        pub sense_voice: SherpaOnnxOfflineSenseVoiceModelConfig,
        pub moonshine: SherpaOnnxOfflineMoonshineModelConfig,
        pub fire_red_asr: SherpaOnnxOfflineFireRedAsrModelConfig,
        pub dolphin: SherpaOnnxOfflineDolphinModelConfig,
        pub zipformer_ctc: SherpaOnnxOfflineZipformerCtcModelConfig,
        pub canary: SherpaOnnxOfflineCanaryModelConfig,
        pub wenet_ctc: SherpaOnnxOfflineWenetCtcModelConfig,
        pub omnilingual: SherpaOnnxOfflineOmnilingualAsrCtcModelConfig,
        pub medasr: SherpaOnnxOfflineMedAsrCtcModelConfig,
        pub funasr_nano: SherpaOnnxOfflineFunASRNanoModelConfig,
        pub fire_red_asr_ctc: SherpaOnnxOfflineFireRedAsrCtcModelConfig,
        pub qwen3_asr: SherpaOnnxOfflineQwen3ASRModelConfig,
        pub cohere_transcribe: SherpaOnnxOfflineCohereTranscribeModelConfig,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineRecognizerConfig {
        pub feat_config: SherpaOnnxFeatureConfig,
        pub model_config: SherpaOnnxOfflineModelConfig,
        pub lm_config: SherpaOnnxOfflineLMConfig,
        pub decoding_method: *const c_char,
        pub max_active_paths: c_int,
        pub hotwords_file: *const c_char,
        pub hotwords_score: f32,
        pub rule_fsts: *const c_char,
        pub rule_fars: *const c_char,
        pub blank_penalty: f32,
        pub hr: SherpaOnnxHomophoneReplacerConfig,
    }
    #[repr(C)]
    pub struct SherpaOnnxOfflineRecognizerResult {
        pub text: *const c_char,
        pub timestamps: *mut f32,
        pub count: c_int,
        pub tokens: *const c_char,
        pub tokens_arr: *const *const c_char,
        pub json: *const c_char,
        pub lang: *const c_char,
        pub emotion: *const c_char,
        pub event: *const c_char,
        pub durations: *mut f32,
        pub ys_log_probs: *mut f32,
        pub segment_timestamps: *const f32,
        pub segment_durations: *const f32,
        pub segment_texts: *const c_char,
        pub segment_texts_arr: *const *const c_char,
        pub segment_count: c_int,
    }

    extern "C" {
        pub fn SherpaOnnxCreateOnlineRecognizer(
            config: *const SherpaOnnxOnlineRecognizerConfig,
        ) -> *const SherpaOnnxOnlineRecognizer;
        pub fn SherpaOnnxDestroyOnlineRecognizer(recognizer: *const SherpaOnnxOnlineRecognizer);
        pub fn SherpaOnnxCreateOnlineStream(
            recognizer: *const SherpaOnnxOnlineRecognizer,
        ) -> *const SherpaOnnxOnlineStream;
        pub fn SherpaOnnxDestroyOnlineStream(stream: *const SherpaOnnxOnlineStream);
        pub fn SherpaOnnxOnlineStreamAcceptWaveform(
            stream: *const SherpaOnnxOnlineStream,
            sample_rate: c_int,
            samples: *const f32,
            n: c_int,
        );
        pub fn SherpaOnnxIsOnlineStreamReady(
            recognizer: *const SherpaOnnxOnlineRecognizer,
            stream: *const SherpaOnnxOnlineStream,
        ) -> c_int;
        pub fn SherpaOnnxDecodeOnlineStream(
            recognizer: *const SherpaOnnxOnlineRecognizer,
            stream: *const SherpaOnnxOnlineStream,
        );
        pub fn SherpaOnnxGetOnlineStreamResult(
            recognizer: *const SherpaOnnxOnlineRecognizer,
            stream: *const SherpaOnnxOnlineStream,
        ) -> *const SherpaOnnxOnlineRecognizerResult;
        pub fn SherpaOnnxDestroyOnlineRecognizerResult(r: *const SherpaOnnxOnlineRecognizerResult);
        pub fn SherpaOnnxOnlineStreamReset(
            recognizer: *const SherpaOnnxOnlineRecognizer,
            stream: *const SherpaOnnxOnlineStream,
        );
        pub fn SherpaOnnxOnlineStreamIsEndpoint(
            recognizer: *const SherpaOnnxOnlineRecognizer,
            stream: *const SherpaOnnxOnlineStream,
        ) -> c_int;
        pub fn SherpaOnnxOnlineStreamInputFinished(stream: *const SherpaOnnxOnlineStream);

        pub fn SherpaOnnxCreateOfflineRecognizer(
            config: *const SherpaOnnxOfflineRecognizerConfig,
        ) -> *const SherpaOnnxOfflineRecognizer;
        pub fn SherpaOnnxDestroyOfflineRecognizer(recognizer: *const SherpaOnnxOfflineRecognizer);
        pub fn SherpaOnnxCreateOfflineStream(
            recognizer: *const SherpaOnnxOfflineRecognizer,
        ) -> *const SherpaOnnxOfflineStream;
        pub fn SherpaOnnxDestroyOfflineStream(stream: *const SherpaOnnxOfflineStream);
        pub fn SherpaOnnxAcceptWaveformOffline(
            stream: *const SherpaOnnxOfflineStream,
            sample_rate: c_int,
            samples: *const f32,
            n: c_int,
        );
        pub fn SherpaOnnxDecodeOfflineStream(
            recognizer: *const SherpaOnnxOfflineRecognizer,
            stream: *const SherpaOnnxOfflineStream,
        );
        pub fn SherpaOnnxGetOfflineStreamResult(
            stream: *const SherpaOnnxOfflineStream,
        ) -> *const SherpaOnnxOfflineRecognizerResult;
        pub fn SherpaOnnxDestroyOfflineRecognizerResult(r: *const SherpaOnnxOfflineRecognizerResult);
    }
}

#[cfg(has_sherpa)]
mod real {
    use super::{sys, OfflineRecognizer, Recognizer, Stream};
    use mozart_core::INPUT_SAMPLE_RATE;
    use std::ffi::{CStr, CString};
    use std::mem;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// 特征维度，沿用旧 zipformer-zh-14M（80 维 fbank）。
    const FEATURE_DIM: i32 = 80;
    /// endpoint 阈值沿用旧 stt_service.py 默认（rule1/rule2/rule3，单位秒）。
    const RULE1_MIN_TRAILING_SILENCE: f32 = 1.2;
    const RULE2_MIN_TRAILING_SILENCE: f32 = 0.6;
    const RULE3_MIN_UTTERANCE_LENGTH: f32 = 12.0;

    fn cstring_path(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("路径含 NUL 字节：{}", path.display()))
    }

    pub fn load_online(model_dir: &Path, threads: u32) -> Result<Box<dyn Recognizer>, String> {
        if model_dir.as_os_str().is_empty() {
            return Err("asr.model_dir 未配置，无法加载在线识别模型".to_string());
        }
        // 模型文件名沿用旧 zipformer-zh-14M 目录布局。
        let encoder = cstring_path(&model_dir.join("encoder-epoch-99-avg-1.onnx"))?;
        let decoder = cstring_path(&model_dir.join("decoder-epoch-99-avg-1.onnx"))?;
        let joiner = cstring_path(&model_dir.join("joiner-epoch-99-avg-1.onnx"))?;
        let tokens = cstring_path(&model_dir.join("tokens.txt"))?;
        let provider = CString::new("cpu").unwrap();
        let decoding = CString::new("greedy_search").unwrap();

        // SAFETY: 配置结构体由指针与整数构成，全零位模式合法；随后逐字段赋值。
        let mut config: sys::SherpaOnnxOnlineRecognizerConfig = unsafe { mem::zeroed() };
        config.feat_config.sample_rate = INPUT_SAMPLE_RATE as i32;
        config.feat_config.feature_dim = FEATURE_DIM;
        config.model_config.transducer.encoder = encoder.as_ptr();
        config.model_config.transducer.decoder = decoder.as_ptr();
        config.model_config.transducer.joiner = joiner.as_ptr();
        config.model_config.tokens = tokens.as_ptr();
        config.model_config.num_threads = threads as i32;
        config.model_config.provider = provider.as_ptr();
        config.decoding_method = decoding.as_ptr();
        config.enable_endpoint = 1;
        config.rule1_min_trailing_silence = RULE1_MIN_TRAILING_SILENCE;
        config.rule2_min_trailing_silence = RULE2_MIN_TRAILING_SILENCE;
        config.rule3_min_utterance_length = RULE3_MIN_UTTERANCE_LENGTH;

        // SAFETY: config 字段指向的 CString 在本调用期间存活；C API 在创建时拷贝配置。
        let handle = unsafe { sys::SherpaOnnxCreateOnlineRecognizer(&config) };
        if handle.is_null() {
            return Err(format!("加载在线识别模型失败：{}", model_dir.display()));
        }
        Ok(Box::new(OnlineRecognizer { handle }))
    }

    pub fn load_offline(model_dir: &Path, threads: u32) -> Result<Box<dyn OfflineRecognizer>, String> {
        if model_dir.as_os_str().is_empty() {
            return Err("asr.review_model_dir 未配置".to_string());
        }
        let model = cstring_path(&model_dir.join("model.int8.onnx"))?;
        let tokens = cstring_path(&model_dir.join("tokens.txt"))?;
        let provider = CString::new("cpu").unwrap();
        let decoding = CString::new("greedy_search").unwrap();
        let language = CString::new("zh").unwrap();

        // SAFETY: 同上，全零位模式合法。
        let mut config: sys::SherpaOnnxOfflineRecognizerConfig = unsafe { mem::zeroed() };
        config.feat_config.sample_rate = INPUT_SAMPLE_RATE as i32;
        config.feat_config.feature_dim = FEATURE_DIM;
        config.model_config.sense_voice.model = model.as_ptr();
        config.model_config.sense_voice.language = language.as_ptr();
        config.model_config.sense_voice.use_itn = 1;
        config.model_config.tokens = tokens.as_ptr();
        config.model_config.num_threads = threads as i32;
        config.model_config.provider = provider.as_ptr();
        config.decoding_method = decoding.as_ptr();

        // SAFETY: config 字段指向的 CString 在本调用期间存活。
        let handle = unsafe { sys::SherpaOnnxCreateOfflineRecognizer(&config) };
        if handle.is_null() {
            return Err(format!("加载复核模型失败：{}", model_dir.display()));
        }
        Ok(Box::new(OfflineRecognizerImpl { handle }))
    }

    struct OnlineRecognizer {
        handle: *const sys::SherpaOnnxOnlineRecognizer,
    }
    // SAFETY: 句柄由解码线程独占使用；sherpa-onnx 识别器可跨线程移动。
    unsafe impl Send for OnlineRecognizer {}

    impl Drop for OnlineRecognizer {
        fn drop(&mut self) {
            // SAFETY: handle 由 CreateOnlineRecognizer 返回且未释放。
            unsafe { sys::SherpaOnnxDestroyOnlineRecognizer(self.handle) };
        }
    }

    impl Recognizer for OnlineRecognizer {
        fn create_stream(&self) -> Box<dyn Stream> {
            // SAFETY: self.handle 有效。
            let stream = unsafe { sys::SherpaOnnxCreateOnlineStream(self.handle) };
            assert!(!stream.is_null(), "sherpa-onnx CreateOnlineStream 返回空");
            Box::new(OnlineStream {
                recognizer: self.handle,
                stream,
            })
        }
    }

    struct OnlineStream {
        recognizer: *const sys::SherpaOnnxOnlineRecognizer,
        stream: *const sys::SherpaOnnxOnlineStream,
    }
    // SAFETY: 流由解码线程独占使用。
    unsafe impl Send for OnlineStream {}

    impl Drop for OnlineStream {
        fn drop(&mut self) {
            // SAFETY: stream 由 CreateOnlineStream 返回且未释放。
            unsafe { sys::SherpaOnnxDestroyOnlineStream(self.stream) };
        }
    }

    impl Stream for OnlineStream {
        fn accept_waveform(&mut self, samples: &[f32]) {
            // SAFETY: stream 有效；samples 指向 len 个 f32。
            unsafe {
                sys::SherpaOnnxOnlineStreamAcceptWaveform(
                    self.stream,
                    INPUT_SAMPLE_RATE as i32,
                    samples.as_ptr(),
                    samples.len() as i32,
                );
            }
        }
        fn decode(&mut self) {
            // SAFETY: 句柄有效；is_ready 为真时 decode 一步。
            unsafe {
                while sys::SherpaOnnxIsOnlineStreamReady(self.recognizer, self.stream) != 0 {
                    sys::SherpaOnnxDecodeOnlineStream(self.recognizer, self.stream);
                }
            }
        }
        fn get_result(&self) -> String {
            // SAFETY: 句柄有效；结果对象用后释放。
            unsafe {
                let result = sys::SherpaOnnxGetOnlineStreamResult(self.recognizer, self.stream);
                let text = CStr::from_ptr((*result).text).to_string_lossy().into_owned();
                sys::SherpaOnnxDestroyOnlineRecognizerResult(result);
                text
            }
        }
        fn reset(&mut self) {
            // SAFETY: 句柄有效。
            unsafe { sys::SherpaOnnxOnlineStreamReset(self.recognizer, self.stream) };
        }
        fn is_endpoint(&self) -> bool {
            // SAFETY: 句柄有效。
            unsafe { sys::SherpaOnnxOnlineStreamIsEndpoint(self.recognizer, self.stream) != 0 }
        }
        fn input_finished(&mut self) {
            // SAFETY: 句柄有效。
            unsafe { sys::SherpaOnnxOnlineStreamInputFinished(self.stream) };
        }
    }

    struct OfflineRecognizerImpl {
        handle: *const sys::SherpaOnnxOfflineRecognizer,
    }
    // SAFETY: 句柄由复核线程独占使用。
    unsafe impl Send for OfflineRecognizerImpl {}

    impl Drop for OfflineRecognizerImpl {
        fn drop(&mut self) {
            // SAFETY: handle 由 CreateOfflineRecognizer 返回且未释放。
            unsafe { sys::SherpaOnnxDestroyOfflineRecognizer(self.handle) };
        }
    }

    impl OfflineRecognizer for OfflineRecognizerImpl {
        fn decode(&self, samples: &[f32]) -> Result<String, String> {
            // SAFETY: handle 有效；流与结果对象用后释放。
            unsafe {
                let stream = sys::SherpaOnnxCreateOfflineStream(self.handle);
                if stream.is_null() {
                    return Err("创建复核流失败".to_string());
                }
                sys::SherpaOnnxAcceptWaveformOffline(
                    stream,
                    INPUT_SAMPLE_RATE as i32,
                    samples.as_ptr(),
                    samples.len() as i32,
                );
                sys::SherpaOnnxDecodeOfflineStream(self.handle, stream);
                let result = sys::SherpaOnnxGetOfflineStreamResult(stream);
                let text = CStr::from_ptr((*result).text).to_string_lossy().into_owned();
                sys::SherpaOnnxDestroyOfflineRecognizerResult(result);
                sys::SherpaOnnxDestroyOfflineStream(stream);
                Ok(text)
            }
        }
    }
}

// ===========================================================================
// Stub：在线返回空文本、endpoint 恒 false；离线返回空文本。
// 测试注入口：MOZART_ASR_STUB_TEXT 非空时在线 stub 返回该文本（进程级集成测试
// 据此验证事件链路），默认空文本；链接 sherpa 时仅经上方 load_* 注入测试路径。
// ===========================================================================
mod stub {
    use super::{OfflineRecognizer, Recognizer, Stream};
    use std::path::Path;

    pub fn load_online(_model_dir: &Path, _threads: u32) -> Result<Box<dyn Recognizer>, String> {
        let text = std::env::var("MOZART_ASR_STUB_TEXT")
            .ok()
            .filter(|text| !text.is_empty());
        Ok(Box::new(StubRecognizer { text }))
    }

    pub fn load_offline(_model_dir: &Path, _threads: u32) -> Result<Box<dyn OfflineRecognizer>, String> {
        Ok(Box::new(StubOffline))
    }

    struct StubRecognizer {
        text: Option<String>,
    }
    impl Recognizer for StubRecognizer {
        fn create_stream(&self) -> Box<dyn Stream> {
            Box::new(StubStream {
                text: self.text.clone(),
            })
        }
    }

    struct StubStream {
        text: Option<String>,
    }
    impl Stream for StubStream {
        fn accept_waveform(&mut self, _samples: &[f32]) {}
        fn decode(&mut self) {}
        fn get_result(&self) -> String {
            self.text.clone().unwrap_or_default()
        }
        fn reset(&mut self) {}
        fn is_endpoint(&self) -> bool {
            false
        }
        fn input_finished(&mut self) {}
    }

    struct StubOffline;
    impl OfflineRecognizer for StubOffline {
        fn decode(&self, _samples: &[f32]) -> Result<String, String> {
            Ok(String::new())
        }
    }
}