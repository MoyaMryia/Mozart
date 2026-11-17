// test_rvc_realtime_params.cpp — 实时参数契约对拍（JSON 单一来源 ↔ C++ 镜像）
// ============================================================================
// 运行时读 tools/model/realtime_chain_params.json（路径由 tests/CMakeLists.txt
// 的 MOZART_REALTIME_PARAMS_JSON 注入），逐项比对 realtime_geometry.hpp 的常量，
// 再用 JSON 数值独立重算几何与 derive_realtime_geometry() 对拍。
// 任一侧被改歪即失败：常量比对拦镜像漂移，重算拦公式漂移。
// 黄金值与 HuBERT 卷积栈 oracle 断言留在 test_rvc_window_geometry.cpp。
#include "rvc/realtime_geometry.hpp"

#include <nlohmann/json.hpp>

#include <cstdint>
#include <cstdio>
#include <fstream>
#include <string>

using namespace rvc;

namespace {

int g_fail = 0;
#define CHECK(cond, msg) do { \
    if (cond) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s\n", msg); ++g_fail; } } while (0)

// 数值比对：失败时打印两侧实际值，直接指出漂移方向。
#define CHECK_NUM(actual, expected, msg) do { \
    const unsigned long long a_ = static_cast<unsigned long long>(actual); \
    const unsigned long long e_ = static_cast<unsigned long long>(expected); \
    if (a_ == e_) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s（C++ %llu，期望 %llu）\n", msg, a_, e_); ++g_fail; } } while (0)

uint64_t num(const nlohmann::json& value) {
    return value.get<uint64_t>();
}

}  // namespace

