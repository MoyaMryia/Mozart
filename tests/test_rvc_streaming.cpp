// test_rvc_streaming.cpp — StreamingRvc 无框架单测
// ============================================================================
// 搬运自 Mozart-Old/rvc-backend/tests/test_streaming.cpp，改动：仅文件名与
// 帧契约头（frame_meta.h 已并入 core/include/mozart/frame.h）。
// 用确定性假管线（输入最近邻 3x 上采样）验证：
//   1. 块调度节奏：攒满 window 出首块，此后每 hop 一块
//   2. 逐样本连续性：流式输出 == 整段直接 3x 上采样（交叉淡化区域两块
//      共享同一段输入、数值相等，混合后无失真）
//   3. 静音窗跳过：VAD 全零窗口跳推理、输出等长静音
//   4. frame_idx 缺口触发重置且流恢复
//   5. 原生多采样率（40k/32k）：块节拍不变，输出帧与启动缓冲按模型采样率量化
#include "rvc/streaming_pipeline.hpp"
#include "rvc/realtime_geometry.hpp"

#include <cmath>
#include <cstdio>
#include <future>
#include <stdexcept>
#include <vector>

using namespace rvc;

namespace {

int g_fail = 0;
#define CHECK(cond, msg) do { \
    if (cond) printf("  ok  %s\n", msg); \
    else { printf("  FAIL %s\n", msg); g_fail++; } } while (0)

// 假管线：输入最近邻 3x 上采样（确定性，便于逐样本对拍）
class Upsample3Pipeline : public RVCPipelineBase {
public:
    std::vector<float> process(const std::vector<float>& audio) override {
        std::vector<float> out;
        out.reserve(audio.size() * 3);
        for (float s : audio) {
            out.push_back(s);
            out.push_back(s);
            out.push_back(s);
        }
        return out;
    }
    bool is_mock() const override { return false; } // 强制流式语义
};

class RealtimeTailPipeline : public Upsample3Pipeline {
public:
    std::vector<float> process_realtime(
        const std::vector<float>& audio, const RvcRealtimeRequest& request
    ) override {
        ++calls;
        last_input_samples = audio.size();
        last_request = request;
        return RVCPipelineBase::process_realtime(audio, request);
    }

    size_t calls = 0;
    size_t last_input_samples = 0;
    RvcRealtimeRequest last_request;
};

mozart_input_frame_t make_frame(uint32_t idx, uint8_t vad, float value,
                                uint8_t segment = 1, uint64_t pts_ns = 0)
{
    mozart_input_frame_t f{};
    f.meta.frame_idx = idx;
    f.meta.vad_flag = vad;
    f.meta.segment_id = segment;
    f.meta.pts_ns = pts_ns;
    for (int i = 0; i < MOZART_INPUT_SAMPLES; ++i) f.pcm[i] = value;
    return f;
}

// Inject the reset from inside process_realtime, at the same boundary where
// the inference thread runs without the generation lock.
class ResetDuringInference : public Upsample3Pipeline {
public:
    ResetDuringInference(StreamingRvc& stream, int outcome)
        : stream_(stream), outcome_(outcome) {}

    std::vector<float> process_realtime(
        const std::vector<float>&, const RvcRealtimeRequest& request
    ) override {
        if (outcome_ >= 0) {
            stream_.push(make_frame(1000, 1, 0.25f));
            const int outcome = outcome_;
            outcome_ = -1;
            if (outcome == 0) throw std::runtime_error("old-inference test failure");
            if (outcome == 1) return {0.5f};
        }
        return std::vector<float>(request.output_samples, 0.75f);
    }

    void reset_realtime() override { ++model_resets; }
    unsigned model_resets = 0;

private:
    StreamingRvc& stream_;
    int outcome_;
};

class SynchronizedReset : public Upsample3Pipeline {
public:
    explicit SynchronizedReset(std::future<void> proceed) : proceed_(std::move(proceed)) {}

    void reset_realtime() override {
        resetting.set_value();
        proceed_.wait();
    }

    std::promise<void> resetting;

private:
    std::future<void> proceed_;
};

StreamingRvc::Config realtime_config() {
    StreamingRvc::Config cfg;
    cfg.window_samples = 3840;
    cfg.crossfade_out = 2400;
    cfg.upstream_realtime = true;
    cfg.past_context_samples = 40000;
    cfg.sola_search_out = 480;
    return cfg;
}

std::vector<float> drain(StreamingRvc& s)
{
    std::vector<float> out;
    float buf[MOZART_OUTPUT_SAMPLES];
    for (;;) {
        const size_t got = s.pop_output(buf, MOZART_OUTPUT_SAMPLES);
        if (got == 0) break;
        out.insert(out.end(), buf, buf + got);
    }
    return out;
}

} // namespace

