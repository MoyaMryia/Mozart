// stream_session.h — SPEC-03 控制面：七命令服务端
// ============================================================================
// Unix SOCK_SEQPACKET + core/include/mozart/ctl.h 信封（16 B 头 + JSON 体），
// 应答体 {"status","code","reason","data"} 与 core/src/ctl/schema.rs 一致。
//
// 七命令语义（SPEC-03 已确认条款）：
//   Hello       版本握手；信封版本不兼容时拒绝并断链。
//   LoadModel   持 ModelManager load_mutex 串行加载；返回 model_handle、
//               capabilities 与 model_version；入口校验一次，内部信任。
//   ListModels  模型目录清单 {id,exists,loaded}；只读、不加载、不阻塞推理
//               线程；loaded 按当前模型 id 判定。
//   StartStream 参数此时固定（kind/model_handle/pitch_shift/index_rate/
//               rms_mix_rate/protect/f0_method）；kind=file 另需 total_frames
//               （正整数 u32），可选 output_socket（绝对路径，覆盖默认输出
//               socket）；realtime 忽略这两字段。已有活动流返回 Busy；
//               未加载返回 NotLoaded；句柄过期返回 InvalidArgument；
//               缺实时资产返回 Unsupported，不静默降级。
//   StopStream  停收新帧，等当前推理块结束（实时约 240 ms，文件停在下一个
//               检查点）后返回；幂等。
//   ResetStream 断续重置：清环、清淡化尾并恢复启动缓冲；无流时空操作。
//   GetStatus   只读原子快照，字段名与 core/src/ctl/schema.rs::RvcStatus
//               一致（daemon 的解析契约），不阻塞推理线程。
//
// 后端 seam：SessionBackend 把「模型加载 / 能力查询 / 建 worker」抽象出来，
// 控制语义可在无 GPU 主机上桩测；生产实现 InferenceBackend 组合搬运的
// ModelManager 与 RealRVCPipeline。
#ifndef RVC_STREAM_SESSION_H
#define RVC_STREAM_SESSION_H

#include <atomic>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <vector>

#include <nlohmann/json.hpp>

#include "control/ctl_link.h"
#include "rvc/pipeline.hpp"

namespace rvc {

// 进程报告的协议/服务版本（与 Cargo/CMake 项目版本一致）。
inline constexpr const char* kRvcVersion = "0.1.0";

// 状态快照（对应 RvcStatus 的非字符串字段；NaN = 无推理数据）。
struct WorkerStats {
    uint64_t frames_in = 0;
    uint64_t frames_out = 0;
    uint64_t frames_dropped = 0;
    uint64_t blocks = 0;
    double inference_ms_last = 0.0;
    double inference_ms_avg = 0.0;
    double inference_ms_p95 = 0.0;
    uint64_t queue_depth = 0;
    std::string last_error;
};

// 活动流句柄：stop() 幂等并等待当前推理块结束；stats() 只读快照。
class StreamWorker {
public:
    virtual ~StreamWorker() = default;
    virtual void stop() = 0;
    virtual void reset() = 0;
    virtual WorkerStats stats() = 0;
};

// LoadModel 结果三态：NotFound → InvalidArgument（不存在的模型属请求错误），
// Failed → InferenceFailed（引擎加载失败），Ok → 应答带句柄与能力。
enum class LoadResult { Ok, NotFound, Failed };

// ListModels 清单条目：id + exists（资产齐全可加载）；loaded 由 handler
// 按当前模型 id 判定（单一口径，core/src/ctl/schema.rs::ModelEntry）。
struct ModelListing {
    std::string id;
    bool exists = false;
};

// kind=file 批处理选项（计划 D1/D2）；realtime 忽略。
// StartStream 入口校验一次，内部信任。
struct FileBatchOptions {
    uint32_t total_frames = 0;              // 攒帧总数，正整数
    std::filesystem::path output_socket;    // 空 = 用配置默认输出 socket
};

// 推理后端 seam。错误用 reason 表达，不跨线程抛异常。
class SessionBackend {
public:
    virtual ~SessionBackend() = default;

