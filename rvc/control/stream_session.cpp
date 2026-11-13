// stream_session.cpp — SPEC-03 七命令服务端实现（见头文件说明）
#include "control/stream_session.h"

#include <chrono>
#include <cmath>
#include <limits>
#include <optional>
#include <utility>

#include <poll.h>
#include <sys/socket.h>

#include <spdlog/spdlog.h>

#include "mozart/ctl.h"
#include "rvc/audio_worker.hpp"
#include "rvc/file_worker.hpp"
#include "transport/unix_dgram_stream.h"

namespace rvc {

namespace {

// 连接内接收轮询与空闲断开：空闲 30 s 断开对齐 daemon 侧纪律（SPEC-04）。
constexpr int kRecvPollMs = 1000;
constexpr int kIdleTimeoutMs = 30'000;

// 应答信封，字段与 core/src/ctl/schema.rs::Response 逐字段一致。
std::vector<uint8_t> to_bytes(const nlohmann::json& envelope) {
    const std::string text = envelope.dump();
    return {text.begin(), text.end()};
}

std::vector<uint8_t> ok_body(nlohmann::json data) {
    const nlohmann::json envelope = {
        {"status", "ok"}, {"code", MOZART_CTL_CODE_OK}, {"reason", ""},
        {"data", std::move(data)}};
    return to_bytes(envelope);
}

std::vector<uint8_t> error_body(const char* code, const std::string& reason) {
    const nlohmann::json envelope = {
        {"status", "error"}, {"code", code}, {"reason", reason},
        {"data", nullptr}};
    return to_bytes(envelope);
}

// 请求体解析：空体按空 JSON 对象（与桩一致）；坏 JSON 一律 InvalidArgument。
nlohmann::json parse_body(const std::vector<uint8_t>& body) {
    if (body.empty()) return nlohmann::json::object();
    return nlohmann::json::parse(body, nullptr, false);
}

// 空串发布为 JSON null；显式分派避免三元运算把 nullptr 转成 std::string。
nlohmann::json string_or_null(const std::string& value) {
    if (value.empty()) return nullptr;
    return value;
}

} // namespace

StreamSession::StreamSession(std::unique_ptr<SessionBackend> backend)
    : backend_(std::move(backend))
{}

StreamSession::~StreamSession() {
    shutdown();
}

std::vector<uint8_t> StreamSession::handle(const CtlMessage& message) {
    // 请求体是外部输入：在入口解析并校验一次，后续内部信任。
    const nlohmann::json body = parse_body(message.body);
    if (!body.is_object()) {
        return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                          "request body must be a JSON object");
    }

    switch (message.type) {
        case MOZART_CTL_RVC_HELLO: {
            // 帧级版本握手已在信封层完成；Hello 报告服务版本。
            return ok_body({{"version", kRvcVersion}});
        }
        case MOZART_CTL_RVC_LOAD_MODEL:
            return handle_load_model(body);
        case MOZART_CTL_RVC_LIST_MODELS: {
            // 只读清单：不加载、不持会话锁、不阻塞推理线程；
            // loaded 按当前模型 id 判定（SPEC-03）。
            const std::string current = backend_->current_model_id();
            nlohmann::json models = nlohmann::json::array();
            for (const auto& entry : backend_->list_models()) {
                models.push_back({{"id", entry.id},
                                  {"exists", entry.exists},
                                  {"loaded", entry.id == current}});
            }
            return ok_body({{"models", std::move(models)}});
        }
        case MOZART_CTL_RVC_START_STREAM:
            return handle_start_stream(body);
        case MOZART_CTL_RVC_STOP_STREAM: {
            shutdown();
            return ok_body(nlohmann::json::object());
        }
        case MOZART_CTL_RVC_RESET_STREAM: {
            std::lock_guard<std::mutex> lock(mutex_);
            if (worker_) worker_->reset();
            return ok_body(nlohmann::json::object());
        }
        case MOZART_CTL_RVC_GET_STATUS: {
            std::lock_guard<std::mutex> lock(mutex_);
            nlohmann::json data;
            data["state"] = worker_ ? "streaming" : (model_id_.empty() ? "idle" : "loaded");
            data["model_id"] = string_or_null(model_id_);
            data["model_version"] = string_or_null(model_version_);
            data["capabilities"] = capabilities_;
            data["stream_kind"] = string_or_null(stream_kind_);
            const WorkerStats stats = current_stats_locked();
            data["frames_in"] = stats.frames_in;
            data["frames_out"] = stats.frames_out;
            data["frames_dropped"] = stats.frames_dropped;
            data["blocks"] = stats.blocks;
            const auto put_ms = [&](const char* name, double value) {
                if (std::isnan(value)) {
                    data[name] = nullptr;
                } else {
                    data[name] = value;
                }
            };
            put_ms("inference_ms_last", stats.inference_ms_last);
            put_ms("inference_ms_avg", stats.inference_ms_avg);
            put_ms("inference_ms_p95", stats.inference_ms_p95);
            data["queue_depth"] = stats.queue_depth;
            data["last_error"] = string_or_null(stats.last_error);
            return ok_body(std::move(data));
        }
        default:
            return error_body(MOZART_CTL_CODE_UNKNOWN_COMMAND, "unknown command");
    }
}

