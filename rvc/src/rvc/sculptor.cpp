// 意图信号与 sculptor 侧车参数实现，镜像 rvc-reference/prosody_sculptor_l2/features.py。
// SPEC-03 步骤 6。
#include "rvc/sculptor.hpp"

#include <nlohmann/json.hpp>
#include <spdlog/spdlog.h>

#include <algorithm>
#include <cmath>
#include <fstream>
#include <stdexcept>

namespace rvc {

namespace {

// FPS = 100：EMA 时间常数对应的逐帧系数（features.py -expm1(-1/(FPS*T))）。
// std::expm1 非 constexpr，运行期初始化一次。
const double kEmaEnergy = -std::expm1(-1.0 / (100.0 * 4.0));
const double kEmaBaseline = -std::expm1(-1.0 / (100.0 * 5.0));
const double kEmaIntent = -std::expm1(-1.0 / (100.0 * 0.08));
constexpr double kLog2 = 0.693147180559945309417;

float sigmoid(float x) {
    return 1.0f / (1.0f + std::exp(-x));
}

} // namespace

float causal_rms_frame(const float* audio_16k, size_t frame_end_offset) {
    // 窗 [end-639, end]，分母恒 640：历史不足部分按静音计（features.py 语义）。
    const size_t begin = frame_end_offset >= 639 ? frame_end_offset - 639 : 0;
    double sum = 0.0;
    for (size_t i = begin; i <= frame_end_offset; ++i) {
        sum += static_cast<double>(audio_16k[i]) * static_cast<double>(audio_16k[i]);
    }
    return static_cast<float>(std::sqrt(sum / 640.0));
}

std::array<float, 4> IntentSignalState::compute_frame(
    const float* audio_16k, size_t audio_size, size_t frame_end_offset, float f0_hz
) {
    if (audio_16k == nullptr || frame_end_offset >= audio_size) {
        throw std::invalid_argument("Intent frame clock exceeds available audio");
    }
    const double log_rms = std::log(std::max(causal_rms_frame(audio_16k, frame_end_offset), 1e-6f));

    if (!initialized_) {
        // 首帧初始化：mean 取本帧、variance 清零，更新步保持恒等（diff=0）。
        mean_ = static_cast<float>(log_rms);
        variance_ = 0.0f;
        initialized_ = true;
    }

    const float energy = static_cast<float>(log_rms);
    const float diff = energy - mean_;
    mean_ += static_cast<float>(kEmaEnergy) * diff;
    variance_ = static_cast<float>(1.0 - kEmaEnergy)
        * (variance_ + static_cast<float>(kEmaEnergy) * diff * diff);
    const float z_log_energy = std::clamp(
        (energy - mean_) / std::sqrt(variance_ + 1e-4f), -4.0f, 4.0f);

    float coarse_rel_f0 = 0.0f;
    if (f0_hz > 0.0f) {
        const float log_f0 = std::log(f0_hz);
        if (!baseline_set_) {
            baseline_ = log_f0;
            baseline_set_ = true;
        } else {
            baseline_ += static_cast<float>(kEmaBaseline) * (log_f0 - baseline_);
        }
        smoothed_ += static_cast<float>(kEmaIntent) * (
            (log_f0 - baseline_) * 12.0f / static_cast<float>(kLog2) - smoothed_);
        coarse_rel_f0 = std::tanh(smoothed_ / 6.0f);
    } else {
        // features.py：无声帧不写通道 1（保持 0），但平滑状态仍衰减。
        smoothed_ *= static_cast<float>(1.0 - kEmaIntent);
    }

    // delta_energy：本帧减 max(0, t-3) 帧；不足时夹紧到最近历史（首帧差分为 0）。
    const size_t back = std::min<size_t>(3, history_count_);
    const float previous = back > 0 ? history_at(back) : z_log_energy;
    const float delta_energy = std::clamp(z_log_energy - previous, -4.0f, 4.0f);
    const float pause_prob = sigmoid(
        (static_cast<float>(std::log(0.012)) - energy) / 0.4f);
    push_history(z_log_energy);
    return {z_log_energy, coarse_rel_f0, delta_energy, pause_prob};
}

void IntentSignalState::push_history(float z_log_energy) {
    energy_history_[2] = energy_history_[1];
    energy_history_[1] = energy_history_[0];
    energy_history_[0] = z_log_energy;
    history_count_ = std::min<size_t>(history_count_ + 1, 3);
}

float IntentSignalState::history_at(size_t back) const {
    if (back == 0 || back > history_count_) {
        throw std::invalid_argument("Intent history index out of range");
    }
    return energy_history_[back - 1];
}

void IntentSignalState::reset() {
    initialized_ = false;
    mean_ = 0.0f;
    variance_ = 0.0f;
    baseline_ = 0.0f;
    smoothed_ = 0.0f;
    baseline_set_ = false;
    energy_history_ = {0.0f, 0.0f, 0.0f};
    history_count_ = 0;
}

std::array<float, 4> SculptorParams::cond_vector() const {
    return {
        static_cast<float>(gender),
        static_cast<float>(std::log(target_f0 / 175.0f) / 0.35),
        target_f0_std / 6.0f,
        softness,
    };
}

std::optional<SculptorParams> SculptorParams::from_json(const std::filesystem::path& path) {
    std::ifstream f(path);
    if (!f) {
        spdlog::error("Cannot open sculptor params: {}", path.string());
        return std::nullopt;
    }
    try {
        nlohmann::json j;
        f >> j;
        SculptorParams params;
        params.gender = j.at("gender").get<int>();
        params.target_f0 = j.at("target_f0").get<float>();
        params.target_f0_std = j.at("target_f0_std").get<float>();
        params.softness = j.at("softness").get<float>();
        if (params.gender != 1 && params.gender != -1) {
            throw std::invalid_argument("sculptor gender must be +1 or -1");
        }
        if (!(params.target_f0 > 0.0f) || params.target_f0_std < 0.0f
            || params.softness < 0.0f || params.softness > 1.0f) {
            throw std::invalid_argument("sculptor target_f0/std/softness out of range");
        }
        return params;
    } catch (const std::exception& e) {
        spdlog::error("Failed to parse sculptor params {}: {}", path.string(), e.what());
        return std::nullopt;
    }
}

} // namespace rvc
