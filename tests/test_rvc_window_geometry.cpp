// test_rvc_window_geometry.cpp — 文件分窗几何单测（纯算术，无 GPU 依赖）
// ============================================================================
// 1. 独立 oracle 自检：HuBERT 卷积栈逐层长度模拟，4 个独立锚点
//    （44800→139 与 feature_extractor.cpp 实时引擎校验常量一致）
// 2. T=398 黄金常量：pad/淡化/窗长与旧流式契约一致
// 3. T=200 / T=2400 具体值：pad 按 T/398 缩放并封顶、窗输入由帧数反推
// 4. 偶数 T 扫描（200..4200，model_sr 32k/40k/48k）：oracle 验证窗输入恰好产
//    T 帧、主窗覆盖无零尾、hop 对齐 160 样本、tgt 量纲换算
// 奇数 T 的拒绝在 infer() 入口（需引擎），不在本单测范围。
#include "rvc/inferencer.hpp"
#include "rvc/realtime_geometry.hpp"

#include <cstddef>
#include <cstdint>
#include <cstdio>

using namespace rvc;

namespace {

int g_fail = 0;
#define CHECK(cond, msg) do { \
    if (cond) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s\n", msg); ++g_fail; } } while (0)

// 独立 oracle：逐层模拟 HuBERT 特征提取器卷积栈的输出长度（无 padding，
// 每层 floor((in-kernel)/stride)+1）。不使用被测函数的 hop 320 / 感受野
// 400 闭式公式，避免恒真自比。
size_t hubert_frames_oracle(size_t length) {
    constexpr size_t kKernel[7] = {10, 3, 3, 3, 3, 2, 2};
    constexpr size_t kStride[7] = {5, 2, 2, 2, 2, 2, 2};
    for (int i = 0; i < 7; ++i) {
        if (length < kKernel[i]) return 0;
        length = (length - kKernel[i]) / kStride[i] + 1;
    }
    return length;
}

// 扫描断言：依赖循环局部变量 t / model_sr / g，失败时打印当前 T 与全部
// 字段便于定位；通过时不逐条打印，避免数万行输出。
#define SCAN_CHECK(cond, msg) do { \
    if (!(cond)) { \
        std::printf("  FAIL %s（T=%zu model_sr=%u）\n", \
                    msg, t, static_cast<unsigned>(model_sr)); \
        std::printf("    pad_16k=%zu win_16k=%zu window_input=%zu " \
                    "crossfade_in=%zu hop_in=%zu pad_tgt=%zu " \
                    "window_out=%zu crossfade_out=%zu\n", \
                    g.pad_16k, g.win_16k, g.window_input, g.crossfade_in, \
                    g.hop_in, g.pad_tgt, g.window_out, g.crossfade_out); \
        ++g_fail; \
    } } while (0)

}  // namespace