std::vector<uint8_t> StreamSession::handle_load_model(
    const nlohmann::json& body) {
    if (!body.contains("model_id") || !body.at("model_id").is_string()) {
        return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                          "model_id must be a non-empty string");
    }
    const std::string model_id = body.at("model_id").get<std::string>();
    if (model_id.empty()) {
        return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                          "model_id must be a non-empty string");
    }
    std::string reason;
    const LoadResult result = backend_->load_model(model_id, reason);
    if (result == LoadResult::NotFound) {
        return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT, reason);
    }
    if (result == LoadResult::Failed) {
        return error_body(MOZART_CTL_CODE_INFERENCE_FAILED, reason);
    }
    std::lock_guard<std::mutex> lock(mutex_);
    model_id_ = backend_->current_model_id();
    model_version_ = backend_->model_version();
    capabilities_ = backend_->capabilities();
    model_handle_ = ++next_handle_;
    spdlog::info("LoadModel '{}' ok (handle={}, version={}, capabilities={})",
                 model_id_, model_handle_, model_version_,
                 nlohmann::json(capabilities_).dump());
    nlohmann::json data;
    data["model_id"] = model_id_;
    data["model_version"] = model_version_;
    data["model_handle"] = model_handle_;
    data["capabilities"] = capabilities_;
    return ok_body(std::move(data));
}

