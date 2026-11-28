// test_sculptor_intent.cpp — L2 意图塑形器意图信号移植对拍（SPEC-03 步骤 6）
// ============================================================================
// 1. 黄金夹具：tests/fixtures/sculptor/intent_golden.json 由
//    rvc-reference/prosody_sculptor_l2/features.py 的 extract_causal_intent_signals
//    生成（rng=20261128，64 帧，含无声/有声段），C++ 逐帧逐通道对拍。
// 2. 状态机语义：重置后首帧初始化（mean=本帧、variance=0 → z=0）、
//    delta_energy 首帧差分为 0、历史回看夹紧与越界拒绝。
// 3. cond 向量与 sculptor.json 校验：镜像 build_cond_vector，非法值拒绝。
#include "rvc/sculptor.hpp"

#include <nlohmann/json.hpp>

#include <cmath>
#include <cstdio>
#include <fstream>
#include <string>
#include <vector>

using namespace rvc;

namespace {

int g_fail = 0;

#define CHECK(cond, msg) do { \
    if (cond) { std::printf("  ok   %s\n", msg); } \
    else { std::printf("  FAIL %s\n", msg); ++g_fail; } } while (0)

struct Fixture {
    size_t frames = 0;
    std::vector<float> audio;
    std::vector<float> f0;
    std::vector<std::array<float, 4>> expected;
};

bool load_fixture(Fixture* out) {
    std::ifstream f(MOZART_SCULPTOR_FIXTURE_DIR "/intent_golden.json");
    if (!f) return false;
    try {
        nlohmann::json j;
        f >> j;
        out->frames = j.at("frames").get<size_t>();
        out->audio = j.at("audio_16k").get<std::vector<float>>();
        out->f0 = j.at("f0").get<std::vector<float>>();
        for (const auto& row : j.at("expected")) {
            out->expected.push_back(row.get<std::array<float, 4>>());
        }
    } catch (const std::exception&) {
        return false;
    }
    return out->audio.size() == out->frames * 160 && out->expected.size() == out->frames;
}

bool write_tmp_params(const char* content, std::string* path_out) {
    static const char* tmp = "/tmp/mozart-sculptor-test-params.json";
    FILE* f = std::fopen(tmp, "w");
    if (!f) return false;
    std::fputs(content, f);
    std::fclose(f);
    *path_out = tmp;
    return true;
}

} // namespace