    // 加载并切为当前模型；reason 给出失败原因。流活动时允许换模型
    //（旧 RealRVCPipeline 原子换入语义；daemon 切换模型不经 StopStream）。
    virtual LoadResult load_model(const std::string& model_id, std::string& reason) = 0;
    // 当前模型 id / 版本 / 能力；未加载时 id 与 version 为空、capabilities 空。
    virtual std::string current_model_id() const = 0;
    virtual std::string model_version() const = 0;
    virtual std::vector<std::string> capabilities() const = 0;
    // 模型目录清单；只读、不加载、不阻塞推理线程。
    virtual std::vector<ModelListing> list_models() const = 0;
    // kind=realtime / kind=file 的能力探测（单一探测点，搬运管线提供）。
    virtual bool supports_realtime() const = 0;
    virtual bool supports_file() const = 0;
    // 启动流 worker：realtime → 帧泵，file → 整段批处理（file_options 生效）。
    // 失败时 reason 给出原因（数据面绑定失败等）。
    virtual std::unique_ptr<StreamWorker> start_worker(
        bool realtime, const RvcParameters& params,
        const FileBatchOptions& file_options, std::string& reason) = 0;
};

class StreamSession {
public:
    explicit StreamSession(std::unique_ptr<SessionBackend> backend);
    ~StreamSession();   // shutdown()

    StreamSession(const StreamSession&) = delete;
    StreamSession& operator=(const StreamSession&) = delete;

    // 处理一条控制消息，返回应答 JSON 体（信封由调用方加）。
    // 协议级错误（坏帧、版本不兼容）不经过本函数，见 serve。
    std::vector<uint8_t> handle(const CtlMessage& message);

    // 服务循环：接受连接并逐条处理，stop 置位后返回（先 shutdown）。
    // 连接逐个服务（daemon 持单连接每秒轮询）；空闲 30 s 断开。
    void serve(CtlListener& listener, const std::atomic<bool>& stop);

    // 幂等停流并保留最终计数缓存（GetStatus 在停流后仍可查询）。
    void shutdown();

private:
    std::unique_ptr<SessionBackend> backend_;
    std::unique_ptr<StreamWorker> worker_;
    WorkerStats final_stats_;
    bool has_final_stats_ = false;

    mutable std::mutex mutex_;               // 模型元数据与计数缓存的快照锁
    std::string model_id_;
    std::string model_version_;
    std::vector<std::string> capabilities_;
    std::string stream_kind_;                // 空 = 无活动流
    uint64_t model_handle_ = 0;              // 0 = 未加载
    uint64_t next_handle_ = 0;
    std::string last_error_;                 // 建 worker 失败等原因

    std::vector<uint8_t> handle_load_model(const nlohmann::json& body);
    std::vector<uint8_t> handle_start_stream(const nlohmann::json& body);
    WorkerStats current_stats_locked() const;
};

// ---- 生产后端：组合 ModelManager + RealRVCPipeline + Unix DGRAM 数据面 ----

struct InferenceBackendConfig {
    std::filesystem::path models_dir;
    std::string device = "cuda";
    bool half = false;
    size_t cache_limit = 2;
    std::filesystem::path hubert_path;            // 空 = 不配置
    std::filesystem::path rmvpe_path;
    std::filesystem::path realtime_hubert_path;
    std::filesystem::path realtime_rmvpe_path;
    std::filesystem::path input_socket;           // rvc 绑定收输入帧
    std::filesystem::path output_socket;          // connect 发输出帧（实时默认，pre 绑定；
                                                  // file 可被 StartStream output_socket 覆盖）
    size_t queue_frames = 8;                      // 数据面内核缓冲帧数
};

// 持有搬运的推理管线与模型缓存；构造时加载特征引擎（旧后端同次序）。
class InferenceBackend final : public SessionBackend {
public:
    explicit InferenceBackend(const InferenceBackendConfig& config);

    LoadResult load_model(const std::string& model_id, std::string& reason) override;
    std::string current_model_id() const override;
    std::string model_version() const override;
    std::vector<std::string> capabilities() const override;
    std::vector<ModelListing> list_models() const override;
    bool supports_realtime() const override;
    bool supports_file() const override;
    std::unique_ptr<StreamWorker> start_worker(
        bool realtime, const RvcParameters& params,
        const FileBatchOptions& file_options, std::string& reason) override;

    // 测试与诊断入口：当前模型目录是否存在（LoadModel 前置探测）。
    bool model_dir_exists(const std::string& model_id) const;

private:
    InferenceBackendConfig config_;
    std::shared_ptr<ModelManager> models_;
    std::unique_ptr<RealRVCPipeline> pipeline_;
};

} // namespace rvc

#endif // RVC_STREAM_SESSION_H
