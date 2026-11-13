// file_worker.cpp — 文件整段批处理外层实现（见头文件说明）
// 结构对齐 audio_worker.cpp：IO 与推理同线程顺序执行（整段批处理无实时
// 节拍约束），停止次序沿用 close stream（解除阻塞）→ join。

#include "rvc/file_worker.hpp"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <stdexcept>
#include <utility>
#include <vector>

#include <spdlog/spdlog.h>

namespace rvc {

FileWorker::FileWorker(RealTimeAudioStream& stream, RVCPipelineBase& pipeline,
                       Config config)
    : stream_(stream), pipeline_(pipeline), config_(config)
{}

FileWorker::~FileWorker() {
    stop();
}

void FileWorker::start() {
    if (running_.exchange(true)) return;
    if (thread_.joinable()) thread_.join();
    if (!stream_.IsOpen()) {
        running_ = false;
        throw std::runtime_error("audio stream must be open before FileWorker::start");
    }
    cancelled_.store(false);
    thread_ = std::thread(&FileWorker::batch_loop, this);
}

void FileWorker::stop() {
    // 取消先行：推理检查点据此抛 InferenceCancelled；关流解除 ReadFrame
    // 与阻塞发送的等待（幂等，批处理自然结束后也交接数据面），join 等
    // worker 收尾（StopStream 在此返回）。
    cancelled_.store(true);
    running_.store(false);
    stream_.Close();
    if (thread_.joinable()) thread_.join();
}

void FileWorker::set_last_error(const std::string& message) {
    std::lock_guard<std::mutex> lock(stats_mutex_);
    if (last_error_ != message) {
        last_error_ = message;
        spdlog::warn("FileWorker: {}", message);
    }
}

void FileWorker::batch_loop() {
    spdlog::info("FileWorker started: total_frames={} ({}ms per frame)",
                 config_.total_frames, MOZART_INPUT_FRAME_MS);
    pipeline_.reset_offline_window_count();   // 批处理开始清零窗计数

    uint64_t frames_in = 0;
    uint64_t frames_out = 0;
    size_t total_out = 0;
    // 整段批处理在 worker 线程内收尾所有异常：取消正常退出，其余记入
    // last_error 供 GetStatus 查询，不跨线程/进程边界（SPEC-03）。
    try {
        // ---- 攒帧：320 样本输入帧拼成 16 kHz 整段，每帧查取消 ----
        std::vector<float> audio;
        audio.reserve(static_cast<size_t>(config_.total_frames) * MOZART_INPUT_SAMPLES);
        while (running_.load() && frames_in < config_.total_frames) {
            mozart_input_frame_t input{};
            // ReadFrame 阻塞至帧到达；false 只在流关停（StopStream）时出现。
            if (!stream_.ReadFrame(&input, sizeof(input))) break;
            audio.insert(audio.end(), input.pcm, input.pcm + MOZART_INPUT_SAMPLES);
            ++frames_in;
            std::lock_guard<std::mutex> lock(stats_mutex_);
            frames_in_ = frames_in;
        }
        if (!running_.load() || frames_in < config_.total_frames) {
            running_ = false;
            spdlog::info("FileWorker stopped while feeding ({}/{} frames)",
                         frames_in, config_.total_frames);
            return;
        }

        // ---- 裁掉尾帧补零，恢复真实音频长度 ----
        // 解码按 320 样本契约帧向上取整补零；这点零尾经 HuBERT 全局注意力会
        // 污染整段特征。total_samples 为解码真实采样数（0=不裁，向后兼容）。
        if (config_.total_samples > 0 && config_.total_samples < audio.size()) {
            audio.resize(static_cast<size_t>(config_.total_samples));
        }

        // ---- 整段离线推理一次（现有离线路径，检查点响应取消）----
        const auto began = std::chrono::steady_clock::now();
        auto output = pipeline_.process_cancellable(audio, cancelled_);
        const double elapsed = std::chrono::duration<double, std::milli>(
            std::chrono::steady_clock::now() - began).count();
        {
            std::lock_guard<std::mutex> lock(stats_mutex_);
            inference_ms_ = elapsed;
        }

        // ---- 回帧：48 kHz 整段切 960 样本输出帧，阻塞逐帧发，不丢 ----
        // meta.frame_idx 从 0 顺序；pts_ns 按输出帧节拍推进；vad_flag 恒 1
        //（文件输出均为有效音频，静音/旁路归音频进程）。末尾不足一帧补零。
        total_out =
            (output.size() + MOZART_OUTPUT_SAMPLES - 1) / MOZART_OUTPUT_SAMPLES;
        for (size_t i = 0; i < total_out && running_.load(); ++i) {
            mozart_output_frame_t frame{};
            frame.meta.frame_idx = static_cast<uint32_t>(i);
            frame.meta.pts_ns =
                static_cast<uint64_t>(i) * MOZART_OUTPUT_FRAME_MS * 1'000'000ull;
            frame.meta.vad_flag = 1;
            const size_t offset = i * MOZART_OUTPUT_SAMPLES;
            const size_t count =
                std::min<size_t>(MOZART_OUTPUT_SAMPLES, output.size() - offset);
            std::copy_n(output.begin() + static_cast<std::ptrdiff_t>(offset),
                        count, frame.pcm);
            if (!stream_.WriteFrame(&frame, sizeof(frame))) {
                // 关停导致的写失败属取消收尾；其余记原因交 GetStatus 查询。
                if (running_.load()) set_last_error("output frame write failed");
                break;
            }
            ++frames_out;
            std::lock_guard<std::mutex> lock(stats_mutex_);
            frames_out_ = frames_out;
        }
    } catch (const InferenceCancelled&) {
        // StopStream 取消：正常收尾，不算错误，不向外抛。
        running_ = false;
        spdlog::info("FileWorker cancelled during inference");
        return;
    } catch (const std::exception& error) {
        // 含整段分配失败（时长上限由 daemon 兜底，此处只记录原因）。
        set_last_error(std::string("file batch failed: ") + error.what());
        running_ = false;
        return;
    }
    {
        std::lock_guard<std::mutex> lock(stats_mutex_);
        done_ = frames_out == total_out;
    }
    running_ = false;
    spdlog::info("FileWorker finished: {} frames in, {} frames out, {} windows",
                 frames_in, frames_out, pipeline_.offline_window_count());
}

FileWorker::Stats FileWorker::stats() const {
    Stats s;
    s.blocks = pipeline_.offline_window_count();
    std::lock_guard<std::mutex> lock(stats_mutex_);
    s.frames_in = frames_in_;
    s.frames_out = frames_out_;
    s.done = done_;
    if (!std::isnan(inference_ms_)) {
        s.inference_ms_last = inference_ms_;
        s.inference_ms_avg = inference_ms_;
        s.inference_ms_p95 = inference_ms_;
    }
    s.last_error = last_error_;
    return s;
}

} // namespace rvc