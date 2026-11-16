// test_rvc_window_geometry.cpp — 文件分窗几何单测（纯算术，无 GPU 依赖）
// ============================================================================
// 1. HuBERT 帧数公式自检：44800 样本 → 139 帧（实时引擎校验常量同源）
// 2. T=398：pad/淡化与旧常量一致，主窗取输出覆盖，零尾消除
// 3. T=200/2380/2400：窗输入恰好产出 T 帧（独立 HuBERT 帧数公式验证）
// 4. 偶数 T 扫描：帧数恒等、对齐与淡化不变量
#include "rvc/inferencer.hpp"

#include <cstdio>

using namespace rvc;

namespace {

int g_fail = 0;
#define CHECK(cond, msg) do { \
    if (cond) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s\n", msg); ++g_fail; } } while (0)

// HuBERT 卷积栈：hop 320、感受野 400 样本，生成器帧数 = 2×HuBERT 帧数。
size_t generator_frames_for_input(size_t input_samples) {
    if (input_samples < 400) return 0;
    return 2 * ((input_samples - 400) / 320 + 1);
}

} // namespace

int main() {
    // 1. 公式自检：与 feature_extractor 实时引擎校验常量 44800→139 同源。
    CHECK(generator_frames_for_input(44800) == 2 * 139,
          "HuBERT 帧数公式：44800 样本 → 139 帧");
    CHECK(generator_frames_for_input(64000) == 398,
          "HuBERT 帧数公式：64000 样本 → T=398");

    // 2. T=398（48 kHz）：pad/淡化不变，主窗 = 输出覆盖 63680 − 2×16000。
    {
        const auto g = offline_window_geometry(398, 48000);
        CHECK(g.pad_16k == 16000, "T=398 pad=16000");
        CHECK(g.window_input == 64000, "T=398 窗输入=64000");
        CHECK(g.win_16k == 31680, "T=398 主窗=31680（输出覆盖，无零尾）");
        CHECK(g.crossfade_in == 3200, "T=398 淡化=3200");
        CHECK(g.hop_in == 28480, "T=398 hop=28480");
        CHECK(g.pad_tgt == 48000, "T=398 pad_tgt=48000");
        CHECK(g.window_out == 95040, "T=398 window_out=95040");
        CHECK(g.crossfade_out == 9600, "T=398 crossfade_out=9600");
    }

    // 3. 典型 T：窗输入恰好产出 T 帧（不补零、不重复），主窗为预期常量。
    {
        const auto g = offline_window_geometry(200, 48000);
        CHECK(g.pad_16k == 8000, "T=200 pad=8000");
        CHECK(g.win_16k == 16000, "T=200 主窗=16000");
        CHECK(generator_frames_for_input(g.window_input) == 200,
              "T=200 窗输入恰好 200 帧（无末帧重复）");
    }
    {
        const auto g = offline_window_geometry(2380, 48000);
        CHECK(g.pad_16k == 16000, "T=2380 pad 封顶 16000");
        CHECK(g.win_16k == 348800, "T=2380 主窗=348800（21.8 s）");
        CHECK(generator_frames_for_input(g.window_input) == 2380,
              "T=2380 窗输入恰好 2380 帧（无末帧重复）");
    }
    {
        const auto g = offline_window_geometry(2400, 40000);
        CHECK(generator_frames_for_input(g.window_input) == 2400,
              "T=2400（large 档 40 kHz）窗输入恰好 2400 帧");
        CHECK(g.pad_tgt == 40000, "T=2400 pad_tgt 按 40 kHz 折算");
        CHECK(g.window_out == g.win_16k * 40000 / 16000,
              "T=2400 window_out 按 40 kHz 折算");
    }

    // 4. 偶数 T 扫描：帧数恒等与几何不变量。
    {
        bool frames_exact = true;
        bool aligned = true;
        bool fade_bounded = true;
        for (size_t t = 100; t <= 4200; t += 2) {
            const auto g = offline_window_geometry(t, 48000);
            if (generator_frames_for_input(g.window_input) != t) frames_exact = false;
            if (g.win_16k % 320 != 0 || g.hop_in % 160 != 0
                || g.pad_16k % 320 != 0 || g.window_input % 320 != 0) aligned = false;
            if (g.crossfade_in > g.win_16k / 4) fade_bounded = false;
        }
        CHECK(frames_exact, "T=100..4200 偶数扫描：窗输入恒好产出 T 帧");
        CHECK(aligned, "T 扫描：窗/输入/pad 对齐 320，hop 对齐 160");
        CHECK(fade_bounded, "T 扫描：淡化不超过主窗 1/4");
    }

    if (g_fail == 0) std::printf("全部通过\n");
    return g_fail == 0 ? 0 : 1;
}