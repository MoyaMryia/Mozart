#include "rvc/inferencer.hpp"
#include "rvc/realtime_geometry.hpp"
// 搬运自 Mozart-Old/rvc-backend/src/rvc/inferencer.cpp，改动：裁掉 filter_radius
//（只保存未参与计算的死参数，SPEC-03 v1 不暴露）。
#include "rvc/profile.hpp"
#include "rvc/resample.hpp"
#include "mozart/frame.h"
#include <future>
#include <cstdlib>
#include <spdlog/spdlog.h>
#include <array>
#include <cmath>
#include <algorithm>
#include <limits>
#include <random>

namespace rvc {

namespace {

constexpr size_t kHop16k = 160;
// F0 帧率 100 Hz，每帧 0.01 s；F0(Hz)×此值 = 每帧周期数。
constexpr double kF0FrameSeconds = 0.01;
constexpr size_t kPad16k = 16000;
constexpr size_t kGeneratorFrames = 200;
// 内层 pad 的缩放基准：流式契约生成器 T=398（pad 1 s）。
// 主窗不按比例缩放，由 HuBERT 帧数反推，见 offline_window_geometry。
constexpr size_t kWindowReferenceFrames = 398;
// HuBERT 卷积栈：hop 320 样本、感受野 400 样本，对 L 样本输出
// floor((L-400)/320)+1 帧。feature_extractor.cpp 实时引擎校验常量
// 44800→139 帧可验证此公式。
constexpr size_t kHubertHop16k = 320;
constexpr size_t kHubertReceptive16k = 400;
// 短交叉淡化：不再固定 50% 重叠，避免把两套噪声/相位混满整段。
constexpr size_t kCrossfade16k = 3200;
constexpr float kPi = 3.14159265358979323846f;
// 文件按 320 样本契约帧攒段，解码向上取整最多补一帧，折算到 160 样本 hop 至多 2 帧。
// 整段单次路径容忍这点补帧余量，避免“帧数严格相等”被解码补帧绊倒而误退分窗。
constexpr size_t kFullLengthFrameSlack = MOZART_INPUT_SAMPLES / kHop16k;
constexpr size_t kRealtimePitchCacheFrames = 1024;

size_t reflect_position(int64_t position, size_t length) {
    // 调用方保证 length >= 2。
    const int64_t period = 2 * (static_cast<int64_t>(length) - 1);
    position %= period;
    if (position < 0) position += period;
    if (position >= static_cast<int64_t>(length)) position = period - position;
    return static_cast<size_t>(position);
}

std::vector<float> reflect_pad(const std::vector<float>& audio, size_t pad) {
    if (audio.empty() || pad == 0) return audio;
    if (audio.size() == 1) return std::vector<float>(1 + 2 * pad, audio.front());
    std::vector<float> out(audio.size() + 2 * pad);
    for (size_t i = 0; i < out.size(); ++i) {
        const int64_t position = static_cast<int64_t>(i) - static_cast<int64_t>(pad);
        out[i] = audio[reflect_position(position, audio.size())];
    }
    return out;
}

// 按有符号起点切片；越出源首尾的位置用反射补齐。用于给分窗取整段里的
// 真实相邻音频和对应音高，只有真正越过整段边界时才反射。
template <typename T>
std::vector<T> slice_with_edge_reflect(
    const std::vector<T>& source, int64_t start, size_t count
) {
    // 调用方保证 source 至少 2 个样本。
    std::vector<T> out(count);
    for (size_t i = 0; i < count; ++i) {
        out[i] = source[reflect_position(start + static_cast<int64_t>(i), source.size())];
    }
    return out;
}

uint64_t splitmix64(uint64_t value) {
    value += 0x9e3779b97f4a7c15ULL;
    value = (value ^ (value >> 30)) * 0xbf58476d1ce4e5b9ULL;
    value = (value ^ (value >> 27)) * 0x94d049bb133111ebULL;
    return value ^ (value >> 31);
}

uint64_t noise_key(uint64_t stream, uint64_t index, uint64_t channel) {
    return splitmix64(stream ^ splitmix64(index + 0x9e3779b97f4a7c15ULL * (channel + 1)));
}

float normal_from_key(uint64_t key) {
    const uint64_t first = splitmix64(key);
    const uint64_t second = splitmix64(key ^ 0xa0761d6478bd642fULL);
    const double u1 = static_cast<double>(first >> 11) * 0x1.0p-53;
    const double u2 = static_cast<double>(second >> 11) * 0x1.0p-53;
    const double radius = std::sqrt(-2.0 * std::log(std::max(u1, 1e-12)));
    return static_cast<float>(
        radius * std::cos(2.0 * 3.14159265358979323846 * u2));
}

// 绝对时间网格激励生成（文件分窗路径使用）：按绝对帧/样本索引键噪声，
// 相邻窗重叠区天然同键同噪声；source_phase 按累计 F0 周期接续。
struct AbsoluteExcitation {
    std::vector<float> latent_noise;
    std::vector<float> source_phase;
    std::vector<float> source_noise;
};

AbsoluteExcitation build_absolute_excitation(
    size_t latent_channels,
    size_t latent_frames,
    int64_t latent_frame_start,
    size_t source_frames,
    int64_t source_frame_start,
    size_t output_per_frame,
    size_t source_dim,
    double absolute_cycles
) {
    AbsoluteExcitation out;
    out.latent_noise.resize(latent_channels * latent_frames);
    for (size_t channel = 0; channel < latent_channels; ++channel) {
        for (size_t t = 0; t < latent_frames; ++t) {
            out.latent_noise[channel * latent_frames + t] = normal_from_key(
                noise_key(0x1ULL,
                    static_cast<uint64_t>(latent_frame_start + static_cast<int64_t>(t)),
                    channel));
        }
    }
    out.source_phase.resize(source_dim);
    std::mt19937 phase_random(114514);
    std::uniform_real_distribution<float> uniform(0.0f, 1.0f);
    for (float& value : out.source_phase) value = uniform(phase_random);
    if (!out.source_phase.empty()) out.source_phase.front() = 0.0f;
    for (size_t harmonic = 0; harmonic < source_dim; ++harmonic) {
        const double cycles = absolute_cycles * static_cast<double>(harmonic + 1);
        double phase = std::fmod(
            static_cast<double>(out.source_phase[harmonic]) + cycles, 1.0);
        if (phase < 0.0) phase += 1.0;
        out.source_phase[harmonic] = static_cast<float>(phase);
    }
    out.source_noise.resize(source_frames * output_per_frame * source_dim);
    const int64_t sample_start =
        source_frame_start * static_cast<int64_t>(output_per_frame);
    for (size_t sample = 0; sample < source_frames * output_per_frame; ++sample) {
        for (size_t channel = 0; channel < source_dim; ++channel) {
            out.source_noise[sample * source_dim + channel] = normal_from_key(
                noise_key(0x2ULL,
                    static_cast<uint64_t>(sample_start + static_cast<int64_t>(sample)),
                    channel));
        }
    }
    return out;
}

void peak_normalize(std::vector<float>& audio, float ceiling) {
    float peak = 0.0f;
    for (float sample : audio) peak = std::max(peak, std::fabs(sample));
    const float scale = peak / ceiling;
    if (scale > 1.0f) {
        for (float& sample : audio) sample /= scale;
    }
}

void iir_filter(const std::array<double, 6>& b,
                const std::array<double, 6>& a,
                std::vector<double>& samples,
                double initial_value) {
    // scipy.signal.lfilter_zi(b, a), applied with the same initial endpoint
    // value as scipy.signal.filtfilt's odd-padded input.
    constexpr std::array<double, 5> zi = {
        -0.96996106627015766, 3.879844239395132,
        -5.8197663213460897, 3.8798441895727569,
        -0.96996104135180816
    };
    std::array<double, 5> state{};
    for (size_t i = 0; i < state.size(); ++i) state[i] = zi[i] * initial_value;
    for (double& sample : samples) {
        const double x = sample;
        const double y = b[0] * x + state[0];
        for (size_t i = 1; i < state.size(); ++i) {
            state[i - 1] = b[i] * x - a[i] * y + state[i];
        }
        state.back() = b.back() * x - a.back() * y;
        sample = y;
    }
}

// scipy.signal.butter(5, 48, btype='high', fs=16000) + filtfilt
std::vector<float> highpass_48hz(const std::vector<float>& audio) {
    if (audio.size() < 16) return audio;
    // scipy.signal.butter(5, 48, btype="high", fs=16000).
    // Keep the reference's direct-form coefficients, but use double state;
    // the same recurrence in float32 diverges on ordinary multi-second audio.
    constexpr std::array<double, 6> b = {
        0.96996064518384473, -4.8498032259192234,
        9.6996064518384468, -9.6996064518384468,
        4.8498032259192234, -0.96996064518384473
    };
    constexpr std::array<double, 6> a = {
        1.0, -4.9390018191683636, 9.7578635267395413,
        -9.6395448494134595, 4.7615067973562102,
        -0.94082365320546057
    };
    constexpr size_t pad = 18;
    if (audio.size() <= pad) return audio;
    std::vector<double> ext(audio.size() + 2 * pad);
    for (size_t i = 0; i < pad; ++i) {
        const size_t src = std::min(i + 1, audio.size() - 1);
        ext[pad - 1 - i] = 2.0 * audio[0] - audio[src];
        const size_t rsrc = audio.size() > i + 1 ? audio.size() - 2 - i : 0;
        ext[pad + audio.size() + i] = 2.0 * audio.back() - audio[rsrc];
    }
    for (size_t i = 0; i < audio.size(); ++i) ext[pad + i] = audio[i];

    iir_filter(b, a, ext, ext.front());
    std::reverse(ext.begin(), ext.end());
    iir_filter(b, a, ext, ext.front());
    std::reverse(ext.begin(), ext.end());

    std::vector<float> result(audio.size());
    for (size_t i = 0; i < result.size(); ++i) result[i] = static_cast<float>(ext[pad + i]);
    return result;
}

std::vector<float> slice_or_pad(const std::vector<float>& src, size_t start, size_t count) {
    std::vector<float> out(count, 0.0f);
    if (start >= src.size()) return out;
    const size_t take = std::min(count, src.size() - start);
    std::copy_n(src.begin() + static_cast<std::ptrdiff_t>(start), take, out.begin());
    return out;
}

void limit_peak(std::vector<float>& audio, float ceiling) {
    float peak = 0.0f;
    for (float sample : audio) peak = std::max(peak, std::fabs(sample));
    if (peak > ceiling) {
        const float scale = ceiling / peak;
        for (float& sample : audio) sample *= scale;
    }
}

} // namespace

OfflineWindowGeometry offline_window_geometry(
    size_t generator_frames, uint32_t model_sr
) {
    // 内层 pad 按 T/398 缩放，封顶到外层 kPad16k（F0 上下文边界），防单窗
    // 音高偏移 (kPad16k-pad_16k) 下溢；T=200/398 不触发封顶。
    const size_t pad_16k = std::min(
        (kPad16k * generator_frames / kWindowReferenceFrames / kHubertHop16k)
            * kHubertHop16k,
        kPad16k);
    // 窗输入由"HuBERT 恰好产出 T 帧"反推：L ∈ [160T+80, 160T+400)，
    // 取下界向上对齐 320。主窗取生成器输出覆盖 T*160 减两侧 pad：输出时间轴
    // 比窗输入短（卷积边缘损耗），按窗输入对称裁剪会越过输出末端补零。
    // L 超出 win+2pad 的余量落在右上下文里。T=398 的 pad/淡化与旧常量一致。
    const size_t min_input =
        generator_frames * kHop16k + (kHubertReceptive16k - kHubertHop16k);
    const size_t window_input =
        ((min_input + kHubertHop16k - 1) / kHubertHop16k) * kHubertHop16k;
    const size_t win_16k = generator_frames * kHop16k - 2 * pad_16k;
    // 短淡化对齐到 2 个 HuBERT 帧，保证 hop 仍是 160 的整数倍。
    const size_t crossfade_in =
        (std::min(kCrossfade16k, win_16k / 4) / kHubertHop16k) * kHubertHop16k;
    OfflineWindowGeometry geometry;
    geometry.pad_16k = pad_16k;
    geometry.win_16k = win_16k;
    geometry.window_input = window_input;
    geometry.crossfade_in = crossfade_in;
    geometry.hop_in = win_16k - crossfade_in;
    geometry.pad_tgt = pad_16k * model_sr / 16000;
    geometry.window_out = win_16k * model_sr / 16000;
    geometry.crossfade_out = crossfade_in * model_sr / 16000;
    return geometry;
}

RVCInferencer::RVCInferencer(
    std::shared_ptr<RVCModel> model,
    std::shared_ptr<FeatureExtractor> feature_extractor,
    uint32_t input_sample_rate,
    uint32_t output_sample_rate,
    std::string f0_method,
    int pitch_shift,
    float index_rate,
    float rms_mix_rate,
    float protect,
    int index_nprobe
)
    : model_(model)
    , feature_extractor_(feature_extractor)
    , input_sample_rate_(input_sample_rate)
    , output_sample_rate_(output_sample_rate)
    , params_{std::move(f0_method), pitch_shift, index_rate, rms_mix_rate, protect, index_nprobe}
{
    reset_realtime();
}

void RVCInferencer::set_parameters(
    std::string f0_method,
    int pitch_shift,
    float index_rate,
    float rms_mix_rate,
    float protect,
    int index_nprobe
) {
    std::lock_guard<std::mutex> lock(params_mutex_);
    params_ = SessionParams{
        std::move(f0_method), pitch_shift, index_rate, rms_mix_rate, protect, index_nprobe
    };
}

RVCInferencer::SessionParams RVCInferencer::snapshot_params() const {
    std::lock_guard<std::mutex> lock(params_mutex_);
    return params_;
}

void RVCInferencer::reset_realtime() {
    realtime_pitch_cache_.assign(kRealtimePitchCacheFrames, 0);
    realtime_pitchf_cache_.assign(kRealtimePitchCacheFrames, 0.0f);
    realtime_intent_cache_.assign(
        kRealtimePitchCacheFrames, std::array<float, 4>{0.0f, 0.0f, 0.0f, 0.0f});
    intent_state_.reset();
    realtime_random_.seed(114514);
}

void RVCInferencer::advance_realtime_silence(size_t shift_frames) {
    if (shift_frames == 0) return;
    push_realtime_pitch_cache({}, {}, shift_frames);
    // 静音跳过路径无音频可算意图，仅同步推进缓存时间轴；EMA 状态冻结，
    // 恢复语音后的首块相对 Python 训练口径存在短暂偏差（已知限制，SPEC-03 步骤 6）。
    push_realtime_intent_cache(nullptr, 0, {}, shift_frames);
}

std::pair<std::vector<int64_t>, std::vector<float>> RVCInferencer::f0_to_pitch(
    const std::vector<float>& f0, int pitch_shift, size_t frames,
    bool interpolate_unvoiced
) {
    std::vector<float> raw(frames, 0.0f);
    const size_t f0_frames = std::max(size_t{1}, f0.size());
    for (size_t t = 0; t < frames; ++t) {
        if (!f0.empty()) raw[t] = f0[std::min(t, f0_frames - 1)];
    }
    if (interpolate_unvoiced) {
        size_t first_voiced = 0;
        while (first_voiced < raw.size() && raw[first_voiced] <= 0.0f) ++first_voiced;
        if (first_voiced < raw.size()) {
            for (size_t t = 0; t < first_voiced; ++t) raw[t] = raw[first_voiced];
            size_t left = first_voiced;
            for (size_t t = first_voiced + 1; t < raw.size(); ++t) {
                if (raw[t] <= 0.0f) continue;
                const size_t right = t;
                for (size_t gap = left + 1; gap < right; ++gap) {
                    const float amount = static_cast<float>(gap - left) / static_cast<float>(right - left);
                    raw[gap] = raw[left] * (1.0f - amount) + raw[right] * amount;
                }
                left = right;
            }
            for (size_t t = left + 1; t < raw.size(); ++t) raw[t] = raw[left];
        }
    }

    std::vector<int64_t> pitch(frames, 1);
    std::vector<float> pitchf(frames, 0.0f);
    const float shift = std::pow(2.0f, static_cast<float>(pitch_shift) / 12.0f);
    constexpr float f0_min = 50.0f;
    constexpr float f0_max = 1100.0f;
    const float mel_min = 1127.0f * std::log1p(f0_min / 700.0f);
    const float mel_max = 1127.0f * std::log1p(f0_max / 700.0f);
    for (size_t t = 0; t < frames; ++t) {
        pitchf[t] = raw[t] > 0.0f ? raw[t] * shift : 0.0f;
        if (pitchf[t] > 0.0f) {
            const float f0_mel = 1127.0f * std::log1p(pitchf[t] / 700.0f);
            const float coarse = (f0_mel - mel_min) * 254.0f / (mel_max - mel_min) + 1.0f;
            // NumPy's rint (used by the Golden path) rounds exact ties to even.
            pitch[t] = static_cast<int64_t>(std::clamp(std::nearbyint(coarse), 1.0f, 255.0f));
        }
    }
    return {std::move(pitch), std::move(pitchf)};
}

void RVCInferencer::apply_realtime_protect(
    std::vector<float>& resized_feats,
    const std::vector<float>& unindexed_tail,
    const std::vector<float>& pitchf,
    size_t skip_head_frames,
    size_t index_start_frames,
    size_t feature_frames,
    size_t emb_dim,
    float protect
) {
    if (protect >= 0.5f || unindexed_tail.empty() || feature_frames == 0 || emb_dim == 0) {
        return;
    }
    const size_t total_frames = std::min(resized_feats.size() / emb_dim, pitchf.size());
    for (size_t target = skip_head_frames; target < total_frames; ++target) {
        if (pitchf[target] > 0.0f) continue;
        const size_t source = std::min(target / 2, feature_frames - 1);
        if (source < index_start_frames) continue;
        const size_t tail_off = (source - index_start_frames) * emb_dim;
        if (tail_off + emb_dim > unindexed_tail.size()) continue;
        const size_t dst_off = target * emb_dim;
        for (size_t c = 0; c < emb_dim; ++c) {
            resized_feats[dst_off + c] = resized_feats[dst_off + c] * protect
                + unindexed_tail[tail_off + c] * (1.0f - protect);
        }
    }
}

void RVCInferencer::apply_realtime_rms_mix_and_limit(
    const float* input_16k,
    size_t input_samples,
    std::vector<float>& output,
    uint32_t output_sr,
    float rms_mix_rate,
    float peak_ceiling
) {
    if (rms_mix_rate < 1.0f && input_16k && input_samples > 0 && !output.empty() && output_sr >= 50) {
        const auto rms_curve = [](const float* samples, size_t count, size_t frame_size, size_t hop_size) {
            const size_t center_pad = frame_size / 2;
            std::vector<float> result;
            result.reserve(count / hop_size + 2);
            for (size_t center = 0; center <= count; center += hop_size) {
                const size_t start = center > center_pad ? center - center_pad : 0;
                const size_t end = std::min(count, center + center_pad);
                const size_t width = std::max<size_t>(1, end - start);
                float sum = 0.0f;
                for (size_t i = start; i < end; ++i) sum += samples[i] * samples[i];
                result.push_back(std::sqrt(sum / static_cast<float>(width)));
            }
            return result;
        };
        // 120 ms 平滑窗、30 ms 步长匹配 300 ms 实时块，吸收声码器群延迟；
        // 静音/底噪区间不反向放大，避免抬高停顿底噪。
        const auto in_rms = rms_curve(input_16k, input_samples, 1920, 480);
        const auto out_rms = rms_curve(output.data(), output.size(), output_sr * 3 / 25, output_sr * 3 / 100);
        const size_t n_grid = std::min(in_rms.size(), out_rms.size());
        if (n_grid > 0) {
            const float exponent = 1.0f - rms_mix_rate;
            std::vector<float> gains(n_grid, 1.0f);
            for (size_t g = 0; g < n_grid; ++g) {
                const float in_lvl = std::max(in_rms[g], 1e-6f);
                const float out_lvl = std::max(out_rms[g], 1e-6f);
                float gain = std::min(std::pow(in_lvl / out_lvl, exponent), 4.0f);
                if (in_rms[g] < 0.005f) {
                    gain = std::min(gain, 1.0f);
                }
                gains[g] = gain;
            }
            for (size_t i = 0; i < output.size(); ++i) {
                float gain = gains.front();
                if (n_grid > 1) {
                    const float pos = (static_cast<float>(i) + 0.5f)
                        * static_cast<float>(n_grid) / static_cast<float>(output.size()) - 0.5f;
                    const float clamped = std::clamp(pos, 0.0f, static_cast<float>(n_grid - 1));
                    const size_t left = static_cast<size_t>(clamped);
                    const size_t right = std::min(left + 1, n_grid - 1);
                    const float frac = clamped - static_cast<float>(left);
                    gain = gains[left] * (1.0f - frac) + gains[right] * frac;
                }
                output[i] *= gain;
            }
        }
    }
    limit_peak(output, peak_ceiling);
}

void RVCInferencer::push_realtime_pitch_cache(
    const std::vector<int64_t>& new_pitch,
    const std::vector<float>& new_pitchf,
    size_t shift
) {
    if (shift >= realtime_pitch_cache_.size()) {
        std::fill(realtime_pitch_cache_.begin(), realtime_pitch_cache_.end(), 0);
        std::fill(realtime_pitchf_cache_.begin(), realtime_pitchf_cache_.end(), 0.0f);
    } else {
        std::move(
            realtime_pitch_cache_.begin() + static_cast<std::ptrdiff_t>(shift),
            realtime_pitch_cache_.end(), realtime_pitch_cache_.begin()
        );
        std::fill(
            realtime_pitch_cache_.end() - static_cast<std::ptrdiff_t>(shift),
            realtime_pitch_cache_.end(), 0
        );
        std::move(
            realtime_pitchf_cache_.begin() + static_cast<std::ptrdiff_t>(shift),
            realtime_pitchf_cache_.end(), realtime_pitchf_cache_.begin()
        );
        std::fill(
            realtime_pitchf_cache_.end() - static_cast<std::ptrdiff_t>(shift),
            realtime_pitchf_cache_.end(), 0.0f
        );
    }

    if (new_pitch.size() < 5 || new_pitchf.size() != new_pitch.size()) {
        return;
    }
    // Upstream recomputes four overlap frames and drops three left-edge plus
    // one right-edge RMVPE frames before updating its persistent pitch cache.
    const size_t fresh_count = std::min(
        new_pitch.size() - 4, realtime_pitch_cache_.size()
    );
    const size_t fresh_source = new_pitch.size() - fresh_count - 1;
    const size_t fresh_target = realtime_pitch_cache_.size() - fresh_count;
    std::copy_n(
        new_pitch.begin() + static_cast<std::ptrdiff_t>(fresh_source),
        fresh_count,
        realtime_pitch_cache_.begin() + static_cast<std::ptrdiff_t>(fresh_target)
    );
    std::copy_n(
        new_pitchf.begin() + static_cast<std::ptrdiff_t>(fresh_source),
        fresh_count,
        realtime_pitchf_cache_.begin() + static_cast<std::ptrdiff_t>(fresh_target)
    );
}

void RVCInferencer::push_realtime_intent_cache(
    const float* audio_16k,
    size_t audio_size,
    const std::vector<float>& new_pitchf,
    size_t shift
) {
    // 滑动与清零语义镜像 push_realtime_pitch_cache。
    if (shift >= realtime_intent_cache_.size()) {
        std::fill(
            realtime_intent_cache_.begin(), realtime_intent_cache_.end(),
            std::array<float, 4>{0.0f, 0.0f, 0.0f, 0.0f});
        intent_state_.reset();
    } else {
        std::move(
            realtime_intent_cache_.begin() + static_cast<std::ptrdiff_t>(shift),
            realtime_intent_cache_.end(), realtime_intent_cache_.begin()
        );
        std::fill(
            realtime_intent_cache_.end() - static_cast<std::ptrdiff_t>(shift),
            realtime_intent_cache_.end(), std::array<float, 4>{0.0f, 0.0f, 0.0f, 0.0f});
    }
    if (new_pitchf.size() < 5) {
        return;
    }
    // fresh 对齐与音高缓存一致：丢弃右缘 1 帧 RMVPE。
    const size_t fresh_count = std::min(
        new_pitchf.size() - 4, realtime_intent_cache_.size()
    );
    const size_t fresh_source = new_pitchf.size() - fresh_count - 1;
    const size_t win_frames = audio_size / kRealtimeFeatureHop;
    if (fresh_count > win_frames) {
        throw std::invalid_argument("Realtime intent fresh frames exceed audio window");
    }
    for (size_t j = 0; j < fresh_count; ++j) {
        const size_t win_frame = win_frames - fresh_count + j;
        const size_t end_offset = win_frame * kRealtimeFeatureHop;
        realtime_intent_cache_[realtime_intent_cache_.size() - fresh_count + j]
            = intent_state_.compute_frame(
                audio_16k, audio_size, end_offset, new_pitchf[fresh_source + j]);
    }
}

void RVCInferencer::apply_realtime_sculptor(
    std::vector<float>& resized_feats,
    std::vector<float>& pitchf,
    std::vector<int64_t>& pitch,
    std::vector<float>& gain_db_frames
) {
    auto& engine = model_->realtime_sculptor_engine();
    const auto cond = model_->sculptor_params().cond_vector();
    const size_t frames = pitchf.size();
    const size_t emb_dim = model_->config().emb_channels;
    if (resized_feats.size() != frames * emb_dim
        || realtime_intent_cache_.size() < frames) {
        throw std::invalid_argument("Sculptor input geometry mismatch");
    }

    std::vector<float> intent;
    intent.reserve(frames * 4);
    for (size_t t = realtime_intent_cache_.size() - frames;
         t < realtime_intent_cache_.size(); ++t) {
        for (const float value : realtime_intent_cache_[t]) intent.push_back(value);
    }
    const std::vector<float> cond_float(cond.begin(), cond.end());
    const std::vector<OnnxInput> inputs{
        {"hubert_upsampled",
         {1, static_cast<int64_t>(frames), static_cast<int64_t>(emb_dim)},
         OnnxInput::Type::Float, resized_feats, {}},
        {"intent_signals", {1, static_cast<int64_t>(frames), 4},
         OnnxInput::Type::Float, std::move(intent), {}},
        {"cond", {1, 4}, OnnxInput::Type::Float, cond_float, {}},
    };
    // 单输出 [1, frames, 771]：通道 0/1/2 为 log_f0/voiced_logit/gain_db，
    // 3..771 为 delta_feats（tools/model/export_sculptor_onnx.py 拼接约定）。
    constexpr size_t kSculptorChannels = 771;
    auto out = engine.run(inputs, {"outputs"});
    if (out.size() != frames * kSculptorChannels) {
        throw std::runtime_error("Sculptor engine output shape is invalid");
    }

    std::vector<float> f0_l2(frames, 0.0f);
    gain_db_frames.assign(frames, 0.0f);
    for (size_t t = 0; t < frames; ++t) {
        const float* row = out.data() + t * kSculptorChannels;
        for (size_t c = 0; c < emb_dim; ++c) {
            resized_feats[t * emb_dim + c] += row[3 + c];
        }
        f0_l2[t] = row[1] > 0.0f
            ? std::clamp(std::exp(row[0]), 50.0f, 1100.0f) : 0.0f;
        gain_db_frames[t] = std::clamp(row[2], -4.0f, 4.0f);
    }
    auto [new_pitch, new_pitchf] = f0_to_pitch(
        f0_l2, /*pitch_shift=*/0, frames, /*interpolate_unvoiced=*/false);
    pitchf = std::move(new_pitchf);
    pitch = std::move(new_pitch);

    // 增益包络：高斯 sigma=6（60 ms）平滑，nearest 边缘夹紧。
    constexpr float kGainSigma = 6.0f;
    std::vector<float> smoothed(frames, 0.0f);
    const int radius = static_cast<int>(kGainSigma * 3.0f);
    float norm = 0.0f;
    std::vector<float> kernel(static_cast<size_t>(2 * radius + 1));
    for (int k = -radius; k <= radius; ++k) {
        const float w = std::exp(-0.5f * static_cast<float>(k * k) / (kGainSigma * kGainSigma));
        kernel[static_cast<size_t>(k + radius)] = w;
        norm += w;
    }
    for (size_t t = 0; t < frames; ++t) {
        float acc = 0.0f;
        for (int k = -radius; k <= radius; ++k) {
            const int64_t idx = std::clamp<int64_t>(
                static_cast<int64_t>(t) + k, 0, static_cast<int64_t>(frames) - 1);
            acc += kernel[static_cast<size_t>(k + radius)] * gain_db_frames[static_cast<size_t>(idx)];
        }
        smoothed[t] = acc / norm;
    }
    gain_db_frames = std::move(smoothed);
}

uint32_t RVCInferencer::sample_realtime_random() {
    return realtime_random_();
}

RVCInferencer::RealtimeStateSnapshot RVCInferencer::realtime_state_snapshot() const {
    std::mt19937 peek = realtime_random_;
    return RealtimeStateSnapshot{
        realtime_pitch_cache_,
        realtime_pitchf_cache_,
        static_cast<uint32_t>(peek()),
    };
}

std::vector<float> RVCInferencer::infer(
    const std::vector<float>& audio, const std::atomic<bool>* cancelled
) {
    check_cancelled(cancelled);
    if (!model_->loaded()) {
        throw std::runtime_error("RVC model not loaded");
    }
    if (!model_->has_generator()) {
        throw std::runtime_error("Offline Generator engine not loaded");
    }

    const SessionParams params = snapshot_params();
    const auto profile_start = profile::Clock::now();
    std::vector<float> audio_16k;
    if (input_sample_rate_ != 16000) {
        audio_16k = resample(audio, input_sample_rate_, 16000);
    } else {
        audio_16k = audio;
    }
    peak_normalize(audio_16k, 0.95f);
    audio_16k = highpass_48hz(audio_16k);
    const auto profile_preprocessed = profile::Clock::now();

    // 输入时长对应的采样数；帧率固定 100 Hz。
    const auto samples_for = [](size_t input_samples, uint32_t rate) {
        return input_samples * rate / 16000;
    };
    // 文件数据面契约：输出采样数由输入时长决定（48 kHz），各推理分支统一收口。
    const auto finalize_output = [&](std::vector<float> converted) {
        converted.resize(samples_for(audio_16k.size(), output_sample_rate_), 0.0f);
        return converted;
    };

    const uint32_t model_sr = model_->config().sample_rate;
    // 外层 pad（整段边界）固定 kPad16k，F0 按它提取；分窗内层几何另随 T 缩放。
    const size_t pad_out = kPad16k * model_sr / 16000;

    size_t generator_frames = kGeneratorFrames;
    const auto generator_shape = model_->generator_engine().input_shape("feats");
    const bool dynamic_generator = generator_shape.size() >= 2
        && generator_shape[1] <= 0;
    if (generator_shape.size() >= 2 && generator_shape[1] > 0) {
        generator_frames = static_cast<size_t>(generator_shape[1]);
    }

    const auto padded = reflect_pad(audio_16k, kPad16k);
    const auto profile_padded = profile::Clock::now();
    const size_t full_frames = padded.size() / kHop16k;
    check_cancelled(cancelled);
    const auto f0_full = feature_extractor_->extract_f0(padded, 16000, params.f0_method);
    check_cancelled(cancelled);
    const auto profile_f0 = profile::Clock::now();
    auto [pitch_full, pitchf_full] = f0_to_pitch(f0_full, params.pitch_shift,
        std::max(f0_full.size(), padded.size() / kHop16k + 1));
    const auto profile_pitch = profile::Clock::now();

    const auto maximum_shape = model_->generator_engine().max_input_shape("feats");
    const bool full_shape_supported = maximum_shape.size() < 2 || maximum_shape[1] <= 0
        || full_frames <= static_cast<size_t>(maximum_shape[1]);
    if (dynamic_generator && !full_shape_supported) {
        // 超出整段 profile 时用引擎允许的最大 T，窗长由 T 反推
        //（offline_window_geometry），不再固定 398 或 10 s。
        generator_frames = static_cast<size_t>(maximum_shape[1]);
    }

    // 生成器帧数 = 2×HuBERT 帧数，奇数 T 会使窗输入少产一帧，特征以末帧重复
    // 顶替造成帧数失配。入口拒绝，避免静默降级。
    if (generator_frames % 2 != 0) {
        throw std::invalid_argument("Generator frame count must be even");
    }

    // The correctness-first dynamic contract follows the original RVC path:
    // one reflect-padded prefix enters HuBERT and one Generator invocation.
    // Static engines retain the legacy fixed-window behavior below.
    if (dynamic_generator && full_shape_supported) {
        auto converted = infer_window(padded, pitchf_full, pitch_full, params, cancelled);
        const size_t context = kPad16k * model_sr / 16000;
        if (converted.size() > 2 * context) {
            converted = std::vector<float>(
                converted.begin() + static_cast<std::ptrdiff_t>(context),
                converted.end() - static_cast<std::ptrdiff_t>(context));
        }
        // HuBERT 帧数按窗长取整，动态生成器输出可能比输入短一帧；文件数据面
        // 要求输出帧数等于输入帧数，按输入时长补齐（多出部分裁掉）。
        const size_t expected = samples_for(audio_16k.size(), model_sr);
        converted.resize(expected, 0.0f);
        converted = apply_rms_mix(audio_16k, std::move(converted), params);
        if (model_sr != output_sample_rate_) {
            converted = resample(converted, model_sr, output_sample_rate_);
        }
        limit_peak(converted, 0.99f);
        converted = finalize_output(std::move(converted));
        if (profile::enabled()) {
            const auto profile_done = profile::Clock::now();
            spdlog::info(
                "[profile][infer] input={} padded={} preprocess={:.3f}ms pad={:.3f}ms "
                "f0={:.3f}ms pitch={:.3f}ms hubert_generator_post={:.3f}ms total={:.3f}ms",
                audio.size(), padded.size(),
                profile::elapsed_ms(profile_start, profile_preprocessed),
                profile::elapsed_ms(profile_preprocessed, profile_padded),
                profile::elapsed_ms(profile_padded, profile_f0),
                profile::elapsed_ms(profile_f0, profile_pitch),
                profile::elapsed_ms(profile_pitch, profile_done),
                profile::elapsed_ms(profile_start, profile_done));
        }
        return converted;
    }

    // A statically exported full-length Generator can follow the Golden
    // pipeline directly. This avoids changing the model's sequence semantics
    // through 2-second chunking when a matching fixed-length export exists.
    // 按引擎形状选整段路径：输入帧数落在引擎容量加补帧余量内即可，特征由
    // synthesize_window 补/裁到引擎帧数；不要求与输入帧数严格相等。
    if (generator_frames != kGeneratorFrames && full_frames >= generator_frames
        && full_frames <= generator_frames + kFullLengthFrameSlack) {
        auto pitchf = slice_or_pad(pitchf_full, 0, generator_frames);
        auto pitch = std::vector<int64_t>(generator_frames, 1);
        for (size_t t = 0; t < generator_frames; ++t) pitch[t] = pitch_full[t];
        auto converted = infer_window(padded, pitchf, pitch, params, cancelled);
        const size_t context = kPad16k * model_sr / 16000;
        if (converted.size() > 2 * context) {
            converted = std::vector<float>(
                converted.begin() + static_cast<std::ptrdiff_t>(context),
                converted.end() - static_cast<std::ptrdiff_t>(context));
        }
        converted = apply_rms_mix(audio_16k, std::move(converted), params);
        if (model_sr != output_sample_rate_) {
            converted = resample(converted, model_sr, output_sample_rate_);
        }
        limit_peak(converted, 0.99f);
        return finalize_output(std::move(converted));
    }

    // 分窗/单窗内层几何：窗长由 HuBERT 帧数反推，pad 按 T/398 缩放并封顶。
    const auto geometry = offline_window_geometry(generator_frames, model_sr);
    const size_t pad_16k = geometry.pad_16k;
    const size_t win_16k = geometry.win_16k;
    const size_t window_input = geometry.window_input;
    const size_t pad_tgt = geometry.pad_tgt;
    const size_t window_out = geometry.window_out;
    const size_t crossfade_out = geometry.crossfade_out;
    const size_t hop_in = geometry.hop_in;

    if (audio_16k.size() <= win_16k) {
        // Golden-aligned: feed the reflect-padded window to HuBERT (matching
        // RVC pipeline.vc which pads by t_pad before HuBERT) and run the
        // generator at the engine's native T. Crop by pad_tgt each side like
        // RVC's [t_pad_tgt:-t_pad_tgt], then pad to the block contract so the
        // streaming pipeline's out_block_/emit_out_ assumptions hold.
        const size_t T = generator_frames;
        // F0 按外层 kPad16k 提取，单窗内层 pad 缩到 pad_16k，音高索引补两者
        // 之差对齐（T=398 时差 0，与旧行为逐位一致）。
        const size_t pitch_offset = (kPad16k - pad_16k) / kHop16k;
        auto pitchf = slice_or_pad(pitchf_full, pitch_offset, T);
        std::vector<int64_t> pitch(T, 1);
        for (size_t t = 0; t < T; ++t) {
            const size_t source = std::min(pitch_offset + t, pitch_full.size() - 1);
            pitch[t] = pitch_full[source];
        }
        auto hubert_input = reflect_pad(audio_16k, pad_16k);
        auto converted = infer_window(hubert_input, pitchf, pitch, params, cancelled);
        if (converted.size() > 2 * pad_tgt) {
            converted = std::vector<float>(
                converted.begin() + static_cast<std::ptrdiff_t>(pad_tgt),
                converted.end() - static_cast<std::ptrdiff_t>(pad_tgt));
        }
        if (converted.size() < window_out) {
            converted.resize(window_out, 0.0f);
        }
        converted = apply_rms_mix(audio_16k, std::move(converted), params);
        if (model_sr != output_sample_rate_) {
            converted = resample(converted, model_sr, output_sample_rate_);
        }
        limit_peak(converted, 0.99f);
        return finalize_output(std::move(converted));
    }

    // 分窗的 source_phase 需要从整段起点累计的 F0 周期，避免每窗从 0 重新起相。
    std::vector<double> pitch_cycles(pitchf_full.size() + 1, 0.0);
    for (size_t i = 0; i < pitchf_full.size(); ++i) {
        pitch_cycles[i + 1] = pitch_cycles[i]
            + static_cast<double>(pitchf_full[i]) * kF0FrameSeconds;
    }

    std::vector<float> assembled;
    assembled.reserve((padded.size() * model_sr / 16000) + window_out);
    size_t windows = 0;
    for (size_t start = 0; start < padded.size(); start += hop_in) {
        check_cancelled(cancelled);
        // 内窗用整段里的真实相邻音频当上下文；只有越过整段首尾时才
        // 反射。RVC 的 Golden 路径也是对整段 pad 后按块切，而不是
        // 对每个块单独做 reflect。最后一个窗为保证定长会越过整段
        // pad 末尾，越界部分仍是反射；Golden 在最后一块会截断。
        auto hubert_input = slice_with_edge_reflect(
            padded,
            static_cast<int64_t>(start) - static_cast<int64_t>(pad_16k),
            window_input);
        // 音高从输入上下文起点开始切，而不是主窗起点；否则整个窗的
        // pitch 会比音频晚 pad，表现为跑调、电音。
        const int64_t pitch_start =
            (static_cast<int64_t>(start) - static_cast<int64_t>(pad_16k))
            / static_cast<int64_t>(kHop16k);
        auto pitchf = slice_with_edge_reflect(pitchf_full, pitch_start, generator_frames);
        auto pitch = slice_with_edge_reflect(pitch_full, pitch_start, generator_frames);
        // 噪声/相位按绝对帧索引对齐：第 0 窗 pitch_start<0（上下文起点在整段
        // 起点之前），不夹到 0，否则首缝与下一窗噪声/相位错网格（金属感）。
        // absolute_cycles 为整段起点到上下文起点的累计周期；pitch_start<0 时
        // 取左 pad 周期的负值，与反射音频的音高一致。
        double absolute_cycles;
        if (pitch_start >= 0) {
            absolute_cycles = pitch_cycles[static_cast<size_t>(pitch_start)];
        } else {
            // 第 0 窗上下文起点在整段起点之前；左 pad 周期取负抵消本地 cumsum。
            // left_frames = pad_16k/160 ≤ 100 < T = pitchf.size()，无需边界守卫。
            const size_t left_frames = static_cast<size_t>(-pitch_start);
            double left_cycles = 0.0;
            for (size_t i = 0; i < left_frames; ++i) {
                left_cycles += static_cast<double>(pitchf[i]) * kF0FrameSeconds;
            }
            absolute_cycles = -left_cycles;
        }
        auto converted = infer_window(
            hubert_input, pitchf, pitch, params, cancelled,
            true, pitch_start, absolute_cycles);
        if (converted.size() > 2 * pad_tgt) {
            converted = std::vector<float>(
                converted.begin() + static_cast<std::ptrdiff_t>(pad_tgt),
                converted.end() - static_cast<std::ptrdiff_t>(pad_tgt));
        }
        if (converted.size() < window_out) converted.resize(window_out, 0.0f);
        if (windows == 0) {
            assembled.insert(assembled.end(), converted.begin(),
                             converted.begin() + static_cast<std::ptrdiff_t>(std::min(converted.size(), window_out)));
        } else {
            const size_t overlap = std::min(crossfade_out, assembled.size());
            const size_t assembled_at = assembled.size() - overlap;
            for (size_t i = 0; i < overlap; ++i) {
                const float t = (static_cast<float>(i) + 0.5f)
                    / static_cast<float>(std::max(size_t{1}, overlap));
                const float w_in = std::sin(0.5f * kPi * t);
                const float w_out = std::cos(0.5f * kPi * t);
                assembled[assembled_at + i] = assembled[assembled_at + i] * w_out + converted[i] * w_in;
            }
            assembled.insert(assembled.end(),
                             converted.begin() + static_cast<std::ptrdiff_t>(overlap),
                             converted.begin() + static_cast<std::ptrdiff_t>(window_out));
        }
        ++windows;
        if (start + win_16k >= padded.size()) break;
    }

    if (assembled.size() > pad_out) {
        assembled.erase(assembled.begin(), assembled.begin() + static_cast<std::ptrdiff_t>(pad_out));
    }
    const size_t expected = samples_for(audio_16k.size(), model_sr);
    if (assembled.size() > expected) assembled.resize(expected);

    assembled = apply_rms_mix(audio_16k, std::move(assembled), params);
    if (model_sr != output_sample_rate_) {
        assembled = resample(assembled, model_sr, output_sample_rate_);
    }
    limit_peak(assembled, 0.99f);
    return finalize_output(std::move(assembled));
}

std::vector<float> RVCInferencer::infer_realtime(
    const std::vector<float>& audio,
    const RvcRealtimeRequest& request
) {
    if (!model_->loaded() || !model_->has_realtime_generator()) {
        throw std::runtime_error("Realtime Generator engines not loaded");
    }
    if (input_sample_rate_ != kRealtimeInputSampleRate
        || !is_supported_realtime_sample_rate(model_->config().sample_rate)
        || request.block_samples_16k == 0
        || request.block_samples_16k % kHop16k != 0
        || request.return_frames == 0
        || audio.size() % kHop16k != 0) {
        throw std::invalid_argument("Unsupported realtime RVC contract");
    }

    const SessionParams params = snapshot_params();
    const auto profile_start = profile::Clock::now();
    const size_t frames = audio.size() / kHop16k;
    if (request.skip_head_frames + request.return_frames != frames) {
        throw std::invalid_argument("Realtime RVC skip/return frames do not cover input");
    }

    auto& front = model_->realtime_front_engine();
    auto& decoder = model_->realtime_decoder_engine();
    const auto front_feats_shape = front.input_shape("feats");
    if (front_feats_shape.size() != 3 || front_feats_shape[0] != 1
        || front_feats_shape[1] != static_cast<int64_t>(frames)
        || front_feats_shape[2] != static_cast<int64_t>(model_->config().emb_channels)) {
        throw std::invalid_argument("Realtime Generator front shape does not match request");
    }

    const char* parallel_setting = std::getenv("MOZART_RVC_PARALLEL_FEATURES");
    const bool parallel = !parallel_setting || std::string(parallel_setting) != "0";
    // F0 与 HuBERT 只读取音频。future 析构在异常路径中等待，保护输入寿命。
    std::future<std::vector<float>> feature_task;
    if (parallel) {
        feature_task = std::async(std::launch::async, [&] {
            return feature_extractor_->extract_features_realtime(audio, 16000);
        });
    }

    size_t f0_samples = request.block_samples_16k + 800;
    if (params.f0_method == "rmvpe") {
        f0_samples = 5120 * ((f0_samples - 1) / 5120 + 1) - kHop16k;
    }
    if (f0_samples > audio.size()) {
        throw std::invalid_argument("Realtime F0 window exceeds analysis input");
    }
    const std::vector<float> f0_audio(
        audio.end() - static_cast<std::ptrdiff_t>(f0_samples), audio.end()
    );
    const auto f0 = feature_extractor_->extract_f0_realtime(
        f0_audio, 16000, params.f0_method);
    if (f0.size() < 5) {
        throw std::runtime_error("Realtime F0 extractor returned too few frames");
    }
    // 实时链路保留无基频帧为 pitch=1、pitchf=0：使声码器 NSF 清音分支关闭正弦谐波，
    // 同时让清辅音保护（protect）按真实无声帧生效，减少清音与停顿处的电音感。
    auto [new_pitch, new_pitchf] = f0_to_pitch(
        f0, params.pitch_shift, f0.size(), /*interpolate_unvoiced=*/false
    );

    const size_t shift = request.block_samples_16k / kHop16k;
    push_realtime_pitch_cache(new_pitch, new_pitchf, shift);
    if (model_->has_sculptor()) {
        push_realtime_intent_cache(audio.data(), audio.size(), new_pitchf, shift);
    }
    if (frames > realtime_pitch_cache_.size()) {
        throw std::invalid_argument("Realtime pitch cache is smaller than analysis input");
    }
    std::vector<int64_t> pitch(
        realtime_pitch_cache_.end() - static_cast<std::ptrdiff_t>(frames),
        realtime_pitch_cache_.end()
    );
    std::vector<float> pitchf(
        realtime_pitchf_cache_.end() - static_cast<std::ptrdiff_t>(frames),
        realtime_pitchf_cache_.end()
    );
    const auto profile_f0 = profile::Clock::now();

    auto feats = parallel ? feature_task.get()
                          : feature_extractor_->extract_features_realtime(audio, 16000);
    const uint32_t emb_dim = model_->config().emb_channels;
    if (feats.empty() || feats.size() % emb_dim != 0) {
        throw std::runtime_error("Realtime HuBERT feature shape is invalid");
    }
    const bool sculptor_active = model_->has_sculptor();
    const size_t feature_frames = feats.size() / emb_dim;
    const size_t index_start = std::min(request.skip_head_frames / 2, feature_frames);
    std::vector<float> unindexed_tail;
    // sculptor 激活时 delta_feats 替代索引检索与清辅音 protect。
    if (!sculptor_active && model_->index().loaded() && params.index_rate > 0.0f) {
        std::vector<float> tail(
            feats.begin() + static_cast<std::ptrdiff_t>(index_start * emb_dim),
            feats.end()
        );
        if (!tail.empty()) {
            model_->index().set_nprobe(static_cast<uint32_t>(params.index_nprobe));
            auto indexed = model_->index().search(tail, emb_dim, params.index_rate);
            if (params.protect < 0.5f) {
                unindexed_tail = std::move(tail);
            }
            std::copy(
                indexed.begin(), indexed.end(),
                feats.begin() + static_cast<std::ptrdiff_t>(index_start * emb_dim)
            );
        }
    }
    std::vector<float> resized_feats(frames * emb_dim, 0.0f);
    for (size_t target = 0; target < frames; ++target) {
        const size_t source = std::min(target / 2, feature_frames - 1);
        std::copy_n(
            feats.begin() + static_cast<std::ptrdiff_t>(source * emb_dim),
            emb_dim,
            resized_feats.begin() + static_cast<std::ptrdiff_t>(target * emb_dim)
        );
    }
    if (!sculptor_active) {
        apply_realtime_protect(
            resized_feats, unindexed_tail, pitchf,
            request.skip_head_frames, index_start, feature_frames, emb_dim, params.protect
        );
    }
    const auto profile_hubert = profile::Clock::now();

    std::vector<float> sculptor_gain_db;
    if (sculptor_active) {
        apply_realtime_sculptor(resized_feats, pitchf, pitch, sculptor_gain_db);
    }

    const auto latent_shape = front.input_shape("latent_noise");
    if (latent_shape.size() != 3 || latent_shape[0] != 1
        || latent_shape[1] <= 0 || latent_shape[2] <= 0) {
        throw std::runtime_error("Realtime front latent-noise shape is invalid");
    }
    const size_t latent_channels = static_cast<size_t>(latent_shape[1]);
    const size_t latent_frames = static_cast<size_t>(latent_shape[2]);
    std::normal_distribution<float> normal(0.0f, 1.0f);
    std::uniform_real_distribution<float> uniform(0.0f, 1.0f);
    std::vector<float> latent_noise(latent_channels * latent_frames);
    for (float& value : latent_noise) value = normal(realtime_random_);
    std::vector<float> decoder_pitchf(request.return_frames, 0.0f);
    std::copy_n(
        pitchf.begin() + static_cast<std::ptrdiff_t>(request.skip_head_frames),
        request.return_frames, decoder_pitchf.begin()
    );
    const auto source_shape = decoder.input_shape("source_noise");
    const size_t source_dim = source_shape.size() == 3 && source_shape[2] > 0
        ? static_cast<size_t>(source_shape[2]) : 1;
    const size_t output_per_frame = model_->config().sample_rate / 100;
    std::vector<float> source_phase(source_dim);
    for (float& value : source_phase) value = uniform(realtime_random_);
    if (!source_phase.empty()) source_phase.front() = 0.0f;
    std::vector<float> source_noise(
        request.return_frames * output_per_frame * source_dim
    );
    for (float& value : source_noise) value = normal(realtime_random_);

    std::vector<int64_t> sid_int64 = {model_->config().spk_id};
    std::vector<int64_t> p_len_int64 = {static_cast<int64_t>(frames)};
    std::vector<float> sid_float = {static_cast<float>(model_->config().spk_id)};
    std::vector<float> p_len_float = {static_cast<float>(frames)};
    std::vector<float> pitch_float(pitch.begin(), pitch.end());
    const auto typed_input = [](
        IEngine& engine, const char* name, std::vector<int64_t> shape,
        std::vector<float> floats, std::vector<int64_t> int64s
    ) {
        const auto type = engine.input_type(name).value_or(OnnxInput::Type::Float);
        return OnnxInput{
            name, std::move(shape), type, std::move(floats), std::move(int64s)
        };
    };
    std::vector<OnnxInput> front_inputs;
    front_inputs.reserve(5);
    front_inputs.push_back(typed_input(
        front, "feats", {1, static_cast<int64_t>(frames), static_cast<int64_t>(emb_dim)},
        std::move(resized_feats), {}
    ));
    front_inputs.push_back(typed_input(
        front, "p_len", {1}, std::move(p_len_float), std::move(p_len_int64)
    ));
    front_inputs.push_back(typed_input(
        front, "pitch", {1, static_cast<int64_t>(frames)},
        std::move(pitch_float), std::move(pitch)
    ));
    front_inputs.push_back(typed_input(
        front, "sid", {1}, sid_float, sid_int64
    ));
    front_inputs.push_back(typed_input(
        front, "latent_noise",
        {1, static_cast<int64_t>(latent_channels), static_cast<int64_t>(latent_frames)},
        std::move(latent_noise), {}
    ));

    std::vector<OnnxInput> decoder_inputs;
    decoder_inputs.reserve(5);
    decoder_inputs.push_back(typed_input(
        decoder, "z",
        {1, static_cast<int64_t>(latent_channels), static_cast<int64_t>(request.return_frames)},
        {}, {}
    ));
    decoder_inputs.push_back(typed_input(
        decoder, "pitchf", {1, static_cast<int64_t>(request.return_frames)},
        std::move(decoder_pitchf), {}
    ));
    decoder_inputs.push_back(typed_input(
        decoder, "sid", {1}, std::move(sid_float), std::move(sid_int64)
    ));
    decoder_inputs.push_back(typed_input(
        decoder, "source_phase", {1, 1, static_cast<int64_t>(source_dim)},
        std::move(source_phase), {}
    ));
    decoder_inputs.push_back(typed_input(
        decoder, "source_noise",
        {1, static_cast<int64_t>(request.return_frames * output_per_frame),
         static_cast<int64_t>(source_dim)},
        std::move(source_noise), {}
    ));
    const auto profile_prepared = profile::Clock::now();
    auto output = front.run_chained(front_inputs, "z", decoder, decoder_inputs, "z", "audio");
    if (output.size() != request.output_samples) {
        throw std::runtime_error("Realtime decoder output shape is invalid");
    }
    const size_t block_input_16k = request.return_frames * kHop16k;
    if (sculptor_active) {
        // L2 增益替代 rms_mix_rate 音量跟随：return 区间逐帧插值后限幅。
        const size_t gain_frames = sculptor_gain_db.size();
        const size_t first = request.skip_head_frames;
        const size_t samples_per_frame = model_->config().sample_rate / 100;
        for (size_t s = 0; s < output.size(); ++s) {
            const double pos = static_cast<double>(first)
                + static_cast<double>(s) / static_cast<double>(samples_per_frame);
            const size_t lo = std::min(static_cast<size_t>(pos), gain_frames - 1);
            const size_t hi = std::min(lo + 1, gain_frames - 1);
            const float frac = static_cast<float>(pos) - static_cast<float>(lo);
            const float db = sculptor_gain_db[lo] * (1.0f - frac) + sculptor_gain_db[hi] * frac;
            output[s] *= std::pow(10.0f, db / 20.0f);
        }
        limit_peak(output, 0.99f);
    } else if (block_input_16k <= audio.size()) {
        apply_realtime_rms_mix_and_limit(
            audio.data() + (audio.size() - block_input_16k),
            block_input_16k,
            output,
            model_->config().sample_rate,
            params.rms_mix_rate,
            0.99f
        );
    } else {
        limit_peak(output, 0.99f);
    }
    if (profile::enabled()) {
        const auto profile_done = profile::Clock::now();
        spdlog::info(
            "[profile][realtime] input={} frames={} f0_with_overlap={:.3f}ms hubert_wait_prepare={:.3f}ms "
            "generator_prepare={:.3f}ms generator={:.3f}ms total={:.3f}ms",
            audio.size(), frames,
            profile::elapsed_ms(profile_start, profile_f0),
            profile::elapsed_ms(profile_f0, profile_hubert),
            profile::elapsed_ms(profile_hubert, profile_prepared),
            profile::elapsed_ms(profile_prepared, profile_done),
            profile::elapsed_ms(profile_start, profile_done)
        );
    }
    return output;
}

std::vector<float> RVCInferencer::infer_window(
    const std::vector<float>& audio_16k,
    const std::vector<float>& pitchf,
    const std::vector<int64_t>& pitch,
    const SessionParams& params,
    const std::atomic<bool>* cancelled,
    bool absolute_noise,
    int64_t absolute_frame_start,
    double absolute_cycles
) {
    check_cancelled(cancelled);
    const auto profile_start = profile::Clock::now();
    auto feats = feature_extractor_->extract_features(audio_16k, 16000);
    check_cancelled(cancelled);
    const auto profile_hubert = profile::Clock::now();
    const auto unindexed = feats;
    feats = apply_index(feats, params);
    check_cancelled(cancelled);
    const auto profile_index = profile::Clock::now();
    auto output = synthesize_window(
        feats, pitchf, pitch, unindexed, params,
        absolute_noise, absolute_frame_start, absolute_cycles);
    check_cancelled(cancelled);
    // 窗完成才计数：取消时未完成的窗不计入文件批处理进度。
    generator_windows_.fetch_add(1, std::memory_order_relaxed);
    if (profile::enabled()) {
        const auto profile_done = profile::Clock::now();
        spdlog::info(
            "[profile][window] input={} features={} hubert={:.3f}ms feature_copies_index={:.3f}ms "
            "generator={:.3f}ms total={:.3f}ms",
            audio_16k.size(), feats.size(),
            profile::elapsed_ms(profile_start, profile_hubert),
            profile::elapsed_ms(profile_hubert, profile_index),
            profile::elapsed_ms(profile_index, profile_done),
            profile::elapsed_ms(profile_start, profile_done));
    }
    return output;
}

std::vector<float> RVCInferencer::resample(
    const std::vector<float>& audio,
    uint32_t src_sr, uint32_t dst_sr
) {
    return resample_audio(audio, src_sr, dst_sr);
}

std::vector<float> RVCInferencer::apply_index(
    const std::vector<float>& feats, const SessionParams& params) {
    if (!model_->index().loaded() || params.index_rate <= 0.0f) {
        return feats;
    }

    model_->index().set_nprobe(static_cast<uint32_t>(params.index_nprobe));
    return model_->index().search(
        feats,
        model_->config().emb_channels,
        params.index_rate
    );
}

std::vector<float> RVCInferencer::synthesize_window(
    const std::vector<float>& feats,
    const std::vector<float>& pitchf,
    const std::vector<int64_t>& pitch,
    const std::vector<float>& unindexed_feats,
    const SessionParams& params,
    bool absolute_noise,
    int64_t absolute_frame_start,
    double absolute_cycles
) {
    if (!model_->generator_engine().loaded()) {
        throw std::runtime_error("Generator engine not loaded");
    }

    const auto profile_start = profile::Clock::now();
    uint32_t emb_dim = model_->config().emb_channels;
    size_t T = kGeneratorFrames;
    const auto generator_shape = model_->generator_engine().input_shape("feats");
    if (generator_shape.size() >= 2 && generator_shape[1] > 0) {
        T = static_cast<size_t>(generator_shape[1]);
    } else if (generator_shape.size() >= 2 && generator_shape[1] <= 0) {
        T = 2 * (feats.size() / emb_dim);
    }

    const auto resize_features = [&](const std::vector<float>& source_feats) {
        std::vector<float> resized(T * emb_dim, 0.0f);
        const size_t source_frames = std::max(size_t{1}, source_feats.size() / emb_dim);
        for (size_t target = 0; target < T; ++target) {
            const size_t source = std::min(target / 2, source_frames - 1);
            for (size_t channel = 0; channel < emb_dim; ++channel) {
                const size_t source_offset = source * emb_dim + channel;
                if (source_offset < source_feats.size()) {
                    resized[target * emb_dim + channel] = source_feats[source_offset];
                }
            }
        }
        return resized;
    };

    std::vector<float> feats_reshaped = resize_features(feats);
    const std::vector<float> unindexed_reshaped = resize_features(unindexed_feats);
    std::vector<float> pitchf_fixed(T, 0.0f);
    std::vector<int64_t> pitch_fixed(T, 1);
    for (size_t t = 0; t < T; ++t) {
        if (t < pitchf.size()) pitchf_fixed[t] = pitchf[t];
        if (t < pitch.size()) pitch_fixed[t] = pitch[t];
    }

    if (params.protect > 0.0f && params.protect < 1.0f) {
        for (size_t t = 0; t < T; ++t) {
            const float feature_weight = pitchf_fixed[t] > 0.0f ? 1.0f : params.protect;
            for (size_t channel = 0; channel < emb_dim; ++channel) {
                const size_t offset = t * emb_dim + channel;
                feats_reshaped[offset] = feats_reshaped[offset] * feature_weight
                    + unindexed_reshaped[offset] * (1.0f - feature_weight);
            }
        }
    }

    std::vector<int64_t> sid_int64 = {model_->config().spk_id};
    std::vector<int64_t> p_len_int64 = {static_cast<int64_t>(T)};
    std::vector<float> sid_float = {static_cast<float>(model_->config().spk_id)};
    std::vector<float> p_len_float = {static_cast<float>(T)};
    std::vector<float> pitch_float(pitch_fixed.begin(), pitch_fixed.end());

    std::vector<int64_t> feats_shape = {1, static_cast<int64_t>(T),
                                         static_cast<int64_t>(emb_dim)};
    std::vector<int64_t> p_len_shape = {1};
    std::vector<int64_t> pitch_shape = {1, static_cast<int64_t>(T)};
    std::vector<int64_t> pitchf_shape = {1, static_cast<int64_t>(T)};
    std::vector<int64_t> sid_shape = {1};

    const auto typed_input = [&](const char* name, const std::vector<int64_t>& shape,
                                  std::vector<float>&& floats, std::vector<int64_t>&& int64s) {
        const auto type = model_->generator_engine().input_type(name).value_or(OnnxInput::Type::Float);
        return OnnxInput{name, shape, type, std::move(floats), std::move(int64s)};
    };
    std::vector<OnnxInput> inputs;
    inputs.reserve(8);
    inputs.push_back(typed_input(
        "feats", feats_shape, std::move(feats_reshaped), {}
    ));
    inputs.push_back(typed_input(
        "p_len", p_len_shape, std::move(p_len_float), std::move(p_len_int64)
    ));
    inputs.push_back(typed_input(
        "pitch", pitch_shape, std::move(pitch_float), std::move(pitch_fixed)
    ));
    inputs.push_back(typed_input(
        "pitchf", pitchf_shape, std::move(pitchf_fixed), {}
    ));
    inputs.push_back(typed_input(
        "sid", sid_shape, std::move(sid_float), std::move(sid_int64)
    ));

    if (model_->generator_engine().input_type("latent_noise")) {
        const auto latent_model_shape =
            model_->generator_engine().input_shape("latent_noise");
        const size_t channels = latent_model_shape.size() >= 2
            && latent_model_shape[1] > 0
            ? static_cast<size_t>(latent_model_shape[1]) : 192;
        const auto source_model_shape =
            model_->generator_engine().input_shape("source_noise");
        const size_t source_dim = source_model_shape.size() >= 3
            && source_model_shape[2] > 0
            ? static_cast<size_t>(source_model_shape[2]) : 1;
        const size_t output_per_frame = model_->config().sample_rate / 100;

        std::vector<float> latent_noise(channels * T);
        std::vector<float> source_phase(source_dim);
        std::vector<float> source_noise(T * output_per_frame * source_dim);
        if (!absolute_noise) {
            // 整段路径保持原来的固定种子序列。
            std::mt19937 random(114514);
            std::normal_distribution<float> normal(0.0f, 1.0f);
            std::uniform_real_distribution<float> uniform(0.0f, 1.0f);
            for (float& value : latent_noise) value = normal(random);
            for (float& value : source_phase) value = uniform(random);
            source_phase.front() = 0.0f;
            for (float& value : source_noise) value = normal(random);
        } else {
            // 分窗路径按绝对帧/样本索引键噪声：重叠区绝对索引≥0，跨窗同键。
            // 第 0 窗左 pad 绝对索引<0，wrap 成 uint64 仍确定（该区被裁剪）。
            // source_phase 用累计 F0 周期补偿本地 cumsum 的相位起点。
            auto excitation = build_absolute_excitation(
                channels, T, absolute_frame_start,
                T, absolute_frame_start,
                output_per_frame, source_dim, absolute_cycles
            );
            latent_noise = std::move(excitation.latent_noise);
            source_phase = std::move(excitation.source_phase);
            source_noise = std::move(excitation.source_noise);
        }

        inputs.push_back(typed_input(
            "latent_noise",
            {1, static_cast<int64_t>(channels), static_cast<int64_t>(T)},
            std::move(latent_noise), {}
        ));
        inputs.push_back(typed_input(
            "source_phase", {1, 1, static_cast<int64_t>(source_dim)},
            std::move(source_phase), {}
        ));
        inputs.push_back(typed_input(
            "source_noise",
            {1, static_cast<int64_t>(T * output_per_frame),
             static_cast<int64_t>(source_dim)},
            std::move(source_noise), {}
        ));
    }
    const auto profile_prepared = profile::Clock::now();
    auto output = model_->generator_engine().run(inputs, {"audio"});
    if (profile::enabled()) {
        const auto profile_done = profile::Clock::now();
        spdlog::info(
            "[profile][generator] frames={} prepare={:.3f}ms engine={:.3f}ms total={:.3f}ms",
            T, profile::elapsed_ms(profile_start, profile_prepared),
            profile::elapsed_ms(profile_prepared, profile_done),
            profile::elapsed_ms(profile_start, profile_done));
    }
    return output;
}

std::vector<float> RVCInferencer::apply_rms_mix(
    const std::vector<float>& original_16k,
    std::vector<float> audio_out,
    const SessionParams& params
) {
    if (params.rms_mix_rate >= 1.0f || original_16k.empty() || audio_out.empty()) {
        return audio_out;
    }
    const auto rms_curve = [](const std::vector<float>& samples, uint32_t sample_rate) {
        const size_t frame_size = std::max<size_t>(1, sample_rate);
        const size_t hop_size = std::max<size_t>(1, sample_rate / 2);
        const size_t center_pad = frame_size / 2;
        std::vector<float> result;
        for (size_t center = 0; center <= samples.size(); center += hop_size) {
            const size_t start = center > center_pad ? center - center_pad : 0;
            const size_t end = std::min(samples.size(), center + center_pad);
            float sum = 0.0f;
            for (size_t i = start; i < end; ++i) sum += samples[i] * samples[i];
            result.push_back(std::sqrt(sum / static_cast<float>(frame_size)));
        }
        return result;
    };
    const auto input_rms = rms_curve(original_16k, 16000);
    const auto output_rms = rms_curve(audio_out, model_->config().sample_rate);
    const auto interpolate = [&](const std::vector<float>& curve, size_t sample) {
        if (curve.size() == 1) return curve.front();
        const float source = (static_cast<float>(sample) + 0.5f)
            * static_cast<float>(curve.size()) / static_cast<float>(audio_out.size()) - 0.5f;
        const float clamped = std::clamp(source, 0.0f, static_cast<float>(curve.size() - 1));
        const size_t left = static_cast<size_t>(clamped);
        const size_t right = std::min(left + 1, curve.size() - 1);
        const float amount = clamped - static_cast<float>(left);
        return curve[left] * (1.0f - amount) + curve[right] * amount;
    };
    for (size_t i = 0; i < audio_out.size(); ++i) {
        const float in_level = interpolate(input_rms, i);
        const float out_level = std::max(interpolate(output_rms, i), 1e-6f);
        const float correction = std::pow(std::max(in_level, 1e-6f) / out_level,
                                          1.0f - params.rms_mix_rate);
        audio_out[i] *= correction;
    }
    return audio_out;
}

} // namespace rvc
