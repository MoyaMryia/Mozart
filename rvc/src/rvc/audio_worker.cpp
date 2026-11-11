// audio_worker.cpp — 帧泵外层实现
// 搬运自 Mozart-Old/rvc-backend/src/rvc/audio_worker.cpp，改动：
//   - C-ABI 流句柄（mozart_io_*）换成 transport/audio_stream.h 的 C++ 接口；
//   - 裁掉静音/旁路（SPEC-03：归音频进程）、VAD/端到端延迟统计（归 Rust）；
//   - 预设选择增加 kind=realtime/file 入口参数；
//   - mock 管线直通分支保留（无 GPU 单测驱动全泵）。

#include "rvc/audio_worker.hpp"

#include <algorithm>
#include <chrono>
#include <cstring>
#include <stdexcept>
#include <utility>

#include <spdlog/spdlog.h>

namespace rvc {

namespace {
// 16k → 48k 线性插值升采样（960 = 3 × 320）：mock 管线直通用
void upsample_16k_to_48k(const float* in, float* out) {
    for (uint32_t j = 0; j < MOZART_OUTPUT_SAMPLES; ++j) {
        const uint32_t i = j / 3;
        const float frac = static_cast<float>(j % 3) / 3.0f;
        const float s0 = in[i];
        const float s1 = (i + 1 < MOZART_INPUT_SAMPLES) ? in[i + 1] : s0;
        out[j] = s0 + (s1 - s0) * frac;
    }
}
} // namespace

AudioWorker::AudioWorker(RealTimeAudioStream& stream,
                         RVCPipelineBase& pipeline,
                         Config config)
    : stream_(stream), pipeline_(pipeline), config_(std::move(config))
{}

AudioWorker::~AudioWorker() {
    stop();
}

void AudioWorker::start() {
    if (running_.exchange(true)) return;
    if (worker_thread_.joinable()) worker_thread_.join();
    // 上一次的推理线程可能仍在退出过程中并引用旧 streaming_，先收尾。
    if (inference_thread_.joinable()) inference_thread_.join();
    if (!stream_.IsOpen()) {
        running_ = false;
        throw std::runtime_error("audio stream must be open before AudioWorker::start");
    }

    stream_mode_ = !pipeline_.is_mock();
    if (stream_mode_) {
        StreamingRvc::Config scfg;
        scfg.skip_silence = config_.skip_silence;
        if (config_.realtime_kind) {
            // 实时短块 preset（旧 AudioWorker.cpp:62-70）：240 ms 窗、
            // 50 ms 交叉淡化、2.5 s 过去上下文、10 ms SOLA 搜索、
            // 400 ms 启动缓冲；能力由 StreamSession 入口校验。
            scfg.upstream_realtime = pipeline_.supports_realtime_streaming();
            scfg.window_samples = 3840;
            scfg.crossfade_out = 2400;
            scfg.past_context_samples = 40000;
            scfg.sola_search_out = 480;
            scfg.startup_buffer_blocks = 1;
            scfg.startup_delay_out =
                config_.realtime_playout_delay_ms * (MOZART_OUTPUT_SAMPLE_RATE / 1000);
            pipeline_.reset_realtime();
        } else {
            // file 种类：动态 Generator 走 quality/full-history，否则 2s 滑窗。
            scfg.full_history = pipeline_.supports_quality_streaming();
            if (scfg.full_history) {
                scfg.right_context_samples = 32000;
                scfg.guard_samples = 80;
                scfg.startup_buffer_blocks = 3;
            }
        }
        streaming_ = std::make_unique<StreamingRvc>(scfg);
        inference_thread_ = std::thread([this] {
            streaming_->inference_loop(pipeline_, running_);
        });
        spdlog::info("AudioWorker stream mode: {}",
            config_.realtime_kind
                ? "upstream realtime (240ms block + 2.5s past + SOLA)"
                : (scfg.full_history
                    ? "Golden quality profile (full history + 2s lookahead + 2 hop reserve)"
                    : "sliding window (2s) + 60ms crossfade"));
    } else {
        spdlog::info("AudioWorker frame mode (mock pipeline)");
    }
    worker_thread_ = std::thread(&AudioWorker::process_loop, this);
}

void AudioWorker::stop() {
    const bool was_running = running_.exchange(false);
    // Closing the stream releases a blocking ReadFrame.
    if (was_running) stream_.Close();
    if (inference_thread_.joinable()) inference_thread_.join();
    if (worker_thread_.joinable()) worker_thread_.join();
    streaming_.reset();
}

void AudioWorker::reset_stream() {
    if (streaming_) streaming_->reset();
}

void AudioWorker::set_last_error(const std::string& message) {
    std::lock_guard<std::mutex> lock(stats_mutex_);
    if (last_error_ != message) {
        last_error_ = message;
        spdlog::warn("AudioWorker: {}", message);
    }
}

void AudioWorker::process_loop() {
    spdlog::info("AudioWorker started: {}Hz -> {}Hz, frame={}ms",
                 MOZART_INPUT_SAMPLE_RATE, MOZART_OUTPUT_SAMPLE_RATE,
                 MOZART_INPUT_FRAME_MS);

    while (running_.load()) {
        mozart_input_frame_t input{};
        // ReadFrame 阻塞至帧到达；false 只在流关停时出现。
        if (!stream_.ReadFrame(&input, sizeof(input))) break;
        {
            std::lock_guard<std::mutex> lock(stats_mutex_);
            ++frames_in_;
        }

        mozart_output_frame_t output{};
        if (stream_mode_) {
            output.meta = input.meta;
            streaming_->push(input);
            const size_t got =
                streaming_->pop_output(output.pcm, MOZART_OUTPUT_SAMPLES);
            if (got < MOZART_OUTPUT_SAMPLES) {
                std::fill(output.pcm + got, output.pcm + MOZART_OUTPUT_SAMPLES, 0.0f);
            }
            std::lock_guard<std::mutex> lock(stats_mutex_);
            if (got < MOZART_OUTPUT_SAMPLES) {
                ++underrun_count_;
                if (!stream_output_started_) ++startup_underrun_count_;
            }
            if (got > 0) stream_output_started_ = true;
        } else {
            process_frame(input, output);
        }
        if (!stream_.WriteFrame(&output, sizeof(output))) {
            set_last_error("output frame write failed");
        }
        {
            std::lock_guard<std::mutex> lock(stats_mutex_);
            ++frames_out_;
        }
    }

    running_ = false;
    spdlog::info("AudioWorker stopped");
}


void AudioWorker::process_frame(const mozart_input_frame_t& input,
                                mozart_output_frame_t& output) {
    output.meta = input.meta;

    if (config_.skip_silence && input.meta.vad_flag == 0) {
        std::memset(output.pcm, 0, sizeof(output.pcm));
        return;
    }

    // 直通（mock）路径为测试保留：上采样即可，不构造逐帧容器。
    upsample_16k_to_48k(input.pcm, output.pcm);
}

AudioWorker::StreamStats AudioWorker::get_stream_stats() const {
    StreamStats s;
    if (streaming_) {
        const auto& src = streaming_->stats();
        s.blocks = src.blocks.load();
        s.skipped_blocks = src.skipped_blocks.load();
        s.resets = src.resets.load();
        s.late_blocks = src.late_blocks.load();
        s.input_overruns = src.input_overruns.load();
        s.output_overruns = src.output_overruns.load();
        s.inference_errors = src.inference_errors.load();
        s.history_resets = src.history_resets.load();
        s.queue_depth_frames =
            streaming_->output_pending() / MOZART_OUTPUT_SAMPLES;
    }
    std::lock_guard<std::mutex> lock(stats_mutex_);
    s.frames_in = frames_in_;
    s.frames_out = frames_out_;
    s.output_underruns = underrun_count_;
    s.startup_output_underruns = startup_underrun_count_;
    s.last_error = last_error_;
    return s;
}

} // namespace rvc
