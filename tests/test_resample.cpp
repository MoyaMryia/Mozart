// test_resample.cpp — 重采样抗镜像/抗混叠单测（无 GPU 依赖，合成信号频谱度量）
// ============================================================================
// 1. 同采样率/空输入短路，原样返回（现用 48 kHz 模型走这条，零回归）
// 2. 输出长度符合 L/M 比例
// 3. 上采样通带保留 + 抗镜像：40k→48k，18 kHz 音保留，22 kHz 镜像被压掉
// 4. 下采样抗混叠：48k→16k，10 kHz 音的 6 kHz 混叠被压掉
// 5. 下采样通带保留：3 kHz 音原样
// 6. 低频上采样与解析正弦逐样本相关 ≈ 1（群延迟已补偿，无偏移）
#include "rvc/resample.hpp"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <vector>

using namespace rvc;

namespace {

int g_fail = 0;
#define CHECK(cond, msg) do { \
    if (cond) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s\n", msg); ++g_fail; } } while (0)

constexpr double kPi = 3.14159265358979323846;

std::vector<float> sine(double fs, double freq, double seconds, double amp = 1.0) {
    const size_t n = static_cast<size_t>(fs * seconds);
    std::vector<float> x(n);
    for (size_t i = 0; i < n; ++i)
        x[i] = static_cast<float>(amp * std::sin(2 * kPi * freq * static_cast<double>(i) / fs));
    return x;
}

// 信号在频率 f 处的峰值幅度（整数周期时即该正弦分量幅度）。
double amplitude(const std::vector<float>& x, double fs, double f) {
    double re = 0.0, im = 0.0;
    const double w = 2 * kPi * f / fs;
    for (size_t n = 0; n < x.size(); ++n) {
        re += x[n] * std::cos(w * static_cast<double>(n));
        im += x[n] * std::sin(w * static_cast<double>(n));
    }
    return 2.0 * std::hypot(re, im) / static_cast<double>(x.size());
}

double corr(const std::vector<float>& a, const std::vector<float>& b) {
    const size_t n = std::min(a.size(), b.size());
    double ma = 0, mb = 0;
    for (size_t i = 0; i < n; ++i) { ma += a[i]; mb += b[i]; }
    ma /= n; mb /= n;
    double num = 0, da = 0, db = 0;
    for (size_t i = 0; i < n; ++i) {
        const double xa = a[i] - ma, xb = b[i] - mb;
        num += xa * xb; da += xa * xa; db += xb * xb;
    }
    return num / std::sqrt(da * db);
}

// 旧实现（纯线性插值）作对照，证明抗镜像确实改善了镜像抑制。
std::vector<float> linear(const std::vector<float>& x, double src, double dst) {
    const double ratio = dst / src;
    const size_t n = static_cast<size_t>(x.size() * ratio);
    std::vector<float> out(n);
    for (size_t i = 0; i < n; ++i) {
        const double pos = static_cast<double>(i) / ratio;
        const size_t i0 = static_cast<size_t>(pos);
        const size_t i1 = std::min(i0 + 1, x.size() - 1);
        const double frac = pos - static_cast<double>(i0);
        out[i] = static_cast<float>(x[i0] * (1 - frac) + x[i1] * frac);
    }
    return out;
}

}  // namespace

int main() {
    std::printf("test_resample\n");

    // 1. 短路：同采样率原样返回（48k 模型现况），空输入返回空。
    {
        auto x = sine(48000, 1000, 0.1);
        auto y = resample_audio(x, 48000, 48000);
        CHECK(y == x, "同采样率原样返回");
        CHECK(resample_audio({}, 40000, 48000).empty(), "空输入返回空");
    }

    // 2. 输出长度：40k→48k = 6/5。
    {
        auto x = sine(40000, 1000, 0.5);
        auto y = resample_audio(x, 40000, 48000);
        CHECK(y.size() == x.size() * 6 / 5, "40k→48k 输出长度 6/5");
    }

    // 3. 上采样抗镜像：18 kHz 通带保留，22 kHz 镜像（40k-18k）压掉。
    {
        auto x = sine(40000, 18000, 0.5);
        auto y = resample_audio(x, 40000, 48000);
        const double pass = amplitude(y, 48000, 18000);
        const double image = amplitude(y, 48000, 22000);
        const double image_lin = amplitude(linear(x, 40000, 48000), 48000, 22000);
        std::printf("    通带 18k=%.4f 镜像 22k=%.5f（线性插值镜像=%.4f）\n", pass, image, image_lin);
        CHECK(pass > 0.9, "上采样通带 18 kHz 保留");
        CHECK(image < 0.02, "上采样镜像 22 kHz 抑制 > 34 dB");
        CHECK(image < image_lin * 0.1, "镜像抑制显著优于线性插值");
    }

    // 4. 下采样抗混叠：48k 的 10 kHz 音混叠到 6 kHz，应被压掉。
    {
        auto x = sine(48000, 10000, 0.5);
        auto y = resample_audio(x, 48000, 16000);
        const double alias = amplitude(y, 16000, 6000);
        std::printf("    混叠 6k=%.5f\n", alias);
        CHECK(alias < 0.02, "下采样混叠 6 kHz 抑制 > 34 dB");
    }

    // 5. 下采样通带：3 kHz 保留。
    {
        auto x = sine(48000, 3000, 0.5);
        auto y = resample_audio(x, 48000, 16000);
        CHECK(amplitude(y, 16000, 3000) > 0.9, "下采样通带 3 kHz 保留");
    }

    // 6. 低频上采样保真：与解析正弦相关 ≈ 1（群延迟补偿后无偏移）。
    {
        auto x = sine(40000, 1000, 0.5);
        auto y = resample_audio(x, 40000, 48000);
        auto ideal = sine(48000, 1000, 0.5);
        const double c = corr(y, ideal);
        std::printf("    1k 上采样 corr=%.6f\n", c);
        CHECK(c > 0.99, "低频上采样与解析正弦相关 > 0.99");
    }

    if (g_fail == 0) std::printf("全部通过\n");
    return g_fail == 0 ? 0 : 1;
}