int main() {
    Fixture fx;
    CHECK(load_fixture(&fx), "黄金夹具加载且长度一致");

    if (!fx.audio.empty()) {
        // 1. 黄金对拍：逐帧逐通道。
        IntentSignalState state;
        size_t mismatches = 0;
        double max_diff = 0.0;
        size_t bad_frame = 0, bad_channel = 0;
        float bad_got = 0.0f, bad_want = 0.0f;
        for (size_t t = 0; t < fx.frames; ++t) {
            const auto got = state.compute_frame(
                fx.audio.data(), fx.audio.size(), t * 160, fx.f0[t]);
            for (size_t c = 0; c < 4; ++c) {
                const double diff = std::fabs(static_cast<double>(got[c]) - fx.expected[t][c]);
                if (diff >= 1e-5) {
                    ++mismatches;
                    if (diff > max_diff) {
                        max_diff = diff;
                        bad_frame = t; bad_channel = c;
                        bad_got = got[c]; bad_want = fx.expected[t][c];
                    }
                }
            }
        }
        CHECK(mismatches == 0, "64 帧全通道与 features.py 对拍（容差 1e-5）");
        std::printf("  max_diff=%.3e (frame=%zu channel=%zu got=%.6f want=%.6f)\n",
                    max_diff, bad_frame, bad_channel, bad_got, bad_want);

        // 2. 确定性：同一状态对象 reset 后重跑逐位一致。
        auto run_all = [&fx] {
            IntentSignalState s;
            std::vector<std::array<float, 4>> out;
            for (size_t t = 0; t < fx.frames; ++t) {
                out.push_back(s.compute_frame(
                    fx.audio.data(), fx.audio.size(), t * 160, fx.f0[t]));
            }
            return out;
        };
        CHECK(run_all() == run_all(), "重置后重跑逐位一致（无状态漂移）");

        // 3. 首帧初始化语义：mean 取本帧 → z_log_energy=0，差分为 0。
        IntentSignalState first;
        const auto head = first.compute_frame(fx.audio.data(), fx.audio.size(), 0, fx.f0[0]);
        CHECK(head[0] == 0.0f, "首帧 z_log_energy == 0（初始化语义）");
        CHECK(head[2] == 0.0f, "首帧 delta_energy == 0");
        first.reset();
        CHECK(first.compute_frame(fx.audio.data(), fx.audio.size(), 0, fx.f0[0]) == head,
              "reset 后首帧与初次一致");
    }

    // 4. 历史回看：back=1 最近一帧；越界与 0 拒绝。
    {
        IntentSignalState s;
        s.push_history(1.0f);
        s.push_history(2.0f);
        bool ok = s.history_at(1) == 2.0f && s.history_at(2) == 1.0f;
        CHECK(ok, "history_at 回看取值正确");
        bool threw = false;
        try { (void)s.history_at(3); } catch (const std::invalid_argument&) { threw = true; }
        CHECK(threw, "history_at(3) 超出已存帧数拒绝");
        threw = false;
        try { (void)s.history_at(0); } catch (const std::invalid_argument&) { threw = true; }
        CHECK(threw, "history_at(0) 拒绝");
    }

    // 5. causal_rms 帧：手算锚点（窗内单个 2.0，其余为 0 → rms² = 4/640）。
    {
        std::vector<float> audio(700, 0.0f);
        audio[659] = 2.0f;  // 窗 [20, 659] 内单个 2.0
        const float rms = causal_rms_frame(audio.data(), 659);
        CHECK(std::fabs(rms * rms - 4.0f / 640.0f) < 1e-9f, "rms 窗内单点能量按 640 归一");
        for (size_t i = 0; i < 700; ++i) audio[i] = 1.0f;
        CHECK(std::fabs(causal_rms_frame(audio.data(), 699) - 1.0f) < 1e-6f,
              "rms 常量 1.0 → 1.0");
    }

    // 6. cond 向量镜像 build_cond_vector。
    {
        const SculptorParams m2f{1, 228.0f, 2.6f, 0.7f};
        const auto cond = m2f.cond_vector();
        CHECK(cond[0] == 1.0f
              && std::fabs(cond[1] - std::log(228.0f / 175.0f) / 0.35f) < 1e-6f
              && std::fabs(cond[2] - 2.6f / 6.0f) < 1e-6f
              && cond[3] == 0.7f, "M2F cond 向量逐项一致");
        const SculptorParams f2m{-1, 125.0f, 3.0f, 0.3f};
        const auto cond_b = f2m.cond_vector();
        CHECK(cond_b[0] == -1.0f
              && std::fabs(cond_b[1] - std::log(125.0f / 175.0f) / 0.35f) < 1e-6f,
              "F2M gender/目标 F0 归化一致");
    }

    // 7. sculptor.json 校验：合法接受、非法拒绝、缺失拒绝。
    {
        std::string path;
        bool ok = write_tmp_params(
            R"({"gender": 1, "target_f0": 228.0, "target_f0_std": 2.6, "softness": 0.7})",
            &path);
        const auto valid = ok ? SculptorParams::from_json(path) : std::nullopt;
        CHECK(valid.has_value() && valid->gender == 1 && valid->target_f0 == 228.0f,
              "合法 sculptor.json 解析");

        CHECK(SculptorParams::from_json("/nonexistent/sculptor.json") == std::nullopt,
              "缺失文件拒绝");
        const char* cases[] = {
            R"({"gender": 2, "target_f0": 228.0, "target_f0_std": 2.6, "softness": 0.7})",
            R"({"gender": 1, "target_f0": 0, "target_f0_std": 2.6, "softness": 0.7})",
            R"({"gender": 1, "target_f0": 228.0, "target_f0_std": 2.6, "softness": 1.5})",
            R"({"gender": 1, "target_f0": 228.0, "target_f0_std": 2.6})",
        };
        bool all_rejected = true;
        for (const char* body : cases) {
            if (!write_tmp_params(body, &path)) { all_rejected = false; break; }
            if (SculptorParams::from_json(path).has_value()) { all_rejected = false; break; }
        }
        CHECK(all_rejected, "非法/缺字段 sculptor.json 全部拒绝");
    }

    std::printf("%s（fail=%d）\n", g_fail == 0 ? "ALL PASS" : "FAILED", g_fail);
    return g_fail == 0 ? 0 : 1;
}
