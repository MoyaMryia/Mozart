#pragma once

// file_worker.hpp — 文件转换整段批处理外层（SPEC-03 离线推理 v1）
// ============================================================================
// 与 AudioWorker 同层的业务门面：不持有 socket 或设备资源，StreamSession
// 打开/关停数据面流并围绕它起停本 worker。与实时泵的区别：
//   攒帧   ReadFrame 循环把 total_frames 个 320 样本输入帧拼成 16 kHz 整段；
//   推理   调 pipeline.process_cancellable 跑现有离线流程一次（峰值归一、
//          高通、整段 F0、整段一次过或 2 s 滑窗、RMS 混合、限幅）；
//   回帧   48 kHz 结果切 960 样本输出帧逐帧发回，meta.frame_idx 从 0 顺序；
//          数据面须为阻塞发送模式（UnixDgramStream blocking_send），不丢帧。
//
// 生命周期：发完线程自行退出（done），StreamSession 在 StopStream 前仍报
// streaming。stop() 置取消 + 关流 + join：攒帧每帧查取消，推理检查点抛
// InferenceCancelled 在 worker 内捕获收尾，不跨线程/进程边界。
// blocks 单一来源是管线的 Generator 窗计数（offline_window_count），
// 批处理开始清零。

#include <atomic>
#include <cstdint>
#include <limits>
#include <mutex>
#include <string>
#include <thread>

#include "mozart/frame.h"
#include "transport/audio_stream.h"
#include "rvc/pipeline.hpp"

namespace rvc {

class FileWorker {
public:
    struct Config {
        // 攒帧总数；StartStream 入口已校验为正整数（u32），此处信任。
        uint32_t total_frames = 0;
        // 真实采样数（尾帧补零前）；StartStream 入口已校验，0=不裁，此处信任。
        uint64_t total_samples = 0;
    };

    // 批处理快照，供 StreamSession 合并发布（GetStatus）。
    struct Stats {
        uint64_t frames_in = 0;          // 成功读入的输入帧
        uint64_t frames_out = 0;         // 成功写出的输出帧
        uint64_t blocks = 0;             // Generator 窗数（管线单一来源）
        // 整段只推理一次：last/avg/p95 同为该次耗时；未推理为 NaN。
        double inference_ms_last = std::numeric_limits<double>::quiet_NaN();
        double inference_ms_avg = std::numeric_limits<double>::quiet_NaN();
        double inference_ms_p95 = std::numeric_limits<double>::quiet_NaN();
        bool done = false;               // 输出帧全部发完
        std::string last_error;          // 推理失败/传输错误原因；取消不算错误
    };

    // 流不归 worker 所有（StreamSession 持有），生命周期由调用方保证
    // 覆盖 start/stop 全程。
    FileWorker(RealTimeAudioStream& stream, RVCPipelineBase& pipeline,
               Config config);
    ~FileWorker();

    FileWorker(const FileWorker&) = delete;
    FileWorker& operator=(const FileWorker&) = delete;

    void start();
    void stop();
    bool running() const noexcept { return running_.load(); }

    Stats stats() const;

private:
    RealTimeAudioStream& stream_;
    RVCPipelineBase& pipeline_;
    Config config_;

    std::atomic<bool> running_{false};
    std::atomic<bool> cancelled_{false};
    std::thread thread_;

    mutable std::mutex stats_mutex_;
    uint64_t frames_in_ = 0;
    uint64_t frames_out_ = 0;
    bool done_ = false;
    double inference_ms_ = std::numeric_limits<double>::quiet_NaN();
    std::string last_error_;

    void batch_loop();
    void set_last_error(const std::string& message);
};

} // namespace rvc