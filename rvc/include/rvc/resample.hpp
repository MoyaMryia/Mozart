#pragma once

// resample.hpp — 窗函数 sinc 多相重采样。
// 替代纯线性插值：线性插值没有抗镜像低通，非 48 kHz 模型上采样到 48 kHz 会残留
// 高频镜像（FUTURE.md「重采样抗镜像滤波」）。这里截止频率取两侧较低的奈奎斯特，
// 上采样抗镜像、下采样抗混叠一并处理。第 1 站输入与第 9 站输出共用。

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <numeric>
#include <vector>

namespace rvc {

inline std::vector<float> resample_audio(
    const std::vector<float>& audio, uint32_t src_sr, uint32_t dst_sr
) {
    if (src_sr == dst_sr || audio.empty()) return audio;

    constexpr double kPi = 3.14159265358979323846;
    const uint32_t g = std::gcd(src_sr, dst_sr);
    const size_t L = dst_sr / g;          // 内插倍数
    const size_t M = src_sr / g;          // 抽取倍数
    const size_t K = std::max(L, M);

    // Kaiser 窗 sinc 原型低通，截止 wc = pi/K（即较低奈奎斯特），阻带约 70 dB。
    // 每相位 kTapsPerPhase 个抽头量级，过渡带约为截止频率的 13%；板端 CPU 预算下
    // 取适中阶数，不追求极致阻带。
    constexpr size_t kTapsPerPhase = 32;
    constexpr double kBeta = 7.0;
    const size_t half = K * kTapsPerPhase;   // 原型半长（上采样率下的样本数）
    const size_t N = 2 * half + 1;
    const double center = static_cast<double>(half);

    const auto bessel_i0 = [](double x) {
        double sum = 1.0, term = 1.0;
        for (int k = 1; k < 40; ++k) {
            term *= (x / (2.0 * k)) * (x / (2.0 * k));
            sum += term;
            if (term < 1e-14 * sum) break;
        }
        return sum;
    };
    const double i0_beta = bessel_i0(kBeta);

    // h[n] = (L/K)·sinc((n-center)/K)·kaiser(n)。DC 增益为 L，零插值后平均 1/L，
    // 滤波再抽取 M，通带净增益 L/L = 1，保持幅度不变。
    std::vector<double> h(N);
    for (size_t n = 0; n < N; ++n) {
        const double x = (static_cast<double>(n) - center) / static_cast<double>(K);
        const double sinc = (x == 0.0) ? 1.0 : std::sin(kPi * x) / (kPi * x);
        const double r = (static_cast<double>(n) - center) / center;
        const double win = bessel_i0(kBeta * std::sqrt(std::max(0.0, 1.0 - r * r))) / i0_beta;
        h[n] = (static_cast<double>(L) / static_cast<double>(K)) * sinc * win;
    }

    // 多相分解：第 p 相位抽头为 h[p], h[p+L], h[p+2L], ...。每个输出样本只算一个相位。
    std::vector<std::vector<float>> phase(L);
    for (size_t p = 0; p < L; ++p) {
        for (size_t k = p; k < N; k += L) phase[p].push_back(static_cast<float>(h[k]));
    }

    const size_t n_out = audio.size() * L / M;
    const size_t n_in = audio.size();
    std::vector<float> out(n_out);
    for (size_t m = 0; m < n_out; ++m) {
        // t = m*M + half：加 half 抵消原型线性相位群延迟，使输出第 m 点对齐输入时刻 m*M/L。
        const size_t t = m * M + half;
        const size_t p = t % L;
        const size_t n0 = t / L;
        const auto& taps = phase[p];
        double acc = 0.0;
        for (size_t i = 0; i < taps.size(); ++i) {
            const int64_t raw = static_cast<int64_t>(n0) - static_cast<int64_t>(i);
            const size_t idx = raw < 0 ? 0
                : (static_cast<size_t>(raw) >= n_in ? n_in - 1 : static_cast<size_t>(raw));
            acc += static_cast<double>(taps[i]) * audio[idx];   // 边界复制，避免末端 fade
        }
        out[m] = static_cast<float>(acc);
    }
    return out;
}

} // namespace rvc