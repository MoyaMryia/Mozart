#pragma once

// audio_worker.hpp — 帧泵外层（搬运自 Mozart-Old/rvc-backend/include/rvc/audio_worker.hpp）
// ============================================================================
// 算法无关 IO 与 RVC 管线之间的业务门面。不持有 socket 或设备资源：
// StreamSession 打开/关停数据面流并围绕它起停本 worker。
//
// 双模式：
//   - mock pipeline → 逐帧 1:1 直通（20ms 帧，零附加延迟；单测用）
//   - real pipeline → kind=realtime 用短块 realtime + SOLA preset；
//                     kind=file 用 quality/full-history 或 2s 滑窗；
//                     推理与 IO 分线程。
//
// 停止次序沿用旧实现：close stream（解除阻塞）→ join inference → join worker。

#include <atomic>
#include <cstdint>
#include <limits>
#include <memory>
#include <mutex>
#include <string>
#include <thread>

#include "mozart/frame.h"
#include "transport/audio_stream.h"
#include "rvc/pipeline.hpp"
#include "rvc/streaming_pipeline.hpp"

namespace rvc {

class AudioWorker {
public:
    struct Config {
        // 会话种类：true → 短块 realtime preset；false → quality/滑窗 preset。
        // 实时能力由入口（StreamSession）校验，此处不再复检。
        bool realtime_kind = true;
        bool skip_silence = true;
        uint32_t realtime_playout_delay_ms = 400;
    };

    // 数据面与推理侧丢弃/欠载计数，供状态快照发布。
    // 坏包与发送丢弃计数归数据面流本身（StreamSession 合并发布）。
    struct StreamStats {
        uint64_t frames_in = 0;          // 成功读入的输入帧
        uint64_t frames_out = 0;         // 成功写出的输出帧（含补零帧）
        uint64_t blocks = 0;             // 完成推理的块数
        uint64_t skipped_blocks = 0;
        uint64_t resets = 0;
        uint64_t late_blocks = 0;
        uint64_t input_overruns = 0;
        uint64_t output_overruns = 0;
        uint64_t inference_errors = 0;
        uint64_t history_resets = 0;
        uint64_t output_underruns = 0;   // 输出欠载补零帧数
        uint64_t startup_output_underruns = 0;
        uint64_t queue_depth_frames = 0; // 输出环待消费帧数
        double inference_ms_last = std::numeric_limits<double>::quiet_NaN();
        double inference_ms_avg = std::numeric_limits<double>::quiet_NaN();
        double inference_ms_p95 = std::numeric_limits<double>::quiet_NaN();
        std::string last_error;           // 传输错误优先，其次推理失败原因
    };

    // 流不归 worker 所有（StreamSession 持有），生命周期由调用方保证
    // 覆盖 start/stop 全程。
    AudioWorker(RealTimeAudioStream& stream,
                RVCPipelineBase& pipeline,
                Config config);
    ~AudioWorker();

    AudioWorker(const AudioWorker&) = delete;
    AudioWorker& operator=(const AudioWorker&) = delete;

    void start();
    void stop();
    bool running() const noexcept { return running_.load(); }

    StreamStats get_stream_stats() const;

    // 断续重置：清环、清淡化尾并恢复启动缓冲（ResetStream 语义）。
    // 仅流式模式有效；无流时为空操作。
    void reset_stream();

private:
    RealTimeAudioStream& stream_;
    RVCPipelineBase& pipeline_;
    Config config_;

    bool stream_mode_ = false;
    std::unique_ptr<StreamingRvc> streaming_;

    std::atomic<bool> running_{false};
    std::thread worker_thread_;
    std::thread inference_thread_;

    mutable std::mutex stats_mutex_;
    uint64_t frames_in_{0};
    uint64_t frames_out_{0};
    uint64_t underrun_count_{0};
    uint64_t startup_underrun_count_{0};
    bool stream_output_started_{false};
    std::string last_error_;

    void process_loop();
    void process_frame(const mozart_input_frame_t& input,
                       mozart_output_frame_t& output);
    void set_last_error(const std::string& message);
};

} // namespace rvc