std::vector<uint8_t> StreamSession::handle_start_stream(
    const nlohmann::json& body) {
    // ---- 入口校验一次（SPEC-03：StartStream 参数此时固定）----
    const char* missing = nullptr;
    if (!body.contains("kind") || !body.at("kind").is_string()) missing = "kind";
    else if (!body.contains("model_handle") || !body.at("model_handle").is_number_unsigned())
        missing = "model_handle";
    else if (!body.contains("pitch_shift") || !body.at("pitch_shift").is_number_integer())
        missing = "pitch_shift";
    else if (!body.contains("index_rate") || !body.at("index_rate").is_number())
        missing = "index_rate";
    else if (!body.contains("rms_mix_rate") || !body.at("rms_mix_rate").is_number())
        missing = "rms_mix_rate";
    else if (!body.contains("protect") || !body.at("protect").is_number())
        missing = "protect";
    else if (!body.contains("f0_method") || !body.at("f0_method").is_string())
        missing = "f0_method";
    if (missing) {
        return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                          std::string("missing or malformed field: ") + missing);
    }
    const std::string kind = body.at("kind").get<std::string>();
    if (kind != "realtime" && kind != "file") {
        return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                          "kind must be realtime or file");
    }
    // kind=file 批处理选项（D1）：total_frames 必填正整数（u32 契约）；
    // realtime 忽略该字段。数据面复用配置默认 socket（SPEC-03 状态门控交接）。
    FileBatchOptions file_options;
    if (kind == "file") {
        if (!body.contains("total_frames")
            || !body.at("total_frames").is_number_unsigned()
            || body.at("total_frames").get<uint64_t>() == 0
            || body.at("total_frames").get<uint64_t>()
                   > std::numeric_limits<uint32_t>::max()) {
            return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                              "total_frames must be a positive integer (u32)");
        }
        file_options.total_frames =
            static_cast<uint32_t>(body.at("total_frames").get<uint64_t>());
        // total_samples 可选（缺省 0=不裁，向后兼容）：解码真实采样数，
        // 须为正且不超过满帧采样数，供 file worker 裁掉尾帧补零。
        if (body.contains("total_samples")) {
            if (!body.at("total_samples").is_number_unsigned()) {
                return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                                  "total_samples must be an unsigned integer");
            }
            const uint64_t total_samples = body.at("total_samples").get<uint64_t>();
            if (total_samples == 0
                || total_samples > static_cast<uint64_t>(file_options.total_frames)
                                       * MOZART_INPUT_SAMPLES) {
                return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                                  "total_samples must be in (0, total_frames*320]");
            }
            file_options.total_samples = total_samples;
        }
    }
    RvcParameters parameters;
    parameters.f0_method = body.at("f0_method").get<std::string>();
    parameters.pitch_shift = body.at("pitch_shift").get<int>();
    parameters.index_rate = body.at("index_rate").get<double>();
    parameters.rms_mix_rate = body.at("rms_mix_rate").get<double>();
    parameters.protect = body.at("protect").get<double>();
    if (const char* bad = invalid_rvc_parameter(parameters)) {
        return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                          std::string("parameter out of range: ") + bad);
    }

    std::lock_guard<std::mutex> lock(mutex_);
    if (worker_) {
        return error_body(MOZART_CTL_CODE_BUSY, "stream already active");
    }
    if (model_id_.empty() || model_handle_ == 0) {
        return error_body(MOZART_CTL_CODE_NOT_LOADED, "load a model first");
    }
    const uint64_t requested_handle = body.at("model_handle").get<uint64_t>();
    if (requested_handle != model_handle_) {
        return error_body(MOZART_CTL_CODE_INVALID_ARGUMENT,
                          "model_handle does not match the loaded model");
    }
    const bool realtime = kind == "realtime";
    if (realtime && !backend_->supports_realtime()) {
        return error_body(MOZART_CTL_CODE_UNSUPPORTED,
                          "model has no realtime assets; refusing realtime stream");
    }
    if (!realtime && !backend_->supports_file()) {
        return error_body(MOZART_CTL_CODE_UNSUPPORTED,
                          "model has no offline generator; refusing file stream");
    }

    std::string reason;
    auto worker = backend_->start_worker(realtime, parameters, file_options,
                                         reason);
    if (!worker) {
        last_error_ = reason;
        return error_body(MOZART_CTL_CODE_INTERNAL, reason);
    }
    worker_ = std::move(worker);
    stream_kind_ = kind;
    has_final_stats_ = false;
    spdlog::info("StartStream kind={} model={} handle={}", kind, model_id_,
                 model_handle_);
    return ok_body({{"stream_kind", kind}});
}

WorkerStats StreamSession::current_stats_locked() const {
    // 调用方持 mutex_。活动流读实时快照；停流后返回最终计数缓存。
    if (worker_) return worker_->stats();
    if (has_final_stats_) return final_stats_;
    return WorkerStats{};
}

void StreamSession::shutdown() {
    std::unique_ptr<StreamWorker> worker;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        worker = std::move(worker_);
        // stream_kind 清空后其余命令立即看到「无流」；计数在下面对齐。
        stream_kind_.clear();
    }
    if (worker) {
        // 停流在锁外等待当前推理块（约 240 ms）；结束后再取快照，
        // GetStatus 在停流后查到的是包含最后一块的最终计数。
        worker->stop();
        const WorkerStats final_stats = worker->stats();
        std::lock_guard<std::mutex> lock(mutex_);
        final_stats_ = final_stats;
        has_final_stats_ = true;
    }
    worker.reset();
}

void StreamSession::serve(CtlListener& listener, const std::atomic<bool>& stop) {
    while (!stop.load(std::memory_order_relaxed)) {
        auto connection = listener.accept(stop);
        if (!connection) break;
        // 连接逐个服务：daemon 持单连接每秒轮询（SPEC-03/04）。
        int idle_ms = 0;
        while (!stop.load(std::memory_order_relaxed)) {
            CtlMessage message;
            const CtlRecv status = connection->recv(message, kRecvPollMs);
            if (status == CtlRecv::Timeout) {
                idle_ms += kRecvPollMs;
                if (idle_ms >= kIdleTimeoutMs) break;
                continue;
            }
            if (status == CtlRecv::Closed) break;
            if (status == CtlRecv::ProtocolError) {
                spdlog::warn("rvc control: malformed frame, closing connection");
                break;
            }
            if (status == CtlRecv::VersionError) {
                // 版本握手不兼容：拒绝并断链（SPEC-03）。
                const auto reply = error_body(
                    MOZART_CTL_CODE_UNSUPPORTED,
                    "control protocol version mismatch; expected "
                        + std::to_string(MOZART_CTL_VERSION));
                connection->send(message.type, message.request_id,
                                 reply.data(), reply.size());
                break;
            }
            idle_ms = 0;
            const auto reply = handle(message);
            if (!connection->send(message.type, message.request_id,
                                  reply.data(), reply.size())) {
                break;
            }
        }
    }
    shutdown();
}