int main()
{
    // ---- 用例 1+2：块调度 + 逐样本连续性 ----
    printf("test_window_scheduling_and_continuity\n");
    {
        StreamingRvc::Config cfg;
        StreamingRvc s(cfg);
        Upsample3Pipeline pipe;

        const size_t kFrames = 500;
        std::vector<float> pushed;
        pushed.reserve(kFrames * MOZART_INPUT_SAMPLES);
        uint32_t idx = 1;
        for (size_t k = 0; k < kFrames; ++k) {
            const float v = std::sin(0.01f * static_cast<float>(k));
            mozart_input_frame_t f = make_frame(idx++, 1, v);
            s.push(f);
            pushed.insert(pushed.end(), f.pcm, f.pcm + MOZART_INPUT_SAMPLES);
            while (s.try_process_one(pipe)) {}
        }
        while (s.try_process_one(pipe)) {}

        const size_t blocks = s.stats().blocks.load();
        // 首块需 window=32000（100 帧）；此后每 hop=31040（97 帧）一块
        CHECK(blocks == 5, "block cadence: 5 blocks for 500 frames");
        CHECK(s.stats().input_overruns.load() == 0,
              "healthy rolling history is not reported as overrun");

        const std::vector<float> out = drain(s);
        const size_t emit_out = (cfg.window_samples - cfg.crossfade_out / 3) * 3;
        CHECK(out.size() == blocks * emit_out, "output == blocks * emit_out");

        // 逐样本对拍：out[j] 必须等于 pushed[j/3]（3x 上采样）
        double max_err = 0.0;
        const size_t check_n = std::min(out.size(), pushed.size() * 3);
        for (size_t j = 0; j < check_n; ++j) {
            const float ref = pushed[j / 3];
            max_err = std::max(max_err, static_cast<double>(std::fabs(out[j] - ref)));
        }
        CHECK(max_err < 1e-6, "streamed output matches direct 3x upsample");
    }

    // ---- 用例 3：静音窗跳过 ----
    printf("test_silence_skip\n");
    {
        StreamingRvc::Config cfg;
        StreamingRvc s(cfg);
        Upsample3Pipeline pipe;

        uint32_t idx = 1;
        for (size_t k = 0; k < 200; ++k) { // 200 帧全静音（vad=0）
            s.push(make_frame(idx++, 0, 0.0f));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(s.stats().blocks.load() == 0, "no inference on silence");
        CHECK(s.stats().skipped_blocks.load() == 2, "2 windows skipped");

        const std::vector<float> out = drain(s);
        bool all_zero = true;
        for (float v : out) all_zero = all_zero && v == 0.0f;
        CHECK(all_zero && out.size() == 2 * 93120, "silence in -> equal silence out");
    }

    // ---- 用例 4：frame_idx 缺口重置 ----
    printf("test_discontinuity_reset\n");
    {
        StreamingRvc::Config cfg;
        StreamingRvc s(cfg);
        Upsample3Pipeline pipe;

        uint32_t idx = 1;
        for (size_t k = 0; k < 150; ++k) { // 先建立流：150 帧 → 1 块
            s.push(make_frame(idx++, 1, 0.1f));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(s.stats().blocks.load() == 1, "first block before gap");
        drain(s);

        idx += 10; // 制造 10 帧缺口
        for (size_t k = 0; k < 150; ++k) {
            s.push(make_frame(idx++, 1, 0.1f));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(s.stats().resets.load() == 1, "gap triggers one reset");
        // 重置后重新攒满窗：再出 1 块
        CHECK(s.stats().blocks.load() == 2, "stream recovers after reset");
        const std::vector<float> out = drain(s);
        CHECK(out.size() == 93120, "post-reset output present");
    }

    printf("test_reset_rejects_old_success_short_and_failed_blocks\n");
    for (const int outcome : {0, 1, 2}) {
        StreamingRvc stream(realtime_config());
        ResetDuringInference pipe(stream, outcome);
        for (unsigned i = 0; i < 12; ++i) stream.push(make_frame(i, 1, 0.25f));
        CHECK(stream.try_process_one(pipe), "old block finishes after reset");
        CHECK(stream.stats().resets.load() == 1, "inference actually crossed reset");
        CHECK(stream.output_pending() == 0, "no old PCM or fallback silence reaches new queue");
        for (unsigned i = 1001; i < 1012; ++i) stream.push(make_frame(i, 1, 0.25f));
        CHECK(stream.try_process_one(pipe), "new block is processed");
        CHECK(pipe.model_resets == 1, "new inference starts after resetting model history");
        CHECK(stream.output_pending() == 11520, "new generation emits one full block");
    }

    printf("test_reset_rejects_a_concurrent_silence_skip\n");
    {
        StreamingRvc stream(realtime_config());
        std::promise<void> proceed;
        SynchronizedReset pipe(proceed.get_future());
        auto resetting = pipe.resetting.get_future();
        stream.reset(); // Force model cache reset before this generation's first block.
        for (unsigned i = 0; i < 12; ++i) stream.push(make_frame(i, 0, 0.0f));
        auto work = std::async(std::launch::async, [&] { return stream.try_process_one(pipe); });
        const bool parked = resetting.wait_for(std::chrono::seconds(2)) == std::future_status::ready;
        CHECK(parked, "inference picked the silent block before the next reset");
        if (parked) stream.push(make_frame(1000, 0, 0.0f));
        proceed.set_value();
        CHECK(work.get(), "inference worker returns");
        CHECK(stream.stats().resets.load() == 2, "input worker reset while inference was paused");
        CHECK(stream.output_pending() == 0, "stale skip cannot publish zeros after reset");
        CHECK(stream.stats().skipped_blocks.load() == 0, "stale skip does not count as a published block");
    }

    // ---- 用例 5：Golden full-history + lookahead 调度和 target crop ----
    printf("test_full_history_context_crop\n");
    {
        StreamingRvc::Config cfg;
        cfg.skip_silence = false;
        cfg.full_history = true;
        cfg.right_context_samples = 32000;
        cfg.guard_samples = 80;
        StreamingRvc s(cfg);
        Upsample3Pipeline pipe;

        constexpr size_t kFrames = 500;
        std::vector<float> pushed;
        pushed.reserve(kFrames * MOZART_INPUT_SAMPLES);
        for (size_t frame = 0; frame < kFrames; ++frame) {
            const float value = std::sin(0.01f * static_cast<float>(frame));
            const auto input = make_frame(
                static_cast<uint32_t>(frame + 1), 1, value
            );
            s.push(input);
            pushed.insert(
                pushed.end(), input.pcm, input.pcm + MOZART_INPUT_SAMPLES
            );
            while (s.try_process_one(pipe)) {}
        }

        const size_t blocks = s.stats().blocks.load();
        CHECK(blocks == 4, "full-history waits for 2s lookahead and guard");
        CHECK(s.stats().input_overruns.load() == 0,
              "full-history retains the complete prefix");
        const auto out = drain(s);
        const size_t emit = (cfg.window_samples - cfg.crossfade_out / 3) * 3;
        CHECK(out.size() == blocks * emit,
              "full-history emits one target hop per prefix");
        double max_err = 0.0;
        for (size_t sample = 0; sample < out.size(); ++sample) {
            max_err = std::max(max_err, static_cast<double>(
                std::fabs(out[sample] - pushed[sample / 3])
            ));
        }
        CHECK(max_err < 1e-6, "full-history crops the target timeline exactly");
    }

    // ---- 用例 6：quality 模式先积攒两块余量再开始播放 ----
    printf("test_quality_startup_buffer\n");
    {
        StreamingRvc::Config cfg;
        cfg.skip_silence = false;
        cfg.full_history = true;
        cfg.right_context_samples = 32000;
        cfg.guard_samples = 80;
        cfg.startup_buffer_blocks = 3;
        StreamingRvc s(cfg);
        Upsample3Pipeline pipe;

        uint32_t idx = 1;
        for (size_t frame = 0; frame < 201; ++frame) {
            s.push(make_frame(idx++, 1, 0.25f));
            while (s.try_process_one(pipe)) {}
        }
        float output[MOZART_OUTPUT_SAMPLES]{};
        CHECK(s.stats().blocks.load() == 1, "first quality block is ready");
        CHECK(s.pop_output(output, MOZART_OUTPUT_SAMPLES) == 0,
              "first block remains buffered");

        for (size_t frame = 0; frame < 97; ++frame) {
            s.push(make_frame(idx++, 1, 0.25f));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(s.stats().blocks.load() == 2, "second quality block is ready");
        CHECK(s.pop_output(output, MOZART_OUTPUT_SAMPLES) == 0,
              "second block remains buffered");

        for (size_t frame = 0; frame < 97; ++frame) {
            s.push(make_frame(idx++, 1, 0.25f));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(s.stats().blocks.load() == 3, "third quality block is ready");
        CHECK(s.pop_output(output, MOZART_OUTPUT_SAMPLES) == MOZART_OUTPUT_SAMPLES,
              "playback starts with two reserve blocks");
    }

    // ---- 用例 7：upstream realtime 只等一个短块，过去上下文补零 ----
    printf("test_upstream_realtime_short_block\n");
    {
        StreamingRvc::Config cfg;
        cfg.window_samples = 3840;       // 240 ms，匹配 20 ms IO 帧
        cfg.crossfade_out = 2400;        // 50 ms @48k
        cfg.upstream_realtime = true;
        cfg.past_context_samples = 40000; // 2.5 s，仅过去上下文
        cfg.sola_search_out = 480;       // 10 ms @48k
        cfg.skip_silence = false;
        StreamingRvc s(cfg);
        RealtimeTailPipeline pipe;

        uint32_t idx = 1;
        for (size_t frame = 0; frame < 11; ++frame) {
            s.push(make_frame(idx++, 1, 0.1f));
            CHECK(!s.try_process_one(pipe), "realtime waits until the 240ms block");
        }
        s.push(make_frame(idx++, 1, 0.1f));
        CHECK(s.try_process_one(pipe), "realtime starts after one 240ms block");
        CHECK(pipe.calls == 1, "one realtime inference call");
        CHECK(pipe.last_input_samples == 44800,
              "realtime input is 2.5s past + block + overlap + search");
        CHECK(pipe.last_request.block_samples_16k == 3840,
              "pipeline receives the explicit input block size");
        CHECK(pipe.last_request.skip_head_frames == 250,
              "pipeline receives the explicit past-frame crop");
        CHECK(pipe.last_request.return_frames == 30,
              "pipeline receives the explicit return-frame count");
        CHECK(pipe.last_request.output_samples == 14400,
              "pipeline returns block + overlap + SOLA search");
        auto first = drain(s);
        CHECK(first.size() == 11520, "realtime emits exactly one 240ms block");

        for (size_t frame = 0; frame < 12; ++frame) {
            s.push(make_frame(idx++, 1, 0.2f));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(pipe.calls == 2, "realtime cadence is one call per 240ms");
        const auto second = drain(s);
        CHECK(second.size() == 11520, "second realtime block has fixed length");
        bool finite = true;
        for (float value : second) finite = finite && std::isfinite(value);
        CHECK(finite, "SOLA output is finite");
        CHECK(s.stats().input_overruns.load() == 0,
              "rolling realtime history is pruned without overrun");
    }

    printf("test_realtime_playout_reserve_absorbs_compute_jitter\n");
    {
        StreamingRvc::Config base;
        base.window_samples = 3840;
        base.crossfade_out = 2400;
        base.upstream_realtime = true;
        base.skip_silence = true;
        auto buffered = base;
        buffered.startup_delay_out = 20 * MOZART_OUTPUT_SAMPLES;
        StreamingRvc immediate(base), reserved(buffered);
        RealtimeTailPipeline immediate_pipe, reserved_pipe;
        for (uint32_t i = 1; i <= 12; ++i) {
            immediate.push(make_frame(i, 0, 0.0f));
            reserved.push(make_frame(i, 0, 0.0f));
            immediate.pop_output(nullptr, MOZART_OUTPUT_SAMPLES);
            reserved.pop_output(nullptr, MOZART_OUTPUT_SAMPLES);
        }
        CHECK(immediate.try_process_one(immediate_pipe) && reserved.try_process_one(reserved_pipe),
              "first blocks are available before the synthetic compute delay");
        float output[MOZART_OUTPUT_SAMPLES]{};
        CHECK(reserved.pop_output(output, 0) == 0, "zero-length read does not consume reserve");
        std::vector<float> immediate_pcm, reserved_pcm;
        unsigned immediate_gaps = 0, reserved_gaps = 0, startup_frames = 0;
        bool immediate_started = false, reserved_started = false;
        for (uint32_t tick = 0; tick < 32; ++tick) {
            if (tick < 12) {
                immediate.push(make_frame(tick + 13, 1, .2f));
                reserved.push(make_frame(tick + 13, 1, .2f));
            }
            // 静音首块瞬时完成；有声块多用六帧，模拟 120 ms 的计算等待。
            if (tick == 18) {
                CHECK(immediate.try_process_one(immediate_pipe) && reserved.try_process_one(reserved_pipe),
                      "delayed producer finishes the second block");
            }
            const auto read = [&](StreamingRvc& stream, std::vector<float>& pcm, bool& started,
                                  unsigned& gaps, unsigned* startup) {
                const size_t got = stream.pop_output(output, MOZART_OUTPUT_SAMPLES);
                if (got) {
                    started = true;
                    pcm.insert(pcm.end(), output, output + got);
                } else if (!started) {
                    if (startup) ++*startup;
                } else if (pcm.size() < 2 * 11520) {
                    ++gaps;
                }
            };
            read(immediate, immediate_pcm, immediate_started, immediate_gaps, nullptr);
            read(reserved, reserved_pcm, reserved_started, reserved_gaps, &startup_frames);
        }
        CHECK(immediate_gaps == 6, "unbuffered path exposes the bounded jitter");
        CHECK(reserved_gaps == 0 && startup_frames == 8, "400 ms startup budget absorbs silence-to-speech compute jitter");
        CHECK(reserved_pcm == immediate_pcm && reserved_pcm.size() == 23040,
              "reserve preserves every emitted PCM sample and its order");
        for (uint32_t i = 1000; i < 1012; ++i) {
            reserved.push(make_frame(i, 1, .3f));
            CHECK(reserved.pop_output(output, MOZART_OUTPUT_SAMPLES) == 0, "reset waits for its input-time budget");
        }
        CHECK(reserved.try_process_one(reserved_pipe), "stream produces output after discontinuity");
        for (int i = 0; i < 8; ++i) {
            CHECK(reserved.pop_output(output, MOZART_OUTPUT_SAMPLES) == 0, "reset restores the startup reserve");
        }
        CHECK(reserved.pop_output(output, MOZART_OUTPUT_SAMPLES) == MOZART_OUTPUT_SAMPLES,
              "reset stream resumes after its reserve");
        buffered.startup_delay_out = 1;
        bool invalid = false;
        try { StreamingRvc rejected(buffered); }
        catch (const std::invalid_argument&) { invalid = true; }
        CHECK(invalid, "reserve rejects incomplete contract frames");
    }

    // ---- 用例 8：full-history 超过历史容量后不永久停摆 ----
    printf("test_full_history_capacity_reanchor\n");
    {
        StreamingRvc::Config cfg;
        cfg.skip_silence = false;
        cfg.full_history = true;
        cfg.window_samples = 32000;
        cfg.crossfade_out = 2880;
        cfg.right_context_samples = 0;
        cfg.guard_samples = 0;
        cfg.max_history_samples = 96000; // 允许 3 块后超容一次
        StreamingRvc s(cfg);
        Upsample3Pipeline pipe;

        uint32_t idx = 1;
        for (size_t frame = 0; frame < 400; ++frame) {
            s.push(make_frame(idx++, 1, 0.1f));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(s.stats().blocks.load() >= 3,
              "stream keeps producing after history capacity is exceeded");
        CHECK(s.stats().history_resets.load() >= 1,
              "history capacity triggers a re-anchor");
        const auto out = drain(s);
        CHECK(!out.empty(), "post-reanchor output present");
    }

    printf("test_segment_changes_preserve_pcm_timeline\n");
    {
        StreamingRvc s{StreamingRvc::Config{}};
        Upsample3Pipeline pipe;
        for (uint32_t i = 1; i <= 300; ++i) {
            s.push(make_frame(i, 1, .1f, static_cast<uint8_t>(i % 7), i * 20000000ull));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(s.stats().resets.load() == 0, "utterance boundaries preserve rolling context");
        CHECK(s.stats().blocks.load() == 3, "continuous PCM still produces scheduled blocks");
        s.push(make_frame(301, 1, .1f, 1, 1000000ull));
        CHECK(s.stats().resets.load() == 1, "backward clock triggers reset");
        s.push(make_frame(302, 1, .1f, 1, 2000000000ull));
        CHECK(s.stats().resets.load() == 1, "continuous frame_idx with forward pts jitter does not reset");
        for (uint32_t i = 303; i <= 405; ++i) {
            s.push(make_frame(i, 1, .1f, 1, 2000000000ull + (i - 302) * 20000000ull));
            while (s.try_process_one(pipe)) {}
        }
        CHECK(s.stats().resets.load() == 1 && s.stats().blocks.load() == 4,
              "stream stays continuous across forward pts jitter");
    }

    printf("test_native_40k_realtime_streaming\n");
    {
        struct Pipe40k : public RVCPipelineBase {
            std::vector<float> process(const std::vector<float>& audio) override {
                return std::vector<float>((audio.size() * 40000) / 16000, 0.25f);
            }
            std::vector<float> process_realtime(
                const std::vector<float>&,
                const RvcRealtimeRequest& request) override {
                return std::vector<float>(request.output_samples, 0.25f);
            }
        } pipe40k;
        StreamingRvc::Config cfg;
        cfg.output_sample_rate = 40000;
        cfg.upstream_realtime = true;
        cfg.window_samples = 3840;
        cfg.past_context_samples = 40000;
        cfg.crossfade_out = 2000;
        cfg.sola_search_out = 400;
        cfg.startup_buffer_blocks = 1;
        cfg.startup_delay_out = 400 * 40;
        StreamingRvc s(cfg);
        for (uint32_t i = 1; i <= 30; ++i) {
            s.push(make_frame(i, 1, 0.1f, 1, i * 20000000ull));
            while (s.try_process_one(pipe40k)) {}
        }
        CHECK(s.output_sample_rate() == 40000, "40k output_sample_rate");
        CHECK(s.output_frame_samples() == 800, "40k output_frame_samples == 800");
        CHECK(s.stats().blocks.load() >= 2, "40k stream produces blocks");
    }

    printf("test_native_32k_realtime_streaming\n");
    {
        // 32k 原生流式：几何由 realtime_geometry.hpp 推导（与 40k/48k 同一公式），
        // 块节拍仍是 240 ms，输出帧与启动缓冲按 32k 契约帧（640 样本）量化。
        struct Pipe32k : public RVCPipelineBase {
            std::vector<float> process(const std::vector<float>& audio) override {
                return std::vector<float>((audio.size() * 32000) / 16000, 0.25f);
            }
            std::vector<float> process_realtime(
                const std::vector<float>&, const RvcRealtimeRequest& request) override {
                ++calls;
                last_request = request;
                return std::vector<float>(request.output_samples, 0.25f);
            }
            size_t calls = 0;
            RvcRealtimeRequest last_request;
        } pipe32k;

        const auto geom = derive_realtime_geometry(32000);
        CHECK(geom.crossfade_max_out == 1600, "32k crossfade max == 1600");
        CHECK(geom.sola_search_max_out == 320, "32k sola max == 320");
        CHECK(geom.output_frame_samples == 640, "32k contract frame == 640 samples");
        CHECK(geom.output_hop == 7680, "32k output hop == 7680");
        CHECK(geom.decoder_out_samples == 9600, "32k decoder out == 9600");

        StreamingRvc::Config cfg;
        cfg.output_sample_rate = 32000;
        cfg.upstream_realtime = true;
        cfg.window_samples = geom.window_samples;
        cfg.past_context_samples = geom.past_context_samples;
        cfg.crossfade_out = geom.crossfade_max_out;
        cfg.sola_search_out = geom.sola_search_max_out;
        cfg.startup_buffer_blocks = 1;
        cfg.skip_silence = false;
        StreamingRvc s(cfg);
        CHECK(s.output_sample_rate() == 32000, "32k output_sample_rate");
        CHECK(s.output_frame_samples() == 640, "32k output_frame_samples == 640");

        uint32_t idx = 1;
        for (size_t frame = 0; frame < 11; ++frame) {
            s.push(make_frame(idx, 1, 0.1f, 1, idx * 20000000ull));
            ++idx;
            CHECK(!s.try_process_one(pipe32k), "32k realtime waits until the 240ms block");
        }
        s.push(make_frame(idx, 1, 0.1f, 1, idx * 20000000ull));
        ++idx;
        CHECK(s.try_process_one(pipe32k), "32k realtime starts after one 240ms block");
        CHECK(pipe32k.calls == 1, "one realtime inference call");
        CHECK(pipe32k.last_request.block_samples_16k == 3840,
              "32k keeps the 16k input block at 3840 samples");
        CHECK(pipe32k.last_request.skip_head_frames == 250,
              "32k keeps the 2.5s past-frame crop");
        CHECK(pipe32k.last_request.return_frames == 30,
              "32k keeps 30 return frames");
        CHECK(pipe32k.last_request.output_samples == 9600,
              "32k block returns 30 frames x 320 samples");
        const auto first = drain(s);
        CHECK(first.size() == 7680, "32k emits exactly one 240ms block");
        CHECK(first.size() % s.output_frame_samples() == 0,
              "32k emit is a whole number of contract frames");
        bool finite = true;
        for (float value : first) finite = finite && std::isfinite(value);
        CHECK(finite, "32k SOLA output is finite");

        for (size_t frame = 0; frame < 12; ++frame) {
            s.push(make_frame(idx, 1, 0.2f, 1, idx * 20000000ull));
            ++idx;
            while (s.try_process_one(pipe32k)) {}
        }
        CHECK(pipe32k.calls == 2, "32k cadence is one call per 240ms");
        const auto second = drain(s);
        CHECK(second.size() == 7680, "second 32k block has fixed length");
        CHECK(s.stats().input_overruns.load() == 0,
              "32k rolling history is pruned without overrun");
        CHECK(s.stats().resets.load() == 0, "32k continuous stream does not reset");

        // 启动缓冲按 32k 契约帧量化：400 ms = 12800 样本 = 20 帧 640。
        auto reserved_cfg = cfg;
        reserved_cfg.startup_delay_out = 400 * 32;
        StreamingRvc reserved(reserved_cfg);
        Pipe32k reserved_pipe;
        for (uint32_t i = 1; i <= 12; ++i) {
            reserved.push(make_frame(i, 1, 0.1f, 1, i * 20000000ull));
        }
        CHECK(reserved.try_process_one(reserved_pipe), "32k reserve block is ready");
        float output[MOZART_OUTPUT_SAMPLES]{};
        for (int i = 0; i < 20; ++i) {
            CHECK(reserved.pop_output(output, 640) == 0,
                  "32k startup budget holds 20 contract frames");
        }
        CHECK(reserved.pop_output(output, 640) == 640,
              "32k playback resumes after its 400ms reserve");
    }

    printf("test_block_boundary_param_and_skip_silence_update\n");
    {
        struct ParamSwitchRealtimePipeline : public RVCPipelineBase {
            std::vector<float> process(const std::vector<float>& audio) override {
                return std::vector<float>(audio.size() * 3, gain_.load());
            }
            std::vector<float> process_realtime(
                const std::vector<float>&, const RvcRealtimeRequest& request) override {
                ++calls;
                const float g = gain_.load();
                observed_gains.push_back(g);
                return std::vector<float>(request.output_samples, g);
            }
            bool is_mock() const override { return false; }
            void set_gain(float g) { gain_.store(g); }

            std::atomic<float> gain_{0.25f};
            size_t calls = 0;
            std::vector<float> observed_gains;
        } pipe;

        const auto geom = derive_realtime_geometry(48000);
        StreamingRvc::Config cfg;
        cfg.output_sample_rate = 48000;
        cfg.upstream_realtime = true;
        cfg.window_samples = geom.window_samples;
        cfg.past_context_samples = geom.past_context_samples;
        cfg.crossfade_out = geom.crossfade_max_out;
        cfg.sola_search_out = geom.sola_search_max_out;
        cfg.startup_buffer_blocks = 1;
        cfg.skip_silence = true;
        StreamingRvc s(cfg);

        uint32_t idx = 1;
        // 12 个静音帧：skip_silence=true 应跳过推理。
        for (size_t i = 0; i < 12; ++i) {
            s.push(make_frame(idx, 0, 0.0f, 1, idx * 20000000ull));
            ++idx;
            while (s.try_process_one(pipe)) {}
        }
        CHECK(pipe.calls == 0 && s.stats().skipped_blocks.load() == 1,
              "initial skip_silence skips silent block");
        drain(s);

        // 在流运行期间块边界关闭 skip_silence 并改参数：下一块立刻生效且无重置。
        s.set_skip_silence(false);
        pipe.set_gain(0.5f);
        for (size_t i = 0; i < 12; ++i) {
            s.push(make_frame(idx, 0, 0.0f, 1, idx * 20000000ull));
            ++idx;
            while (s.try_process_one(pipe)) {}
        }
        CHECK(pipe.calls == 1 && pipe.observed_gains.back() == 0.5f,
              "skip_silence=false and new param take effect on next block");
        const auto second_out = drain(s);
        CHECK(second_out.size() == geom.output_hop
              && std::fabs(second_out.back() - 0.5f) < 1e-6f,
              "emitted samples reflect updated block parameter");

        // 再次改回 skip_silence=true 与参数：有声块跑新参，静音块跳过，全程无重置。
        pipe.set_gain(0.75f);
        s.set_skip_silence(true);
        for (size_t i = 0; i < 12; ++i) {
            s.push(make_frame(idx, 1, 0.1f, 1, idx * 20000000ull));
            ++idx;
            while (s.try_process_one(pipe)) {}
        }
        CHECK(pipe.calls == 2 && pipe.observed_gains.back() == 0.75f,
              "voiced block uses latest parameter without restarting stream");
        drain(s);

        for (size_t i = 0; i < 12; ++i) {
            s.push(make_frame(idx, 0, 0.0f, 1, idx * 20000000ull));
            ++idx;
            while (s.try_process_one(pipe)) {}
        }
        CHECK(pipe.calls == 2 && s.stats().skipped_blocks.load() == 2
              && s.stats().resets.load() == 0,
              "re-enabled skip_silence skips next silent block with zero resets");
    }

    printf("test_skip_silence_fades_tail_and_advances_pitch_cache\n");
    {
        struct SilenceTrackPipeline : public RVCPipelineBase {
            std::vector<float> process(const std::vector<float>& audio) override {
                return std::vector<float>(audio.size() * 3, 0.8f);
            }
            std::vector<float> process_realtime(
                const std::vector<float>&, const RvcRealtimeRequest& request) override {
                return std::vector<float>(request.output_samples, 0.8f);
            }
            void advance_realtime_silence(size_t hop_frames) override {
                silence_advances.push_back(hop_frames);
            }
            std::vector<size_t> silence_advances;
        } pipe;

        StreamingRvc::Config cfg = realtime_config();
        cfg.skip_silence = true;
        StreamingRvc s(cfg);
        uint32_t idx = 1;
        // 1) Voiced block: emits 11520 samples ending at 0.8, saves tail_ = 0.8
        for (size_t i = 0; i < 12; ++i) s.push(make_frame(idx++, 1, 0.2f));
        CHECK(s.try_process_one(pipe), "voiced block succeeds");
        const auto b1 = drain(s);
        CHECK(b1.size() == 11520 && std::fabs(b1.back() - 0.8f) < 1e-6f, "voiced block ends at 0.8");

        // 2) Silent block: fades tail_ from 0.8 -> 0 over 2400 samples, advances pitch cache by 24
        for (size_t i = 0; i < 12; ++i) s.push(make_frame(idx++, 0, 0.0f));
        CHECK(s.try_process_one(pipe), "silent skip succeeds");
        CHECK(pipe.silence_advances.size() == 1 && pipe.silence_advances[0] == 24,
              "silence skip advances realtime pitch cache by 24 hop frames");
        const auto b2 = drain(s);
        CHECK(b2.size() == 11520, "silent skip emits full block length");
        CHECK(std::fabs(b2.front() - 0.8f) < 1e-5f, "silent block starts smoothly from tail_ without cliff");
        CHECK(std::fabs(b2[2399]) < 1e-6f && std::fabs(b2[2400]) < 1e-6f && std::fabs(b2.back()) < 1e-6f,
              "silent block reaches zero at crossfade end and stays zero");
        float max_step_b2 = std::fabs(b2.front() - b1.back());
        for (size_t i = 1; i < b2.size(); ++i) {
            max_step_b2 = std::max(max_step_b2, std::fabs(b2[i] - b2[i - 1]));
        }
        CHECK(max_step_b2 < 0.002f, "voiced->silent transition has no hard step");

        // 3) Resumed voiced block: smoothly fades in from 0 -> 0.8 over 2400 samples
        for (size_t i = 0; i < 12; ++i) s.push(make_frame(idx++, 1, 0.2f));
        CHECK(s.try_process_one(pipe), "resumed voiced block succeeds");
        const auto b3 = drain(s);
        CHECK(b3.size() == 11520 && std::fabs(b3.front()) < 1e-5f && std::fabs(b3.back() - 0.8f) < 1e-6f,
              "silent->voiced block fades in cleanly from 0 to 0.8");
        float max_step_b3 = std::fabs(b3.front() - b2.back());
        for (size_t i = 1; i < b3.size(); ++i) {
            max_step_b3 = std::max(max_step_b3, std::fabs(b3[i] - b3[i - 1]));
        }
        CHECK(max_step_b3 < 0.002f, "silent->voiced transition has no hard step");

        // 4) Direct RVCInferencer pitch cache advance test
        RVCInferencer inf(nullptr, nullptr);
        std::vector<int64_t> p(30, 120);
        std::vector<float> pf(30, 220.0f);
        inf.push_realtime_pitch_cache(p, pf, 24);
        const auto before = inf.realtime_state_snapshot();
        CHECK(before.pitch_cache.back() == 120 && std::fabs(before.pitchf_cache.back() - 220.0f) < 1e-5f,
              "pitch cache populated before silence advance");
        inf.advance_realtime_silence(24);
        const auto after = inf.realtime_state_snapshot();
        CHECK(after.pitch_cache.size() == before.pitch_cache.size(), "pitch cache size preserved");
        bool trailing_zero = true;
        for (size_t i = after.pitch_cache.size() - 24; i < after.pitch_cache.size(); ++i) {
            trailing_zero = trailing_zero && after.pitch_cache[i] == 0 && after.pitchf_cache[i] == 0.0f;
        }
        CHECK(trailing_zero && after.pitch_cache[after.pitch_cache.size() - 25] == 120,
              "advance_realtime_silence shifts pitch cache by exact 24 frames");
    }

    printf("test_realtime_excitation_grid_continuity_and_sola_floor\n");
    {
        RVCInferencer inf(nullptr, nullptr);
        constexpr size_t kChannels = 4;
        constexpr size_t kLatentFrames = 54;
        constexpr size_t kReturnFrames = 30;
        constexpr size_t kOutPerFrame = 480;
        constexpr size_t kSourceDim = 3;
        constexpr size_t kShift = 24;

        const auto e0 = inf.build_realtime_excitation(
            kChannels, kLatentFrames, kReturnFrames, kOutPerFrame, kSourceDim);
        std::vector<float> pitchf(kReturnFrames, 213.5f);
        inf.advance_realtime_excitation(pitchf, kShift);
        const auto e1 = inf.build_realtime_excitation(
            kChannels, kLatentFrames, kReturnFrames, kOutPerFrame, kSourceDim);

        // latent_noise overlap: block0 t=24..53 (abs 0..29) == block1 t=0..29 (abs 0..29)
        bool latent_match = true;
        for (size_t c = 0; c < kChannels; ++c) {
            for (size_t i = 0; i < kReturnFrames; ++i) {
                if (e0.latent_noise[c * kLatentFrames + (kShift + i)]
                    != e1.latent_noise[c * kLatentFrames + i]) {
                    latent_match = false;
                }
            }
        }
        CHECK(latent_match, "adjacent realtime blocks share identical latent_noise in overlap");

        // source_noise overlap: block0 frames 24..29 == block1 frames 0..5
        const size_t overlap_samples = (kReturnFrames - kShift) * kOutPerFrame * kSourceDim;
        const size_t shift_samples = kShift * kOutPerFrame * kSourceDim;
        bool source_match = true;
        for (size_t i = 0; i < overlap_samples; ++i) {
            if (e0.source_noise[shift_samples + i] != e1.source_noise[i]) {
                source_match = false;
            }
        }
        CHECK(source_match, "adjacent realtime blocks share identical source_noise in overlap");

        // source_phase continuity mod 1 for all harmonics
        const double expected_cycles = std::fmod(24.0 * 213.5 * 0.01, 1.0);
        bool phase_match = true;
        for (size_t h = 0; h < kSourceDim; ++h) {
            double expect = std::fmod(
                static_cast<double>(e0.source_phase[h])
                + expected_cycles * static_cast<double>(h + 1), 1.0);
            if (expect < 0.0) expect += 1.0;
            if (std::fabs(static_cast<double>(e1.source_phase[h]) - expect) > 1e-5) {
                phase_match = false;
            }
        }
        CHECK(phase_match, "source_phase advances continuously mod 1 across harmonics");

        inf.reset_realtime();
        const auto snap = inf.realtime_state_snapshot();
        CHECK(snap.frame_cursor == 0 && snap.phase_cycles == 0.0,
              "reset_realtime clears frame cursor and phase cycles");

        // Weak-signal SOLA locks offset to 0 instead of chasing sub-floor noise.
        struct WeakSolaPipe : public RVCPipelineBase {
            std::vector<float> process(const std::vector<float>& audio) override {
                return std::vector<float>(audio.size() * 3, 0.0f);
            }
            std::vector<float> process_realtime(
                const std::vector<float>&, const RvcRealtimeRequest& req) override {
                std::vector<float> out(req.output_samples, 0.0f);
                if (call++ == 0) {
                    // Quiet tail below 3e-3 floor with a distinct tiny peak at tail[100]
                    out[11520 + 100] = 5e-4f;
                } else {
                    // Block 2 has matching tiny peak shifted by +300 samples, plus strong marker at 6000
                    out[300 + 100] = 5e-4f;
                    out[6000] = 0.9f;
                }
                return out;
            }
            size_t call = 0;
        } weak_pipe;

        StreamingRvc::Config cfg = realtime_config();
        cfg.skip_silence = false;
        StreamingRvc s(cfg);
        uint32_t idx = 1;
        for (size_t i = 0; i < 12; ++i) s.push(make_frame(idx++, 1, 0.01f));
        CHECK(s.try_process_one(weak_pipe), "weak SOLA block 1 runs");
        drain(s);
        for (size_t i = 0; i < 12; ++i) s.push(make_frame(idx++, 1, 0.01f));
        CHECK(s.try_process_one(weak_pipe), "weak SOLA block 2 runs");
        const auto out2 = drain(s);
        CHECK(std::fabs(out2[6000] - 0.9f) < 1e-6f && std::fabs(out2[5700]) < 1e-6f,
              "weak-signal SOLA keeps offset == 0 without jumping to noise peak");
    }

    printf("test_realtime_protect_rms_mix_and_peak_limit\n");
    {
        // protect: 4 frames (2 voiced, 2 unvoiced), emb_dim=2, skip_head=0, index_start=0
        const std::vector<float> base_indexed = {
            1.0f, 1.0f,  // t=0 voiced
            1.0f, 1.0f,  // t=1 unvoiced
            1.0f, 1.0f,  // t=2 unvoiced
            1.0f, 1.0f,  // t=3 voiced
        };
        const std::vector<float> unindexed = {
            0.0f, 0.0f,  // src=0 (covers t=0,1)
            0.0f, 0.0f,  // src=1 (covers t=2,3)
        };
        const std::vector<float> pitchf = {200.0f, 0.0f, 0.0f, 220.0f};

        // protect = 0.5 -> disabled (boundary contract)
        auto f_bypass = base_indexed;
        RVCInferencer::apply_realtime_protect(f_bypass, unindexed, pitchf, 0, 0, 2, 2, 0.5f);
        CHECK(f_bypass == base_indexed, "protect=0.5 leaves indexed features unchanged");

        // protect = 0.3 -> unvoiced frames blend to 0.3, voiced stay 1.0
        auto f_blend = base_indexed;
        RVCInferencer::apply_realtime_protect(f_blend, unindexed, pitchf, 0, 0, 2, 2, 0.3f);
        CHECK(std::fabs(f_blend[0] - 1.0f) < 1e-6f && std::fabs(f_blend[2] - 0.3f) < 1e-6f
              && std::fabs(f_blend[4] - 0.3f) < 1e-6f && std::fabs(f_blend[6] - 1.0f) < 1e-6f,
              "protect=0.3 blends only unvoiced frames toward unindexed features");

        // rms_mix_rate = 1.0 -> bypass RMS scaling, still applies 0.99 peak limiter
        std::vector<float> in_16k(4800, 0.1f);
        std::vector<float> out_48k(14400, 1.5f);
        RVCInferencer::apply_realtime_rms_mix_and_limit(
            in_16k.data(), in_16k.size(), out_48k, 48000, 1.0f, 0.99f);
        CHECK(std::fabs(out_48k[7200] - 0.99f) < 1e-5f,
              "rms_mix_rate=1.0 skips RMS scaling and limits peak to 0.99");

        // rms_mix_rate = 0.0 -> output matches input RMS (0.1 vs 0.4 -> scaled down near 0.1)
        std::vector<float> out_scaled(14400, 0.4f);
        RVCInferencer::apply_realtime_rms_mix_and_limit(
            in_16k.data(), in_16k.size(), out_scaled, 48000, 0.0f, 0.99f);
        CHECK(std::fabs(out_scaled[7200] - 0.1f) < 0.01f,
              "rms_mix_rate=0.0 aligns output envelope to input RMS");
    }

    if (g_fail) {
        printf("\n%d test(s) FAILED\n", g_fail);
        return 1;
    }
    printf("\nall tests passed\n");
    return 0;
}
