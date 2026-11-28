// L2 因果意图塑形器（prosody sculptor）资产与意图信号移植。
// 权重训练与契约见 rvc-reference/prosody_sculptor_l2/（features.py 为唯一
// 特征定义来源）；本文件镜像其 causal_rms / EMA / cond 向量语义。
// SPEC-03 步骤 6。
#pragma once

#include <array>
#include <cstdint>
#include <filesystem>
#include <optional>
#include <string>
#include <vector>

namespace rvc {

// sculptor.json 侧车参数：目标韵律条件向量（与模型目录一同部署，只读）。
struct SculptorParams {
    int gender = 1;             // +1 目标女声（M2F），-1 目标男声（F2M）
    float target_f0 = 228.0f;   // 目标基频 Hz
    float target_f0_std = 2.6f; // 目标语调动态范围（半音）
    float softness = 0.7f;      // 声重/气感柔和度 0..1

    // features.py build_cond_vector：[gender, log(f0/175)/0.35, std/6, softness]
    std::array<float, 4> cond_vector() const;

    static std::optional<SculptorParams> from_json(const std::filesystem::path& path);
};

// 跨块因果意图信号状态：EMA 均值/方差、F0 基线与平滑轮廓。
// 推进节奏与 inferencer 音高缓存一致：每块只计算新落入的 fresh 帧，
// 断流重置时调用 reset() 后从下一个计算帧重新初始化。
class IntentSignalState {
public:
    void reset();

    // 计算一帧意图信号。audio_16k 为整窗样本（本帧 RMS 窗必须完整落在其中），
    // frame_end_offset 为本帧窗末样本下标（含，帧窗 = [end-639, end]），
    // f0_hz 为本帧 100 Hz 基频（<=0 视为无声帧）。
    // 返回 [z_log_energy, coarse_rel_f0, delta_energy, pause_prob]。
    std::array<float, 4> compute_frame(
        const float* audio_16k, size_t audio_size, size_t frame_end_offset, float f0_hz);

    // delta_energy 需要回看 3 帧的 z_log_energy：history_at(1) 为最近一帧。
    // 重置后不足的帧数自动夹紧到已存帧（首帧差分为 0），镜像 Python 的
    // out[np.maximum(0, t-3)] 语义。
    void push_history(float z_log_energy);
    float history_at(size_t back) const;

    bool initialized() const { return initialized_; }

private:
    bool initialized_ = false;
    float mean_ = 0.0f;
    float variance_ = 0.0f;
    float baseline_ = 0.0f;
    float smoothed_ = 0.0f;
    bool baseline_set_ = false;
    // 最近 3 帧 z_log_energy 环形历史；back=1 取最近一帧。
    std::array<float, 3> energy_history_ = {0.0f, 0.0f, 0.0f};
    size_t history_count_ = 0;
};

// features.py causal_rms：单帧 RMS，窗 [end-639, end]（含端点，640 样本）。
// 不足 640 样本时补零对齐语义由调用方保证（实时窗恒 640 样本历史）。
float causal_rms_frame(const float* audio_16k, size_t frame_end_offset);

} // namespace rvc