// ---- 生产后端与 worker ----

namespace {

// StreamWorker 生产实现：持有数据面流与 AudioWorker（生命周期与流绑定）。
class PumpStreamWorker final : public StreamWorker {
public:
    PumpStreamWorker(std::unique_ptr<UnixDgramStream> stream,
                     std::unique_ptr<AudioWorker> worker)
        : stream_(std::move(stream)), worker_(std::move(worker)) {}

    void stop() override {
        // 次序照旧：close stream（解除 ReadFrame 阻塞）→ join inference → join worker。
        worker_->stop();
    }

    void reset() override { worker_->reset_stream(); }

    WorkerStats stats() override {
        const AudioWorker::StreamStats s = worker_->get_stream_stats();
        const DgramStreamStats transport = stream_->stats();
        WorkerStats out;
        out.frames_in = s.frames_in;
        out.frames_out = s.frames_out;
        out.frames_dropped = transport.recv_dropped + transport.send_dropped;
        out.blocks = s.blocks;
        out.inference_ms_last = s.inference_ms_last;
        out.inference_ms_avg = s.inference_ms_avg;
        out.inference_ms_p95 = s.inference_ms_p95;
        out.queue_depth = s.queue_depth_frames;
        out.last_error = s.last_error;
        return out;
    }

private:
    std::unique_ptr<UnixDgramStream> stream_;
    std::unique_ptr<AudioWorker> worker_;
};

// StreamWorker 文件实现：持有数据面流（阻塞发送）与 FileWorker。
// 批处理发完线程自行退出，但流句柄保留到 StopStream（GetStatus 仍报
// streaming，daemon 读完输出帧后停流）。
class FileStreamWorker final : public StreamWorker {
public:
    FileStreamWorker(std::unique_ptr<UnixDgramStream> stream,
                     std::unique_ptr<FileWorker> worker)
        : stream_(std::move(stream)), worker_(std::move(worker)) {}

    void stop() override {
        // 次序照旧：置取消 + close stream（解除攒帧/发送阻塞）→ join worker。
        worker_->stop();
    }

    void reset() override {
        // ResetStream 是流式断续语义；整段批处理无环可清，空操作。
    }

    WorkerStats stats() override {
        const FileWorker::Stats s = worker_->stats();
        const DgramStreamStats transport = stream_->stats();
        WorkerStats out;
        out.frames_in = s.frames_in;
        out.frames_out = s.frames_out;
        out.frames_dropped = transport.recv_dropped + transport.send_dropped;
        out.blocks = s.blocks;
        out.inference_ms_last = s.inference_ms_last;
        out.inference_ms_avg = s.inference_ms_avg;
        out.inference_ms_p95 = s.inference_ms_p95;
        out.queue_depth = 0;   // 整段批处理无输出环
        out.last_error = s.last_error;
        return out;
    }

private:
    std::unique_ptr<UnixDgramStream> stream_;
    std::unique_ptr<FileWorker> worker_;
};

} // namespace

InferenceBackend::InferenceBackend(const InferenceBackendConfig& config)
    : config_(config) {
    // hubert 传裸路径，rmvpe 与实时资产传 optional（RealRVCPipeline 契约）；
    // 配置为空保持 nullopt，不在此处二次包装。
    std::optional<std::filesystem::path> rmvpe;
    std::optional<std::filesystem::path> realtime_hubert;
    std::optional<std::filesystem::path> realtime_rmvpe;
    if (!config_.rmvpe_path.empty()) rmvpe = config_.rmvpe_path;
    if (!config_.realtime_hubert_path.empty()) realtime_hubert = config_.realtime_hubert_path;
    if (!config_.realtime_rmvpe_path.empty()) realtime_rmvpe = config_.realtime_rmvpe_path;

    models_ = std::make_shared<ModelManager>(
        config_.models_dir, config_.device, config_.half, config_.cache_limit,
        config_.models);
    pipeline_ = std::make_unique<RealRVCPipeline>(
        models_, config_.hubert_path, rmvpe,
        MOZART_INPUT_SAMPLE_RATE, MOZART_OUTPUT_SAMPLE_RATE,
        config_.device, config_.half, RvcMockConfig{}, RvcParameters{},
        realtime_hubert, realtime_rmvpe);
    spdlog::info("rvc inference backend ready (models_dir={})",
                 config_.models_dir.string());
}