int main() {
    std::printf("test_rvc_window_geometry\n");

    // 1. oracle 自检：独立锚点，先证明 oracle 本身正确。
    CHECK(hubert_frames_oracle(44800) == 139, "oracle 44800 → 139 帧");
    CHECK(hubert_frames_oracle(64000) == 199, "oracle 64000 → 199 帧");
    CHECK(hubert_frames_oracle(32000) == 99, "oracle 32000 → 99 帧");
    CHECK(hubert_frames_oracle(32320) == 100, "oracle 32320 → 100 帧");

    // 2. T=398 黄金常量（48 kHz）：与旧流式契约的 pad/淡化/窗长一致。
    {
        const auto g = offline_window_geometry(398, 48000);
        CHECK(g.pad_16k == 16000, "T=398 pad_16k == 16000");
        CHECK(g.window_input == 64000, "T=398 window_input == 64000");
        CHECK(g.win_16k == 31680, "T=398 win_16k == 31680");
        CHECK(g.crossfade_in == 3200, "T=398 crossfade_in == 3200");
        CHECK(g.hop_in == 28480, "T=398 hop_in == 28480");
        CHECK(g.pad_tgt == 48000, "T=398 pad_tgt == 48000");
        CHECK(g.window_out == 95040, "T=398 window_out == 95040");
        CHECK(g.crossfade_out == 9600, "T=398 crossfade_out == 9600");
    }
    {
        const auto g = offline_window_geometry(398, 32000);
        CHECK(g.pad_tgt == 32000, "T=398@32k pad_tgt == 32000");
        CHECK(g.window_out == 63360, "T=398@32k window_out == 63360");
        CHECK(g.crossfade_out == 6400, "T=398@32k crossfade_out == 6400");
    }

    // 3. T=200：pad 按 T/398 缩放不封顶；T=2400：pad 封顶 16000。
    {
        const auto g = offline_window_geometry(200, 48000);
        CHECK(g.pad_16k == 8000, "T=200 pad_16k == 8000");
        CHECK(g.window_input == 32320, "T=200 window_input == 32320");
        CHECK(g.win_16k == 16000, "T=200 win_16k == 16000");
        CHECK(g.hop_in == 12800, "T=200 hop_in == 12800");
    }
    {
        const auto g = offline_window_geometry(2400, 48000);
        CHECK(g.window_input == 384320, "T=2400 window_input == 384320");
        CHECK(g.win_16k == 352000, "T=2400 win_16k == 352000");
        CHECK(g.hop_in == 348800, "T=2400 hop_in == 348800");
        CHECK(g.pad_16k == 16000, "T=2400 pad_16k == 16000（封顶）");
    }

    // 4. 偶数 T 扫描：窗输入由独立 oracle 验证恰好产 T 帧，其余为几何关系。
    for (uint32_t model_sr : {32000u, 40000u, 48000u}) {
        const int fail_before = g_fail;
        size_t checked = 0;
        for (size_t t = 200; t <= 4200; t += 2) {
            const auto g = offline_window_geometry(t, model_sr);
            const size_t frames = hubert_frames_oracle(g.window_input);
            SCAN_CHECK(2 * frames == t, "窗输入恰好产 T 帧（独立 oracle）");
            SCAN_CHECK(g.win_16k == t * 160 - 2 * g.pad_16k,
                       "主窗 = T*160 − 2*pad");
            SCAN_CHECK(g.window_input >= g.win_16k + 2 * g.pad_16k,
                       "窗输入含右上下文余量");
            SCAN_CHECK(2 * frames * 160 - 2 * g.pad_16k == g.win_16k,
                       "输出覆盖无零尾");
            SCAN_CHECK(g.pad_16k > 0 && g.pad_16k <= 16000,
                       "pad 为正且封顶 16000");
            SCAN_CHECK(g.crossfade_in > 0, "crossfade_in > 0");
            SCAN_CHECK(g.hop_in > 0, "hop_in > 0");
            SCAN_CHECK(g.hop_in == g.win_16k - g.crossfade_in,
                       "hop = 主窗 − 淡化");
            SCAN_CHECK(g.hop_in % 160 == 0, "hop 对齐 160 样本");
            SCAN_CHECK(g.pad_tgt == g.pad_16k * model_sr / 16000,
                       "pad_tgt 量纲换算");
            SCAN_CHECK(g.window_out == g.win_16k * model_sr / 16000,
                       "window_out 量纲换算");
            SCAN_CHECK(g.crossfade_out == g.crossfade_in * model_sr / 16000,
                       "crossfade_out 量纲换算");
            ++checked;
        }
        if (g_fail == fail_before)
            std::printf("  ok  偶数 T 扫描 200..4200 全部通过"
                        "（model_sr=%u，%zu 个 T）\n",
                        static_cast<unsigned>(model_sr), checked);
    }

    // 5. 实时几何单一来源镜像验证（48k / 40k / 32k，SPEC-03）。
    {
        const auto rt48 = derive_realtime_geometry(48000);
        CHECK(rt48.overlap_out == 2880, "rt48 overlap_out == 2880");
        CHECK(rt48.margin_16k == 960, "rt48 margin_16k == 960");
        CHECK(rt48.front_frames == 280, "rt48 front_frames == 280");
        CHECK(rt48.skip_head == 250, "rt48 skip_head == 250");
        CHECK(rt48.return_frames == 30, "rt48 return_frames == 30");
        CHECK(rt48.hubert_samples == 44800, "rt48 hubert_samples == 44800");
        CHECK(rt48.hubert_frames == 139, "rt48 hubert_frames == 139");
        CHECK(rt48.decoder_out_samples == 14400, "rt48 decoder_out_samples == 14400");
        CHECK(rt48.output_hop == 11520, "rt48 output_hop == 11520");
        CHECK(rt48.flow_head == 226, "rt48 flow_head == 226");
        CHECK(rt48.latent_frames == 54, "rt48 latent_frames == 54");
        CHECK(rt48.decoder_head == 24, "rt48 decoder_head == 24");
        CHECK(rt48.rmvpe_frames == 32, "rt48 rmvpe_frames == 32");

        const auto rt40 = derive_realtime_geometry(40000);
        CHECK(rt40.front_frames == 280, "rt40 front_frames == 280");
        CHECK(rt40.return_frames == 30, "rt40 return_frames == 30");
        CHECK(rt40.output_frame_samples == 800, "rt40 output_frame_samples == 800");
        CHECK(rt40.crossfade_max_out == 2000, "rt40 crossfade_max_out == 2000");
        CHECK(rt40.sola_search_max_out == 400, "rt40 sola_search_max_out == 400");
        CHECK(rt40.decoder_out_samples == 12000, "rt40 decoder_out_samples == 12000");
        CHECK(rt40.output_hop == 9600, "rt40 output_hop == 9600");

        const auto rt32 = derive_realtime_geometry(32000);
        CHECK(rt32.output_frame_samples == 640, "rt32 output_frame_samples == 640");
        CHECK(rt32.decoder_out_samples == 9600, "rt32 decoder_out_samples == 9600");
        CHECK(rt32.output_hop == 7680, "rt32 output_hop == 7680");
    }

    if (g_fail == 0) std::printf("全部通过\n");
    return g_fail == 0 ? 0 : 1;
}