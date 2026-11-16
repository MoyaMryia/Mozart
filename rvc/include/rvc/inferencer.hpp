#pragma once

// inferencer.hpp — RVC 推理器（搬运自 Mozart-Old/rvc-backend/include/rvc/inferencer.hpp）
// 改动：裁掉 filter_radius——旧实现只保存、未参与计算，v1 不暴露死参数
//（SPEC-03，推迟实现见 FUTURE.md）。

#include <atomic>
#include <vector>
#include <memory>
#include <cstdint>
#include <cstddef>
#include <random>
#include "rvc/cancellation.hpp"

#include "rvc/model_loader.hpp"
#include "rvc/feature_extractor.hpp"

namespace rvc {

struct RvcRealtimeRequest {
    size_t block_samples_16k = 0;
    size_t skip_head_frames = 0;
    size_t return_frames = 0;
    size_t output_samples = 0;
};

// 文件分窗/单窗几何（纯算术，无引擎依赖，可单测）。
// 16k 量以样本计，tgt 量以模型采样率计。
// window_input 是 HuBERT 输入长度；win_16k 是主窗（生成器输出覆盖减两侧 pad），
// 两者之差含卷积边缘损耗余量，作为额外右上下文。
// 前置条件：generator_frames 为偶数（生成器 T = 2×HuBERT 帧数），由调用方校验。
struct OfflineWindowGeometry {
    size_t pad_16k;
    size_t win_16k;
    size_t window_input;
    size_t crossfade_in;
    size_t hop_in;
    size_t pad_tgt;
    size_t window_out;
    size_t crossfade_out;
};

OfflineWindowGeometry offline_window_geometry(
    size_t generator_frames, uint32_t model_sr);

class RVCInferencer {
public:
    RVCInferencer(
        std::shared_ptr<RVCModel> model,
        std::shared_ptr<FeatureExtractor> feature_extractor,
        uint32_t input_sample_rate = 16000,
        uint32_t output_sample_rate = 48000,
        std::string f0_method = "rmvpe",
        int pitch_shift = 0,
        float index_rate = 0.0f,
        float rms_mix_rate = 1.0f,
        float protect = 0.33f,
        int index_nprobe = 1
    );

    std::vector<float> infer(
        const std::vector<float>& audio, const std::atomic<bool>* cancelled = nullptr
    );
    std::vector<float> infer_realtime(
        const std::vector<float>& audio,
        const RvcRealtimeRequest& request
    );
    void reset_realtime();

    // 文件批处理进度（SPEC-03）：已完成的 Generator 窗数，infer_window 每窗 +1，
    // 整段一次过计 1。GetStatus 只读快照；批处理开始时清零。
    uint64_t generator_window_count() const {
        return generator_windows_.load(std::memory_order_relaxed);
    }
    void reset_generator_window_count() {
        generator_windows_.store(0, std::memory_order_relaxed);
    }

private:
    std::shared_ptr<RVCModel> model_;
    std::shared_ptr<FeatureExtractor> feature_extractor_;
    uint32_t input_sample_rate_;
    uint32_t output_sample_rate_;
    std::string f0_method_;
    int pitch_shift_;
    float index_rate_;
    float rms_mix_rate_;
    float protect_;
    int index_nprobe_;
    std::vector<int64_t> realtime_pitch_cache_;
    std::vector<float> realtime_pitchf_cache_;
    std::mt19937 realtime_random_{114514};
    std::atomic<uint64_t> generator_windows_{0};

    std::vector<float> resample(
        const std::vector<float>& audio,
        uint32_t src_sr, uint32_t dst_sr
    );

    std::vector<float> apply_index(const std::vector<float>& feats);

    std::vector<float> synthesize_window(
        const std::vector<float>& feats,
        const std::vector<float>& pitchf,
        const std::vector<int64_t>& pitch,
        const std::vector<float>& unindexed_feats,
        bool absolute_noise,
        int64_t absolute_frame_start,
        double absolute_cycles
    );

    std::vector<float> apply_rms_mix(
        const std::vector<float>& original_16k,
        std::vector<float> audio_out
    );

    std::vector<float> infer_window(
        const std::vector<float>& audio_16k,
        const std::vector<float>& pitchf,
        const std::vector<int64_t>& pitch,
        const std::atomic<bool>* cancelled = nullptr,
        bool absolute_noise = false,
        int64_t absolute_frame_start = 0,
        double absolute_cycles = 0.0
    );
};

} // namespace rvc
