// test_rvc_control.cpp — StreamSession 七命令语义单测（无 GPU 依赖）
// ============================================================================
// 用桩后端（SessionBackend seam）覆盖控制面契约：
//   1. Hello 版本报告与请求体校验
//   2. LoadModel 三态：NotFound→InvalidArgument / Failed→InferenceFailed / 成功
//   3. StartStream：NotLoaded / 参数边界 / 句柄校验 / Busy / 能力不足拒绝；
//      kind=file 的 total_frames / output_socket 入口校验（D1/D2）
//   4. ListModels：清单字段与 loaded 按当前模型 id 判定；真实 ModelManager
//      目录扫描（InferenceBackend，无 GPU）
//   5. GetStatus：流式与停流后的原子快照字段（字段名 = RvcStatus 契约）
//   6. StopStream 幂等、ResetStream 断续重置透传
//   7. 线上往返：应答信封回显 type/request_id；版本不兼容拒绝并断链；
//      坏帧断链；未知命令 UnknownCommand
#include "control/stream_session.h"

#include <atomic>
#include <chrono>
#include <cmath>
#include <cstring>
#include <cstdio>
#include <filesystem>
#include <fstream>
#include <memory>
#include <string>
#include <thread>
#include <vector>

#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include "mozart/ctl.h"

using namespace rvc;

namespace {

int g_fail = 0;
#define CHECK(cond, msg) do { \
    if (cond) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s\n", msg); ++g_fail; } } while (0)

// ---- 桩后端 ----
// StopStream 后 StreamSession 会销毁 worker；stop/reset 的观测走后端侧
// 原子计数器，不回读已销毁的 worker 对象（悬垂指针）。
struct FakeBackend;

struct FakeWorker : StreamWorker {
    explicit FakeWorker(FakeBackend& owner) : owner_(owner) {}

    void stop() override;
    void reset() override;
    WorkerStats stats() override;

private:
    FakeBackend& owner_;
};

struct FakeBackend : SessionBackend {
    std::string loaded_model;
    bool allow_realtime = true;
    bool allow_file = true;
    bool fail_start = false;
    std::atomic<int> stop_calls{0};
    std::atomic<int> reset_calls{0};
    std::vector<ModelListing> listing{{"alpha", true}, {"beta", false}};

    LoadResult load_model(const std::string& model_id, std::string& reason) override {
        if (model_id == "missing") {
            reason = "model directory not found: missing";
            return LoadResult::NotFound;
        }
        if (model_id == "broken") {
            reason = "engine load failed";
            return LoadResult::Failed;
        }
        loaded_model = model_id;
        return LoadResult::Ok;
    }
    std::string current_model_id() const override { return loaded_model; }
    std::string model_version() const override { return "v9"; }
    std::vector<std::string> capabilities() const override {
        std::vector<std::string> caps;
        if (allow_realtime) caps.push_back("realtime");
        if (allow_file) caps.push_back("file");
        return caps;
    }
    std::vector<ModelListing> list_models() const override { return listing; }
    bool supports_realtime() const override { return allow_realtime; }
    bool supports_file() const override { return allow_file; }
    std::unique_ptr<StreamWorker> start_worker(
        bool realtime, const RvcParameters& params,
        const FileBatchOptions& file_options, std::string& reason) override {
        if (fail_start) {
            reason = "bind failed";
            return nullptr;
        }
        last_params = params;
        last_realtime = realtime;
        last_file_options = file_options;
        return std::make_unique<FakeWorker>(*this);
    }
    RvcParameters last_params{};
    bool last_realtime = false;
    FileBatchOptions last_file_options{};
};

void FakeWorker::stop() { owner_.stop_calls.fetch_add(1); }
void FakeWorker::reset() { owner_.reset_calls.fetch_add(1); }
WorkerStats FakeWorker::stats() {
    WorkerStats out{};
    out.frames_in = 10;
    out.frames_out = 9;
    out.frames_dropped = 1;
    out.blocks = 2;
    out.queue_depth = 3;
    return out;
}

// ---- 应答解析 ----
struct Reply {
    std::string status;
    std::string code;
    std::string reason;
    nlohmann::json data;
};

Reply parse_reply(const std::vector<uint8_t>& bytes) {
    const auto envelope = nlohmann::json::parse(bytes, nullptr, false);
    return {envelope.at("status").get<std::string>(),
            envelope.at("code").get<std::string>(),
            envelope.at("reason").get<std::string>(),
            envelope.at("data")};
}

nlohmann::json start_body(uint64_t handle) {
    return {
        {"kind", "realtime"},
        {"model_handle", handle},
        {"pitch_shift", 0},
        {"index_rate", 0.0},
        {"rms_mix_rate", 1.0},
        {"protect", 0.33},
        {"f0_method", "rmvpe"},
    };
}

std::vector<uint8_t> to_bytes(const nlohmann::json& value) {
    const std::string text = value.dump();
    return {text.begin(), text.end()};
}

CtlMessage make_message(uint16_t type, const nlohmann::json& body) {
    CtlMessage message;
    message.type = type;
    message.request_id = 7;
    message.body = to_bytes(body);
    return message;
}

// ---- 线上客户端（裸 socket，覆盖 ctl_link 服务端）----
int connect_client(const std::filesystem::path& path) {
    const int fd = ::socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, path.c_str(), sizeof(addr.sun_path) - 1);
    socklen_t len = static_cast<socklen_t>(
        offsetof(sockaddr_un, sun_path) + path.native().size() + 1);
    if (::connect(fd, reinterpret_cast<const sockaddr*>(&addr), len) != 0) {
        std::perror("connect");
        std::exit(1);
    }
    return fd;
}

