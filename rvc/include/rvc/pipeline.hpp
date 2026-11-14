#pragma once

// pipeline.hpp — RVC 推理管线门面
// 搬运自 Mozart-Old/rvc-backend/include/rvc/pipeline.hpp，改动：
//   - 裁掉 RVCPipelineFactory（旧后端启动时自动挑首个可用模型；新进程由
//     LoadModel 控制命令显式加载）；
//   - RvcParameters 裁掉 filter_radius（死参数，SPEC-03 v1 不暴露）。

#include <vector>
#include <memory>
#include <atomic>
#include <cstdint>
#include <filesystem>
#include <mutex>
#include <optional>
#include <string>
#include <map>

#include "rvc/model_loader.hpp"
#include "rvc/feature_extractor.hpp"
#include "rvc/inferencer.hpp"
#include "rvc/cancellation.hpp"

namespace rvc {

struct RvcMockConfig {
    bool generator = false;
    bool hubert = false;
    bool rmvpe = false;
};

struct RvcParameters {
    std::string f0_method = "rmvpe";
    int pitch_shift = 0;
    double index_rate = 0.0f;
    int index_nprobe = 1;
    double rms_mix_rate = 1.0f;
    double protect = 0.33f;
};

const char* invalid_rvc_parameter(const RvcParameters& parameters);

class RVCPipelineBase {
public:
    virtual ~RVCPipelineBase() = default;

    virtual std::vector<float> process(const std::vector<float>& audio) = 0;
    virtual std::vector<float> process_cancellable(
        const std::vector<float>& audio, const std::atomic<bool>& cancelled
    ) {
        check_cancelled(&cancelled);
        auto output = process(audio);
        check_cancelled(&cancelled);
        return output;
    }

    virtual std::vector<float> process_realtime(
        const std::vector<float>& audio, const RvcRealtimeRequest& request
    ) {
        auto output = process(audio);
        if (output.size() > request.output_samples) {
            output.erase(
                output.begin(),
                output.end() - static_cast<std::ptrdiff_t>(request.output_samples)
            );
        }
        return output;
    }
    virtual void reset_realtime() {}

    // 文件批处理进度 seam（SPEC-03 blocks）：已完成的 Generator 窗数，
    // 整段一次过计 1。真实管线委托 RVCInferencer（infer_window 计数，单一来源）；
    // mock 管线按整段调用计数。无窗概念的管线恒 0。
    virtual uint64_t offline_window_count() const { return 0; }
    virtual void reset_offline_window_count() {}

    virtual bool switch_model(const std::string&) { return false; }
    virtual std::string current_model_id() const { return ""; }
    virtual bool is_mock() const { return true; }
    virtual std::map<std::string, std::string> model_info() const { return {}; }
    virtual RvcParameters parameters() const { return {}; }
    virtual bool set_parameters(const RvcParameters&) { return false; }
    virtual bool supports_quality_streaming() const { return false; }
    virtual bool supports_realtime_streaming() const { return false; }
};

class MockRVCPipeline : public RVCPipelineBase {
public:
    MockRVCPipeline(
        uint32_t input_sample_rate = 16000,
        uint32_t output_sample_rate = 48000
    );

    std::vector<float> process(const std::vector<float>& audio) override;
    std::vector<float> process_cancellable(
        const std::vector<float>& audio, const std::atomic<bool>& cancelled
    ) override;
    uint64_t offline_window_count() const override;
    void reset_offline_window_count() override;

private:
    uint32_t input_sample_rate_;
    uint32_t output_sample_rate_;
    std::atomic<uint64_t> window_count_{0};

    std::vector<float> upsample_linear(const std::vector<float>& audio, float ratio);
};

class RealRVCPipeline : public RVCPipelineBase {
public:
    RealRVCPipeline(
        std::shared_ptr<ModelManager> model_manager,
        const std::filesystem::path& hubert_path,
        const std::optional<std::filesystem::path>& rmvpe_path,
        uint32_t input_sample_rate = 16000,
        uint32_t output_sample_rate = 48000,
        const std::string& device = "cuda",
        bool half = false,
        RvcMockConfig mock = {},
        RvcParameters parameters = {},
        const std::optional<std::filesystem::path>& realtime_hubert_path = std::nullopt,
        const std::optional<std::filesystem::path>& realtime_rmvpe_path = std::nullopt
    );

    std::vector<float> process(const std::vector<float>& audio) override;
    std::vector<float> process_cancellable(
        const std::vector<float>& audio, const std::atomic<bool>& cancelled
    ) override;
    std::vector<float> process_realtime(
        const std::vector<float>& audio,
        const RvcRealtimeRequest& request
    ) override;
    void reset_realtime() override;
    uint64_t offline_window_count() const override;
    void reset_offline_window_count() override;
    bool switch_model(const std::string& model_id) override;
    std::string current_model_id() const override;
    bool is_mock() const override { return false; }
    std::map<std::string, std::string> model_info() const override;
    RvcParameters parameters() const override;
    bool set_parameters(const RvcParameters& parameters) override;
    bool supports_quality_streaming() const override;
    bool supports_realtime_streaming() const override;

private:
    std::shared_ptr<ModelManager> model_manager_;
    std::shared_ptr<FeatureExtractor> feature_extractor_;
    std::shared_ptr<RVCInferencer> inferencer_;

    uint32_t input_sample_rate_;
    uint32_t output_sample_rate_;
    std::string device_;
    bool half_;
    RvcMockConfig mock_;
    RvcParameters parameters_;

    // 保护 inferencer_ / parameters_ 的换入换出：实时推理线程会同时
    // 调用 process_realtime()，模型或参数切换必须原子替换 shared_ptr。
    mutable std::mutex mutex_;

    void rebuild_inferencer_locked();
    std::shared_ptr<RVCInferencer> current_inferencer() const;
};

} // namespace rvc