LoadResult InferenceBackend::load_model(const std::string& model_id,
                                          std::string& reason) {
    if (config_.models_dir.empty()) {
        reason = "models_dir is not configured";
        return LoadResult::Failed;
    }
    if (!model_dir_exists(model_id)) {
        reason = "model directory not found: " + model_id;
        return LoadResult::NotFound;
    }
    // load_mutex 串行加载；命中缓存约 1 ms，冷加载含引擎反序列化。
    if (!models_->load_model(model_id) || !pipeline_->switch_model(model_id)) {
        reason = "model load failed: " + model_id + " (see log for details)";
        return LoadResult::Failed;
    }
    return LoadResult::Ok;
}

bool InferenceBackend::model_dir_exists(const std::string& model_id) const {
    if (config_.models_dir.empty()) return false;
    std::error_code error;
    return std::filesystem::is_directory(config_.models_dir / model_id, error);
}

std::string InferenceBackend::current_model_id() const {
    const auto model = models_->current_model();
    return model ? model->id() : std::string{};
}

std::string InferenceBackend::model_version() const {
    const auto model = models_->current_model();
    return model ? model->config().version : std::string{};
}

std::vector<std::string> InferenceBackend::capabilities() const {
    std::vector<std::string> result;
    if (supports_realtime()) result.push_back("realtime");
    if (supports_file()) result.push_back("file");
    return result;
}

std::vector<ModelListing> InferenceBackend::list_models() const {
    // ModelManager 目录扫描（其内部锁只保护缓存表，不占推理线程）。
    std::vector<ModelListing> result;
    for (const auto& entry : models_->list_models()) {
        result.push_back({entry.at("id"), entry.at("exists") == "true"});
    }
    return result;
}

bool InferenceBackend::supports_realtime() const {
    return pipeline_->supports_realtime_streaming();
}

bool InferenceBackend::supports_file() const {
    // kind=file 需要离线 Generator；整段批处理由 FileWorker 走现有离线
    // 推理路径（整段一次过或 2 s 滑窗由 inferencer 按引擎形状决定）。
    const auto model = models_->current_model();
    return model && model->loaded() && model->has_generator();
}

std::unique_ptr<StreamWorker> InferenceBackend::start_worker(
    bool realtime, const RvcParameters& params,
    const FileBatchOptions& file_options, std::string& reason) {
    // 会话参数在启动时固定（SPEC-03）；先落参数再造推理器。
    pipeline_->set_parameters(params);
    try {
        // 数据面复用配置默认 socket（SPEC-03「复用帧数据面 / 切换时交接绑定」）：
        // 实时由 pre 绑定输出 socket；文件期间 pre 经 PRE_SET_FILE_MODE 暂停
        // 发送并释放输出 socket，daemon 接管同一对 socket。C++ 对对端无感知，
        // 只绑定/连接配置路径，实时/文件互斥由 daemon 状态机保证。
        auto stream = std::make_unique<UnixDgramStream>(
            config_.input_socket, config_.output_socket, config_.queue_frames,
            /*blocking_send=*/!realtime);
        if (!realtime) {
            FileWorker::Config worker_config;
            worker_config.total_frames = file_options.total_frames;
            worker_config.total_samples = file_options.total_samples;
            auto worker = std::make_unique<FileWorker>(*stream, *pipeline_,
                                                       worker_config);
            worker->start();
            return std::make_unique<FileStreamWorker>(std::move(stream),
                                                      std::move(worker));
        }
        auto worker = std::make_unique<AudioWorker>(*stream, *pipeline_,
                                                    AudioWorker::Config{});
        worker->start();
        return std::make_unique<PumpStreamWorker>(std::move(stream),
                                                   std::move(worker));
    } catch (const std::exception& error) {
        reason = std::string("start stream worker: ") + error.what();
        return nullptr;
    }
}

} // namespace rvc