void put32(uint8_t* p, uint32_t v) {
    for (int i = 0; i < 4; ++i) p[i] = static_cast<uint8_t>(v >> (8 * i));
}
void put16(uint8_t* p, uint16_t v) {
    p[0] = static_cast<uint8_t>(v);
    p[1] = static_cast<uint8_t>(v >> 8);
}

ssize_t send_frame(int fd, uint32_t magic, uint16_t version, uint16_t type,
                   uint32_t request_id, const uint8_t* body, size_t len) {
    uint8_t frame[MOZART_CTL_HEADER_BYTES + MOZART_CTL_MAX_BODY_BYTES];
    put32(frame, magic);
    put16(frame + 4, version);
    put16(frame + 6, type);
    put32(frame + 8, request_id);
    put32(frame + 12, static_cast<uint32_t>(len));
    std::memcpy(frame + MOZART_CTL_HEADER_BYTES, body, len);
    return ::send(fd, frame, MOZART_CTL_HEADER_BYTES + len, MSG_NOSIGNAL);
}

// 接收一帧；返回 n<=0 表示对端关闭，-2 表示超时。
ssize_t recv_frame(int fd, uint8_t* buffer, size_t cap, int timeout_ms) {
    pollfd pfd{fd, POLLIN, 0};
    if (::poll(&pfd, 1, timeout_ms) <= 0) return -2;
    return ::recv(fd, buffer, cap, 0);
}

struct ReplyHeader {
    uint16_t version = 0;
    uint16_t type = 0;
    uint32_t request_id = 0;
    std::vector<uint8_t> body;
};

ReplyHeader parse_frame(const uint8_t* buffer, ssize_t n) {
    ReplyHeader out;
    out.version = static_cast<uint16_t>(buffer[4] | (buffer[5] << 8));
    out.type = static_cast<uint16_t>(buffer[6] | (buffer[7] << 8));
    out.request_id = static_cast<uint32_t>(buffer[8])
        | (static_cast<uint32_t>(buffer[9]) << 8)
        | (static_cast<uint32_t>(buffer[10]) << 16)
        | (static_cast<uint32_t>(buffer[11]) << 24);
    out.body.assign(buffer + MOZART_CTL_HEADER_BYTES, buffer + n);
    return out;
}

} // namespace

