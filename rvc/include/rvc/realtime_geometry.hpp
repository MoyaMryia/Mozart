#pragma once

// realtime_geometry.hpp — 实时几何与参数契约（镜像 tools/model/realtime_chain_params.json）
// ============================================================================
// 单一来源公式见 docs/SPEC/SPEC-03-rvc.md「实时几何与资产参数」节与
// tools/model/realtime_chain_params.py。本文件与 Python 侧由双镜像单测钉住。

#include <cstddef>
#include <cstdint>
#include <stdexcept>

namespace rvc {

inline constexpr uint32_t kRealtimeInputSampleRate = 16000;
inline constexpr uint32_t kRealtimeContractOutputSampleRate = 48000;
inline constexpr size_t kRealtimeFeatureHop = 160;
inline constexpr size_t kRealtimeFeatureFrameRate = 100;
inline constexpr size_t kRealtimeHubertHop = 320;
inline constexpr size_t kRealtimeHubertReceptive = 400;
inline constexpr size_t kRealtimeRmvpeMelBands = 128;
inline constexpr size_t kRealtimeRmvpeBlockFrames = 32;

inline constexpr size_t kRealtimeWindowSamples = 3840;
inline constexpr size_t kRealtimePastContextSamples = 40000;
inline constexpr size_t kRealtimeCrossfadeMaxOut48k = 2400;
inline constexpr size_t kRealtimeSolaSearchMaxOut48k = 480;

inline constexpr uint32_t kRealtimeDefaultCrossfadeMs = 50;
inline constexpr uint32_t kRealtimeDefaultSolaSearchMs = 10;
inline constexpr uint32_t kRealtimeDefaultStartupDelayMs = 400;
inline constexpr size_t kRealtimeStartupBufferBlocks = 1;
inline constexpr bool kRealtimeDefaultSkipSilence = true;

inline constexpr bool is_supported_realtime_sample_rate(uint32_t sr) {
    return sr == 32000 || sr == 40000 || sr == 48000;
}

struct RealtimeGeometry {
    uint32_t input_sample_rate;
    uint32_t output_sample_rate;
    size_t feature_hop;
    size_t feature_frame_rate;
    size_t window_samples;
    size_t past_context_samples;
    size_t crossfade_max_out;
    size_t sola_search_max_out;
    size_t overlap_out;
    size_t margin_16k;
    size_t front_frames;
    size_t skip_head;
    size_t return_frames;
    size_t decoder_frames;
    size_t hubert_samples;
    size_t hubert_frames;
    size_t model_frame_out;
    size_t output_frame_samples;
    size_t decoder_out_samples;
    size_t output_hop;
    size_t flow_head;
    size_t latent_frames;
    size_t decoder_head;
    size_t rmvpe_frames;
};

inline RealtimeGeometry derive_realtime_geometry(
    uint32_t output_sample_rate = kRealtimeContractOutputSampleRate,
    size_t window_samples = kRealtimeWindowSamples,
    size_t past_context_samples = kRealtimePastContextSamples
) {
    if (!is_supported_realtime_sample_rate(output_sample_rate)) {
        throw std::invalid_argument("unsupported realtime output sample rate");
    }
    if (window_samples == 0 || window_samples % kRealtimeFeatureHop != 0) {
        throw std::invalid_argument("window_samples must be positive and divisible by 160");
    }
    if (past_context_samples == 0 || past_context_samples % kRealtimeFeatureHop != 0) {
        throw std::invalid_argument("past_context_samples must be positive and divisible by 160");
    }

    constexpr size_t overlap_ref = kRealtimeCrossfadeMaxOut48k + kRealtimeSolaSearchMaxOut48k;
    const size_t margin_16k =
        (overlap_ref * kRealtimeInputSampleRate) / kRealtimeContractOutputSampleRate;
    if ((past_context_samples + window_samples + margin_16k) % kRealtimeFeatureHop != 0) {
        throw std::invalid_argument("P + W + margin_16k must be divisible by 160");
    }

    const size_t crossfade_max_out =
        (kRealtimeCrossfadeMaxOut48k * output_sample_rate) / kRealtimeContractOutputSampleRate;
    const size_t sola_search_max_out =
        (kRealtimeSolaSearchMaxOut48k * output_sample_rate) / kRealtimeContractOutputSampleRate;
    const size_t overlap_out = crossfade_max_out + sola_search_max_out;

    const size_t front_frames =
        (past_context_samples + window_samples + margin_16k) / kRealtimeFeatureHop;
    const size_t skip_head = past_context_samples / kRealtimeFeatureHop;
    const size_t return_frames = (window_samples + margin_16k) / kRealtimeFeatureHop;
    const size_t decoder_frames = return_frames;

    const size_t hubert_samples = front_frames * kRealtimeFeatureHop;
    const size_t hubert_frames =
        (hubert_samples - kRealtimeHubertReceptive) / kRealtimeHubertHop + 1;
    const size_t model_frame_out = output_sample_rate / kRealtimeFeatureFrameRate;
    const size_t output_frame_samples = output_sample_rate / 50;
    const size_t decoder_out_samples = return_frames * model_frame_out;
    const size_t output_hop =
        (window_samples * output_sample_rate) / kRealtimeInputSampleRate;

    const size_t flow_head = skip_head > 24 ? skip_head - 24 : 0;
    const size_t latent_frames = front_frames - flow_head;
    const size_t decoder_head = skip_head - flow_head;

    const size_t f0_samples = window_samples + 800;
    const size_t rmvpe_block = kRealtimeRmvpeBlockFrames * kRealtimeFeatureHop;
    const size_t rmvpe_audio =
        rmvpe_block * ((f0_samples - 1) / rmvpe_block + 1) - kRealtimeFeatureHop;
    const size_t rmvpe_mel_frames = rmvpe_audio / kRealtimeFeatureHop + 1;
    const size_t rmvpe_frames =
        ((rmvpe_mel_frames + kRealtimeRmvpeBlockFrames - 1) / kRealtimeRmvpeBlockFrames)
        * kRealtimeRmvpeBlockFrames;

    return RealtimeGeometry{
        kRealtimeInputSampleRate,
        output_sample_rate,
        kRealtimeFeatureHop,
        kRealtimeFeatureFrameRate,
        window_samples,
        past_context_samples,
        crossfade_max_out,
        sola_search_max_out,
        overlap_out,
        margin_16k,
        front_frames,
        skip_head,
        return_frames,
        decoder_frames,
        hubert_samples,
        hubert_frames,
        model_frame_out,
        output_frame_samples,
        decoder_out_samples,
        output_hop,
        flow_head,
        latent_frames,
        decoder_head,
        rmvpe_frames,
    };
}

}  // namespace rvc
