#include <cmath>
#include <cstdint>
#include <cstdio>
#include <vector>

#include "mozart_aec3.h"

namespace {

uint32_t next_u32(uint32_t& state) {
    state ^= state << 13;
    state ^= state >> 17;
    state ^= state << 5;
    return state;
}

float next_sample(uint32_t& state) {
    return static_cast<float>(next_u32(state) >> 9) / 8388608.0f - 1.0f;
}

}  // namespace

int main() {
    if (mozart_aec3_create(44100) != nullptr || mozart_aec3_create(0) != nullptr ||
        mozart_aec3_create(-1) != nullptr) {
        std::fprintf(stderr, "invalid sample rate must return null\n");
        return 1;
    }

    mozart_aec3_t* aec = mozart_aec3_create(48000);
    if (!aec) {
        std::fprintf(stderr, "mozart_aec3_create(48000) failed\n");
        return 2;
    }

    float bad_buf[100] = {};
    if (mozart_aec3_process_reverse(aec, bad_buf, 100) == 0 ||
        mozart_aec3_process_capture(aec, bad_buf, 100, 0) == 0) {
        std::fprintf(stderr, "non-multiple sample count must fail\n");
        mozart_aec3_destroy(aec);
        return 3;
    }

    // 10 s synthetic far-end signal + 65 ms delayed room echo (2-tap FIR).
    constexpr int kRate = 48000;
    constexpr size_t kFrameSamples = 960;  // 20 ms
    constexpr size_t kTotalFrames = 500;   // 10 s
    constexpr size_t kTotalSamples = kTotalFrames * kFrameSamples;
    constexpr size_t kDelaySamples = kRate * 65 / 1000;  // 3120

    std::vector<float> far(kTotalSamples, 0.0f);
    std::vector<float> mic(kTotalSamples, 0.0f);
    uint32_t rng = 0x13579BDFu;
    float lp = 0.0f;
    for (size_t i = kRate; i < kTotalSamples; ++i) {
        // Modulated speech-like envelope + colored noise so delay estimator locks cleanly.
        float env = 0.35f * (0.6f + 0.4f * std::sin(2.0f * 3.1415926f * 2.5f * static_cast<float>(i) / kRate));
        lp = 0.85f * lp + 0.15f * next_sample(rng);
        far[i] = env * lp;
    }
    for (size_t i = kDelaySamples + 2; i < kTotalSamples; ++i) {
        mic[i] = 0.30f * far[i - kDelaySamples] + 0.12f * far[i - kDelaySamples - 2];
    }

    std::vector<float> out = mic;
    for (size_t f = 0; f < kTotalFrames; ++f) {
        const size_t off = f * kFrameSamples;
        if (mozart_aec3_process_reverse(aec, &far[off], kFrameSamples) != 0) {
            std::fprintf(stderr, "process_reverse failed at frame %zu\n", f);
            mozart_aec3_destroy(aec);
            return 4;
        }
        if (mozart_aec3_process_capture(aec, &out[off], kFrameSamples, 0) != 0) {
            std::fprintf(stderr, "process_capture failed at frame %zu\n", f);
            mozart_aec3_destroy(aec);
            return 5;
        }
    }

    double p_in = 0.0;
    double p_out = 0.0;
    for (size_t i = 3 * kRate; i < kTotalSamples; ++i) {
        p_in += static_cast<double>(mic[i]) * mic[i];
        p_out += static_cast<double>(out[i]) * out[i];
    }
    const double erle_db = 10.0 * std::log10((p_in + 1e-24) / (p_out + 1e-24));
    mozart_aec3_stats_t stats{};
    if (mozart_aec3_get_stats(aec, &stats) != 0) {
        std::fprintf(stderr, "get_stats failed\n");
        mozart_aec3_destroy(aec);
        return 6;
    }
    std::printf("synthetic_65ms_erle=%.2f dB has_delay=%d delay_ms=%d\n",
                erle_db, stats.has_delay, stats.delay_ms);
    if (erle_db <= 15.0) {
        std::fprintf(stderr, "ERLE %.2f dB did not exceed 15 dB\n", erle_db);
        mozart_aec3_destroy(aec);
        return 7;
    }

    if (mozart_aec3_reset(aec) != 0 ||
        mozart_aec3_process_reverse(aec, far.data(), kFrameSamples) != 0 ||
        mozart_aec3_process_capture(aec, out.data(), kFrameSamples, 0) != 0) {
        std::fprintf(stderr, "reset or post-reset process failed\n");
        mozart_aec3_destroy(aec);
        return 8;
    }

    mozart_aec3_destroy(aec);
    return 0;
}