int main() {
    auto make_session = [] {
        auto backend = std::make_unique<FakeBackend>();
        FakeBackend* raw = backend.get();
        return std::pair{std::make_unique<StreamSession>(std::move(backend)), raw};
    };

    std::printf("test_hello_and_body_validation\n");
    {
        auto [session, backend] = make_session();
        Reply reply = parse_reply(session->handle(
            make_message(MOZART_CTL_RVC_HELLO, nlohmann::json::object())));
        CHECK(reply.status == "ok" && reply.data.at("version") == kRvcVersion,
              "hello reports the service version");
        reply = parse_reply(session->handle(make_message(MOZART_CTL_RVC_HELLO,
                                                          nlohmann::json::object())));
        CHECK(reply.status == "ok", "hello accepts an empty body");
        CtlMessage bad_body;
        bad_body.type = MOZART_CTL_RVC_HELLO;
        bad_body.body = {'}', '{'};
        reply = parse_reply(session->handle(bad_body));
        CHECK(reply.code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "malformed body is rejected once at the entry");
    }

    std::printf("test_load_model_three_way_result\n");
    {
        auto [session, backend] = make_session();
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", 42}})))
                  .code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "non-string model_id is rejected");
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", ""}})))
                  .code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "empty model_id is rejected");
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "missing"}})))
                  .code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "missing model maps to InvalidArgument");
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "broken"}})))
                  .code == MOZART_CTL_CODE_INFERENCE_FAILED,
              "engine failure maps to InferenceFailed");
        const Reply ok = parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "qiqi"}})));
        CHECK(ok.status == "ok" && ok.data.at("model_id") == "qiqi"
              && ok.data.at("model_version") == "v9"
              && ok.data.at("model_handle") == 1,
              "successful load returns identity and handle");
        CHECK(ok.data.at("capabilities") ==
                  nlohmann::json({"realtime", "file"}),
              "capabilities report realtime and file");
    }

    std::printf("test_start_stream_validation_and_busy\n");
    {
        auto [session, backend] = make_session();
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, start_body(1))))
                  .code == MOZART_CTL_CODE_NOT_LOADED,
              "start before load is NotLoaded");
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "qiqi"}})));

        nlohmann::json body = start_body(1);
        body["kind"] = "bogus";
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, body)))
                  .code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "unknown kind is rejected");
        body = start_body(99);
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, body)))
                  .code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "stale model_handle is rejected");
        body = start_body(1);
        body["pitch_shift"] = 13;
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, body)))
                  .code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "out-of-range parameter is rejected");
        body = start_body(1);
        body.erase("protect");
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, body)))
                  .code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "missing field is rejected");
        body = start_body(1);
        body["f0_method"] = "harvest";
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, body)))
                  .code == MOZART_CTL_CODE_INVALID_ARGUMENT,
              "unsupported f0_method is rejected");

        backend->allow_realtime = false;
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, start_body(1))))
                  .code == MOZART_CTL_CODE_UNSUPPORTED,
              "missing realtime assets refuse realtime (no silent downgrade)");
        backend->allow_realtime = true;

        const Reply ok = parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_START_STREAM, start_body(1))));
        CHECK(ok.status == "ok" && ok.data.at("stream_kind") == "realtime",
              "valid start returns the stream kind");
        CHECK(backend->last_realtime
              && backend->last_params.pitch_shift == 0
              && backend->last_params.f0_method == "rmvpe",
              "backend receives the fixed session parameters");
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, start_body(1))))
                  .code == MOZART_CTL_CODE_BUSY,
              "second start while active is Busy");
    }

    std::printf("test_file_kind_capability_gate\n");
    {
        auto [session, backend] = make_session();
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "qiqi"}})));
        backend->allow_file = false;
        nlohmann::json body = start_body(1);
        body["kind"] = "file";
        body["total_frames"] = 10;
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, body)))
                  .code == MOZART_CTL_CODE_UNSUPPORTED,
              "missing offline generator refuses file kind");
    }

    std::printf("test_list_models\n");
    {
        auto [session, backend] = make_session();
        Reply reply = parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LIST_MODELS, nlohmann::json::object())));
        CHECK(reply.status == "ok", "list models answers ok");
        nlohmann::json models = reply.data.at("models");
        CHECK(models.size() == 2 && models[0].at("id") == "alpha"
              && models[0].at("exists") == true
              && models[1].at("id") == "beta"
              && models[1].at("exists") == false,
              "listing carries id and exists per entry");
        CHECK(models[0].at("loaded") == false && models[1].at("loaded") == false,
              "nothing is loaded before LoadModel");

        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "alpha"}})));
        reply = parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LIST_MODELS, nlohmann::json::object())));
        models = reply.data.at("models");
        CHECK(models[0].at("loaded") == true && models[1].at("loaded") == false,
              "loaded follows the current model id");
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_GET_STATUS, nlohmann::json::object())))
                  .data.at("state") == "loaded",
              "list models is read-only and does not start a stream");
    }

    std::printf("test_list_models_directory_scan\n");
    {
        // 真实 ModelManager 目录扫描（InferenceBackend，无 GPU 不加载）：
        // alpha 资产齐全、beta 空目录、散文件不计入清单。
        const auto dir = std::filesystem::temp_directory_path()
            / ("mozart-rvc-models-" + std::to_string(::getpid()));
        std::filesystem::remove_all(dir);
        std::filesystem::create_directories(dir / "alpha");
        std::filesystem::create_directories(dir / "beta");
        { std::ofstream(dir / "alpha" / "config.json") << "{}"; }
        { std::ofstream(dir / "alpha" / "alpha.onnx"); }
        { std::ofstream(dir / "stray.txt"); }

        InferenceBackendConfig config;
        config.models_dir = dir;
        StreamSession session(std::make_unique<InferenceBackend>(config));
        const Reply reply = parse_reply(session.handle(make_message(
            MOZART_CTL_RVC_LIST_MODELS, nlohmann::json::object())));
        CHECK(reply.status == "ok", "directory scan answers ok");
        const nlohmann::json models = reply.data.at("models");
        CHECK(models.size() == 2, "only model directories are listed");
        CHECK(models[0].at("id") == "alpha" && models[0].at("exists") == true
              && models[0].at("loaded") == false,
              "complete assets report exists and unloaded");
        CHECK(models[1].at("id") == "beta" && models[1].at("exists") == false,
              "empty directory reports missing assets");
        std::filesystem::remove_all(dir);
    }

    std::printf("test_file_start_validation\n");
    {
        auto [session, backend] = make_session();
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "qiqi"}})));
        auto file_body = [](uint64_t handle) {
            nlohmann::json body = start_body(handle);
            body["kind"] = "file";
            body["total_frames"] = 10;
            return body;
        };
        auto rejected = [&](const nlohmann::json& body, const char* msg) {
            CHECK(parse_reply(session->handle(make_message(
                      MOZART_CTL_RVC_START_STREAM, body)))
                      .code == MOZART_CTL_CODE_INVALID_ARGUMENT, msg);
        };
        nlohmann::json body = file_body(1);
        body.erase("total_frames");
        rejected(body, "file without total_frames is rejected");
        body = file_body(1);
        body["total_frames"] = 0;
        rejected(body, "zero total_frames is rejected");
        body = file_body(1);
        body["total_frames"] = -3;
        rejected(body, "negative total_frames is rejected");
        body = file_body(1);
        body["total_frames"] = 1.5;
        rejected(body, "fractional total_frames is rejected");
        body = file_body(1);
        body["total_frames"] = "10";
        rejected(body, "string total_frames is rejected");
        body = file_body(1);
        body["total_frames"] = 4'294'967'296ull;   // u32 契约上限之外
        rejected(body, "total_frames beyond u32 is rejected");
        body = file_body(1);
        body["output_socket"] = "relative.sock";
        rejected(body, "relative output_socket is rejected");
        body = file_body(1);
        body["output_socket"] = 42;
        rejected(body, "non-string output_socket is rejected");

        body = file_body(1);
        body["output_socket"] = "/tmp/mozart-rvc-file-out.sock";
        const Reply ok = parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_START_STREAM, body)));
        CHECK(ok.status == "ok" && ok.data.at("stream_kind") == "file",
              "valid file start returns the stream kind");
        CHECK(!backend->last_realtime
              && backend->last_file_options.total_frames == 10
              && backend->last_file_options.output_socket
                     == "/tmp/mozart-rvc-file-out.sock",
              "backend receives total_frames and the output socket override");
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_STOP_STREAM, nlohmann::json::object())));

        // realtime 忽略 file 字段：即使值非法也不校验。
        body = start_body(1);
        body["total_frames"] = "bogus";
        body["output_socket"] = "relative.sock";
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_START_STREAM, body))).status == "ok",
              "realtime ignores total_frames and output_socket");
    }

    std::printf("test_get_status_snapshot\n");
    {
        auto [session, backend] = make_session();
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "qiqi"}})));
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_START_STREAM, start_body(1))));

        Reply reply = parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_GET_STATUS, nlohmann::json::object())));
        const nlohmann::json& status = reply.data;
        CHECK(reply.status == "ok", "get status answers ok");
        CHECK(status.at("state") == "streaming", "state is streaming");
        CHECK(status.at("model_id") == "qiqi"
              && status.at("model_version") == "v9"
              && status.at("stream_kind") == "realtime",
              "model and stream fields are populated");
        CHECK(status.at("frames_in") == 10 && status.at("frames_out") == 9
              && status.at("frames_dropped") == 1 && status.at("blocks") == 2
              && status.at("queue_depth") == 3,
              "counter fields come from the worker snapshot");
        CHECK(status.at("capabilities") == nlohmann::json({"realtime", "file"}),
              "capabilities field is present");
        CHECK(status.contains("inference_ms_last")
              && status.at("inference_ms_last").is_number(),
              "timing fields present when measured");

        // 停流后：状态回到 loaded，计数保留最终快照。
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_STOP_STREAM, nlohmann::json::object())));
        CHECK(backend->stop_calls.load() >= 1,
              "stop reaches the worker");
        reply = parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_GET_STATUS, nlohmann::json::object())));
        CHECK(reply.data.at("state") == "loaded", "state is loaded after stop");
        CHECK(reply.data.at("stream_kind").is_null(),
              "stream_kind is null after stop");
        CHECK(reply.data.at("frames_out") == 9,
              "final counters stay queryable after stop");
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_STOP_STREAM, nlohmann::json::object())));
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_RVC_STOP_STREAM, nlohmann::json::object())))
                  .status == "ok",
              "stop is idempotent");
    }

    std::printf("test_reset_and_unknown_command\n");
    {
        auto [session, backend] = make_session();
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_LOAD_MODEL, {{"model_id", "qiqi"}})));
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_START_STREAM, start_body(1))));
        parse_reply(session->handle(make_message(
            MOZART_CTL_RVC_RESET_STREAM, nlohmann::json::object())));
        CHECK(backend->reset_calls.load() == 1,
              "reset reaches the active stream");
        CHECK(parse_reply(session->handle(make_message(
                  MOZART_CTL_DAEMON_HELLO, nlohmann::json::object())))
                  .code == MOZART_CTL_CODE_UNKNOWN_COMMAND,
              "unknown command type is rejected");
    }

    std::printf("test_wire_roundtrip_and_version_gate\n");
    {
        // 真实 socket 往返：serve 线程 + 裸 socket 客户端。
        const auto dir = std::filesystem::temp_directory_path()
            / ("mozart-rvc-ctl-" + std::to_string(::getpid()));
        std::filesystem::remove_all(dir);
        std::filesystem::create_directories(dir);
        const auto path = dir / "rvc.sock";
        CtlListener listener(path);
        auto [session, backend] = make_session();
        std::atomic<bool> stop{false};
        std::thread server([&] { session->serve(listener, stop); });

        const int fd = connect_client(path);
        const std::string body = R"({"model_id":"qiqi"})";
        CHECK(send_frame(fd, MOZART_CTL_MAGIC, MOZART_CTL_VERSION,
                        MOZART_CTL_RVC_HELLO, 11,
                        reinterpret_cast<const uint8_t*>(body.data()),
                        body.size()) > 0,
              "hello frame sent");
        uint8_t buffer[MOZART_CTL_HEADER_BYTES + MOZART_CTL_MAX_BODY_BYTES];
        ssize_t n = recv_frame(fd, buffer, sizeof(buffer), 2000);
        CHECK(n > 0, "hello reply arrives");
        ReplyHeader header = parse_frame(buffer, n);
        CHECK(header.version == MOZART_CTL_VERSION
              && header.type == MOZART_CTL_RVC_HELLO
              && header.request_id == 11,
              "reply echoes version, type and request_id");
        CHECK(parse_reply(header.body).data.at("version") == kRvcVersion,
              "wire hello body carries the version");

        // 版本不兼容：拒绝应答后断链。
        CHECK(send_frame(fd, MOZART_CTL_MAGIC, MOZART_CTL_VERSION + 1,
                        MOZART_CTL_RVC_HELLO, 12, nullptr, 0) > 0,
              "mismatched-version frame sent");
        n = recv_frame(fd, buffer, sizeof(buffer), 2000);
        CHECK(n > 0, "version rejection reply arrives");
        header = parse_frame(buffer, n);
        CHECK(parse_reply(header.body).code == MOZART_CTL_CODE_UNSUPPORTED,
              "version mismatch is refused with a reason");
        n = recv_frame(fd, buffer, sizeof(buffer), 2000);
        CHECK(n == 0, "connection is closed after refusal");
        ::close(fd);

        // 新连接：坏帧（坏魔数）直接断链；未知命令回 UnknownCommand。
        const int fd2 = connect_client(path);
        send_frame(fd2, 0xDEAD'BEEFu, MOZART_CTL_VERSION,
                   MOZART_CTL_RVC_HELLO, 13, nullptr, 0);
        n = recv_frame(fd2, buffer, sizeof(buffer), 2000);
        CHECK(n == 0, "bad-magic frame closes the connection");
        ::close(fd2);

        const int fd3 = connect_client(path);
        CHECK(send_frame(fd3, MOZART_CTL_MAGIC, MOZART_CTL_VERSION, 0x0F00, 14,
                         nullptr, 0) > 0,
              "unknown-command frame sent");
        n = recv_frame(fd3, buffer, sizeof(buffer), 2000);
        header = parse_frame(buffer, n);
        CHECK(n > 0
              && parse_reply(header.body).code == MOZART_CTL_CODE_UNKNOWN_COMMAND,
              "unknown command answered over the wire");
        ::close(fd3);

        stop = true;
        server.join();
        std::filesystem::remove_all(dir);
    }

    if (g_fail) {
        std::printf("\n%d test(s) FAILED\n", g_fail);
        return 1;
    }
    std::printf("\nall tests passed\n");
    return 0;
}
