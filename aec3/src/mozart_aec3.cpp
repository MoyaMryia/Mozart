#include "mozart_aec3.h"

#include <algorithm>
#include <cstring>
#include <new>

#include "modules/audio_processing/include/audio_processing.h"

namespace {

constexpr size_t kMaxChunkSamples = 480; // 10 ms @ 48 kHz
constexpr int kMaxStreamDelayMs = 500;

bool supported_rate(int sample_rate_hz) {
    return sample_rate_hz == 16000 || sample_rate_hz == 32000 || sample_rate_hz == 48000;
}

}  // namespace

struct mozart_aec3 {
    int sample_rate_hz;
    size_t chunk_samples;
    webrtc::StreamConfig stream_config;
    rtc::scoped_refptr<webrtc::AudioProcessing> apm;
};

mozart_aec3_t* mozart_aec3_create(int sample_rate_hz) {
    if (!supported_rate(sample_rate_hz)) {
        return nullptr;
    }
    rtc::scoped_refptr<webrtc::AudioProcessing> apm(
        webrtc::AudioProcessingBuilder().Create());
    if (!apm) {
        return nullptr;
    }
    webrtc::AudioProcessing::Config cfg;
    cfg.echo_canceller.enabled = true;
    cfg.echo_canceller.mobile_mode = false;
    cfg.high_pass_filter.enabled = true;
    apm->ApplyConfig(cfg);

    webrtc::StreamConfig sc(sample_rate_hz, 1);
    webrtc::ProcessingConfig pc;
    pc.input_stream() = sc;
    pc.output_stream() = sc;
    pc.reverse_input_stream() = sc;
    pc.reverse_output_stream() = sc;
    if (apm->Initialize(pc) != webrtc::AudioProcessing::kNoError) {
        return nullptr;
    }

    auto* handle = new (std::nothrow) mozart_aec3{
        sample_rate_hz,
        static_cast<size_t>(sample_rate_hz / 100),
        sc,
        apm,
    };
    return handle;
}

void mozart_aec3_destroy(mozart_aec3_t* handle) {
    delete handle;
}

int mozart_aec3_reset(mozart_aec3_t* handle) {
    if (!handle || !handle->apm) {
        return -1;
    }
    return handle->apm->Initialize() == webrtc::AudioProcessing::kNoError ? 0 : -1;
}

int mozart_aec3_process_reverse(mozart_aec3_t* handle, const float* pcm, size_t samples) {
    if (!handle || !handle->apm || !pcm || samples == 0 ||
        (samples % handle->chunk_samples) != 0) {
        return -1;
    }
    float scratch[kMaxChunkSamples];
    for (size_t off = 0; off < samples; off += handle->chunk_samples) {
        const float* in_ptr[1] = {pcm + off};
        float* out_ptr[1] = {scratch};
        if (handle->apm->ProcessReverseStream(
                in_ptr, handle->stream_config, handle->stream_config, out_ptr) !=
            webrtc::AudioProcessing::kNoError) {
            return -1;
        }
    }
    return 0;
}

int mozart_aec3_process_capture(
    mozart_aec3_t* handle,
    float* pcm,
    size_t samples,
    int stream_delay_ms) {
    if (!handle || !handle->apm || !pcm || samples == 0 ||
        (samples % handle->chunk_samples) != 0) {
        return -1;
    }
    const int delay = std::clamp(stream_delay_ms, 0, kMaxStreamDelayMs);
    float scratch[kMaxChunkSamples];
    for (size_t off = 0; off < samples; off += handle->chunk_samples) {
        std::memcpy(scratch, pcm + off, handle->chunk_samples * sizeof(float));
        handle->apm->set_stream_delay_ms(delay);
        const float* in_ptr[1] = {scratch};
        float* out_ptr[1] = {pcm + off};
        if (handle->apm->ProcessStream(
                in_ptr, handle->stream_config, handle->stream_config, out_ptr) !=
            webrtc::AudioProcessing::kNoError) {
            return -1;
        }
    }
    return 0;
}

int mozart_aec3_get_stats(const mozart_aec3_t* handle, mozart_aec3_stats_t* out) {
    if (!handle || !handle->apm || !out) {
        return -1;
    }
    std::memset(out, 0, sizeof(*out));
    const auto stats = handle->apm->GetStatistics();
    if (stats.delay_ms.has_value()) {
        out->has_delay = 1;
        out->delay_ms = *stats.delay_ms;
    }
    if (stats.echo_return_loss_enhancement.has_value()) {
        out->has_erle = 1;
        out->erle_db = *stats.echo_return_loss_enhancement;
    }
    if (stats.echo_return_loss.has_value()) {
        out->has_erl = 1;
        out->erl_db = *stats.echo_return_loss;
    }
    return 0;
}