int main() {
    std::printf("test_rvc_realtime_params\n");

    const std::string path = MOZART_REALTIME_PARAMS_JSON;
    std::ifstream file(path);
    if (!file.good()) {
        std::printf("  FAIL 读不到单一来源 %s\n", path.c_str());
        return 1;
    }
    const nlohmann::json params = nlohmann::json::parse(file, nullptr, false);
    if (params.is_discarded() || !params.contains("model_contract")
        || !params.contains("geometry")) {
        std::printf("  FAIL %s 不是合法的实时参数契约\n", path.c_str());
        return 1;
    }
    const nlohmann::json& mc = params["model_contract"];
    const nlohmann::json& g = params["geometry"];

    // ---- 1. model_contract：只读训练常量镜像 ----
    std::printf("model_contract mirrors the JSON\n");
    CHECK_NUM(kRealtimeInputSampleRate, num(mc["input_sample_rate"]),
              "input_sample_rate");
    CHECK_NUM(kRealtimeContractOutputSampleRate, num(mc["output_sample_rate"]),
              "contract output_sample_rate");
    CHECK_NUM(kRealtimeFeatureHop, num(mc["feature_hop"]), "feature_hop");
    CHECK_NUM(kRealtimeFeatureFrameRate, num(mc["feature_frame_rate"]),
              "feature_frame_rate");
    CHECK_NUM(kRealtimeRmvpeMelBands, num(mc["rmvpe_mel_bands"]), "rmvpe_mel_bands");
    // HuBERT hop 由输入采样率与 HuBERT 帧率推导，不另写字面量。
    CHECK_NUM(kRealtimeHubertHop,
              num(mc["input_sample_rate"]) / num(mc["hubert_frame_rate"]),
              "hubert hop == input_sample_rate / hubert_frame_rate");
    CHECK_NUM(kRealtimeOutputFrameRate, 50,
              "契约输出帧率 50 Hz（frame.h 的 20 ms 帧）");

    // ---- 2. geometry：形状常量与旋钮默认值/上限 ----
    std::printf("geometry mirrors the JSON\n");
    CHECK_NUM(kRealtimeWindowSamples, num(g["window_samples"]), "window_samples");
    CHECK_NUM(kRealtimePastContextSamples, num(g["past_context_samples"]),
              "past_context_samples");
    CHECK_NUM(kRealtimeCrossfadeMaxOut48k, num(g["crossfade_max_out"]),
              "crossfade_max_out");
    CHECK_NUM(kRealtimeSolaSearchMaxOut48k, num(g["sola_search_max_out"]),
              "sola_search_max_out");
    CHECK_NUM(kRealtimeDefaultCrossfadeMs, num(g["crossfade_ms"]), "crossfade_ms 默认");
    CHECK_NUM(kRealtimeDefaultSolaSearchMs, num(g["sola_search_ms"]),
              "sola_search_ms 默认");
    CHECK_NUM(kRealtimeDefaultStartupDelayMs, num(g["startup_delay_ms"]),
              "startup_delay_ms 默认");
    CHECK_NUM(kRealtimeStartupBufferBlocks, num(g["startup_buffer_blocks"]),
              "startup_buffer_blocks");
    CHECK(kRealtimeDefaultSkipSilence == g["skip_silence"].get<bool>(),
          "skip_silence 默认");

    const uint64_t samples_per_ms = num(mc["output_sample_rate"]) / 1000;
    CHECK_NUM(kRealtimeMaxCrossfadeMs, num(g["crossfade_max_out"]) / samples_per_ms,
              "crossfade_ms 上限由 crossfade_max_out 换算");
    CHECK_NUM(kRealtimeMaxSolaSearchMs, num(g["sola_search_max_out"]) / samples_per_ms,
              "sola_search_ms 上限由 sola_search_max_out 换算");
    CHECK_NUM(kRealtimeMaxOverlapMs,
              (num(g["crossfade_max_out"]) + num(g["sola_search_max_out"])) / samples_per_ms,
              "overlap 上限由两者之和换算");
    CHECK_NUM(kRealtimeMaxStartupDelayMs, num(g["startup_delay_max_ms"]),
              "startup_delay_ms 上限");

    // ---- 3. 整除模数：由帧率推导，与 JSON 契约一致 ----
    std::printf("knob quanta follow the frame grids\n");
    CHECK_NUM(kRealtimeOverlapMsQuantum, 1000 / num(mc["feature_frame_rate"]),
              "overlap 模数 == 模型帧周期");
    CHECK_NUM(kRealtimeStartupMsQuantum, MOZART_OUTPUT_FRAME_MS,
              "startup 模数 == 契约帧周期");

    // ---- 4. 用 JSON 数值独立重算几何，与推导函数逐字段对拍 ----
    // 辅助训练常量（HuBERT 感受野、RMVPE 块帧数、f0 右余量、decoder_head）
    // 的黄金锚点在 test_rvc_window_geometry.cpp（44800→139 与卷积栈 oracle）。
    const uint64_t in_sr = num(mc["input_sample_rate"]);
    const uint64_t contract_sr = num(mc["output_sample_rate"]);
    const uint64_t hop = num(mc["feature_hop"]);
    const uint64_t frame_rate = num(mc["feature_frame_rate"]);
    const uint64_t w = num(g["window_samples"]);
    const uint64_t p = num(g["past_context_samples"]);
    const uint64_t c_ref = num(g["crossfade_max_out"]);
    const uint64_t s_ref = num(g["sola_search_max_out"]);
    const uint64_t margin = (c_ref + s_ref) * in_sr / contract_sr;
    const uint64_t front = (p + w + margin) / hop;
    const uint64_t skip = p / hop;
    const uint64_t ret = (w + margin) / hop;
    const uint64_t hubert_samples = front * hop;
    const uint64_t hubert_frames =
        (hubert_samples - kRealtimeHubertReceptive) / kRealtimeHubertHop + 1;
    const uint64_t flow_head = skip > 24 ? skip - 24 : 0;
    const uint64_t f0_samples = w + 800;
    const uint64_t rmvpe_block = kRealtimeRmvpeBlockFrames * hop;
    const uint64_t rmvpe_audio =
        rmvpe_block * ((f0_samples - 1) / rmvpe_block + 1) - hop;
    const uint64_t rmvpe_mel_frames = rmvpe_audio / hop + 1;
    const uint64_t rmvpe_frames =
        ((rmvpe_mel_frames + kRealtimeRmvpeBlockFrames - 1) / kRealtimeRmvpeBlockFrames)
        * kRealtimeRmvpeBlockFrames;

    for (uint32_t sr : {32000u, 40000u, 48000u}) {
        std::printf("derive_realtime_geometry(%u) matches JSON-derived shapes\n",
                    static_cast<unsigned>(sr));
        const auto geom = derive_realtime_geometry(sr);
        const uint64_t crossfade_max = c_ref * sr / contract_sr;
        const uint64_t sola_max = s_ref * sr / contract_sr;
        CHECK_NUM(geom.input_sample_rate, in_sr, "input_sample_rate");
        CHECK_NUM(geom.output_sample_rate, sr, "output_sample_rate");
        CHECK_NUM(geom.window_samples, w, "window_samples");
        CHECK_NUM(geom.past_context_samples, p, "past_context_samples");
        CHECK_NUM(geom.crossfade_max_out, crossfade_max, "crossfade_max_out 按采样率缩放");
        CHECK_NUM(geom.sola_search_max_out, sola_max, "sola_search_max_out 按采样率缩放");
        CHECK_NUM(geom.overlap_out, crossfade_max + sola_max, "overlap_out");
        CHECK_NUM(geom.margin_16k, margin, "margin_16k");
        CHECK_NUM(geom.front_frames, front, "front_frames");
        CHECK_NUM(geom.skip_head, skip, "skip_head");
        CHECK_NUM(geom.return_frames, ret, "return_frames");
        CHECK_NUM(geom.decoder_frames, ret, "decoder_frames == return_frames");
        CHECK_NUM(geom.hubert_samples, hubert_samples, "hubert_samples");
        CHECK_NUM(geom.hubert_frames, hubert_frames, "hubert_frames");
        CHECK_NUM(geom.model_frame_out, sr / frame_rate, "model_frame_out");
        CHECK_NUM(geom.output_frame_samples, sr / (1000 / MOZART_OUTPUT_FRAME_MS),
                  "output_frame_samples");
        CHECK_NUM(geom.decoder_out_samples, ret * (sr / frame_rate), "decoder_out_samples");
        CHECK_NUM(geom.output_hop, w * sr / in_sr, "output_hop");
        CHECK_NUM(geom.flow_head, flow_head, "flow_head");
        CHECK_NUM(geom.latent_frames, front - flow_head, "latent_frames");
        CHECK_NUM(geom.decoder_head, skip - flow_head, "decoder_head");
        CHECK_NUM(geom.rmvpe_frames, rmvpe_frames, "rmvpe_frames");
        CHECK(geom.return_frames <= geom.latent_frames, "return <= latent");
    }

    // ---- 5. 入口校验器：出厂默认合法，越界/overlap 偏离定值/违反整除一律拒绝 ----
    std::printf("validate_realtime_knobs accepts the shipped defaults\n");
    CHECK(validate_realtime_knobs(kRealtimeDefaultCrossfadeMs,
                                  kRealtimeDefaultSolaSearchMs,
                                  kRealtimeDefaultStartupDelayMs).empty(),
          "JSON 默认旋钮合法");
    CHECK(validate_realtime_knobs(num(g["crossfade_ms"]), num(g["sola_search_ms"]),
                                  num(g["startup_delay_ms"])).empty(),
          "JSON 原始数值合法");
    CHECK(validate_realtime_knobs(kRealtimeMaxCrossfadeMs, kRealtimeMaxSolaSearchMs,
                                  kRealtimeMaxStartupDelayMs).empty(),
          "上限组合即锁定的 overlap 定值");

    // overlap 锁定只针对块几何：startup_delay_ms 不参与形状，整个合法范围仍可调。
    std::printf("startup_delay_ms stays adjustable while overlap is locked\n");
    for (uint64_t delay : {0ull, 20ull, 200ull, 400ull, 15000ull}) {
        const std::string label = "startup_delay_ms=" + std::to_string(delay);
        CHECK(validate_realtime_knobs(kRealtimeMaxCrossfadeMs, kRealtimeMaxSolaSearchMs,
                                      delay).empty(),
              (label + " 合法且被接受").c_str());
    }

    std::printf("validate_realtime_knobs rejects out-of-range and off-grid values\n");
    CHECK(!validate_realtime_knobs(kRealtimeMaxCrossfadeMs + 1, 0, 0).empty(),
          "crossfade 超上限拒绝");
    CHECK(!validate_realtime_knobs(0, kRealtimeMaxSolaSearchMs + 1, 0).empty(),
          "sola 超上限拒绝");
    CHECK(!validate_realtime_knobs(0, 0, kRealtimeMaxStartupDelayMs + 1).empty(),
          "startup 超上限拒绝");
    CHECK(!validate_realtime_knobs(kRealtimeMaxCrossfadeMs, kRealtimeMaxSolaSearchMs,
                                   410).empty(),
          "startup 410 违反整除拒绝");
    CHECK(!validate_realtime_knobs(kRealtimeMaxCrossfadeMs, kRealtimeMaxSolaSearchMs,
                                   1).empty(),
          "startup 1 违反整除拒绝");
    CHECK(validate_realtime_knobs(kRealtimeMaxCrossfadeMs, kRealtimeMaxSolaSearchMs, 410)
              .find("multiple of") != std::string::npos,
          "startup 拒绝原因点明整除模数");
    // u64 原始值不得因窄化回绕成合法值。
    CHECK(!validate_realtime_knobs(0x1'0000'0032ull, 0, 0).empty(),
          "超 u32 的 crossfade 拒绝");
    CHECK(!validate_realtime_knobs(0, 0, 0xFFFF'FFFF'FFFF'FFFFull).empty(),
          "u64 上限的 startup 拒绝");

    // ---- 6. overlap 锁为引擎形状对应的定值：小于它分析窗帧数与资产不符 ----
    // 与 core/src/config.rs 的 rvc_realtime_overlap_is_locked_to_engine_shape 同口径。
    std::printf("overlap below the engine shape is rejected with an explicit reason\n");
    const uint64_t locked[][2] = {
        {40, 10}, {30, 10}, {45, 0}, {40, 5}, {0, 5}, {0, 0}, {50, 0}, {0, 10},
    };
    for (const auto& pair : locked) {
        const std::string label = std::to_string(pair[0]) + "+" + std::to_string(pair[1]);
        const std::string reason = validate_realtime_knobs(pair[0], pair[1], 400);
        CHECK(!reason.empty(), (label + " 偏离 overlap 定值被拒绝").c_str());
        CHECK(reason.find("must equal") != std::string::npos
                  && reason.find("fixed at the maximum overlap") != std::string::npos
                  && reason.find("not implemented") != std::string::npos,
              (label + " 拒绝原因说明引擎形状固定、可调 overlap 尚未实现").c_str());
    }

    if (g_fail == 0) std::printf("全部通过\n");
    return g_fail == 0 ? 0 : 1;
}