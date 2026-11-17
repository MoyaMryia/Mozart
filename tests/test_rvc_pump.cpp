// test_rvc_pump.cpp — AudioWorker 全泵单测（无 GPU 依赖）
// ============================================================================
// 用确定性假管线驱动「收 → StreamingRvc → 发」全链，验证搬运后的
// AudioWorker 与新 Unix DGRAM 数据面的接线：
//   1. mock 管线直通模式：1:1 泵（每帧必回帧，样本为 3x 上采样）
//   2. 流式模式（滑窗 preset）：帧进 = 帧出，块调度异步推进
//   3. stop() 关停次序：close stream 解除阻塞 → join 推理/泵线程
//   4. 实时几何对拍：AudioWorker 按旋钮换算出的分析窗与请求帧数必须等于
//      derive_realtime_geometry 的资产形状（两侧改歪即红）
#include "rvc/audio_worker.hpp"
#include "transport/unix_dgram_stream.h"

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <filesystem>
#include <functional>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

using namespace rvc;

namespace {

int g_fail = 0;
#define CHECK(cond, msg) do { \
    if (cond) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s\n", msg); ++g_fail; } } while (0)

// 数值对拍：失败时打印两侧实际值，直接指出漂移方向。
#define CHECK_NUM(actual, expected, msg) do { \
    const unsigned long long a_ = static_cast<unsigned long long>(actual); \
    const unsigned long long e_ = static_cast<unsigned long long>(expected); \
    if (a_ == e_) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s（实际 %llu，期望 %llu）\n", msg, a_, e_); ++g_fail; } } while (0)

// 假管线：输入最近邻 3x 上采样（确定性；搬运自旧 tests/test_streaming.cpp）
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
    bool is_mock() const override { return false; }   // 强制流式语义
};

class PassThroughPipeline : public RVCPipelineBase {
public:
    std::vector<float> process(const std::vector<float>& audio) override {
        return audio;
    }
    bool is_mock() const override { return true; }   // 直通模式
};

// 实时能力假管线：记录 StreamingRvc 实际下发的分析窗与请求。AudioWorker 把
// ms 旋钮换算成 Config，StreamingRvc 再据此算分析窗；本管线让这条换算链的
// 结果可与 derive_realtime_geometry 的资产形状直接对拍（推理线程写，测试读）。
class RealtimeShapePipeline : public RVCPipelineBase {
public:
    struct Snapshot {
        size_t calls = 0;
        size_t input_samples = 0;
        RvcRealtimeRequest request;
    };

    explicit RealtimeShapePipeline(uint32_t output_sample_rate)
        : sr_(output_sample_rate) {}

    std::vector<float> process(const std::vector<float>& audio) override {
        return std::vector<float>((audio.size() * sr_) / 16000, 0.25f);
    }
    std::vector<float> process_realtime(
        const std::vector<float>& audio, const RvcRealtimeRequest& request) override {
        const std::lock_guard<std::mutex> lock(mutex_);
        ++snapshot_.calls;
        snapshot_.input_samples = audio.size();
        snapshot_.request = request;
        // 镜像真实引擎的形状与覆盖校验（inferencer.cpp 的 frames/shape 检查），
        // 否则 inference_errors == 0 是恒真断言，拦不住几何漂移。
        const size_t frames = audio.size() / kRealtimeFeatureHop;
        const auto geom = derive_realtime_geometry(sr_);
        if (audio.size() % kRealtimeFeatureHop != 0
            || request.skip_head_frames + request.return_frames != frames
            || frames != geom.front_frames
            || request.return_frames != geom.return_frames) {
            throw std::invalid_argument("realtime shape does not match engine contract");
        }
        return std::vector<float>(request.output_samples, 0.25f);
    }
    bool is_mock() const override { return false; }
    bool supports_realtime_streaming() const override { return true; }
    uint32_t realtime_sample_rate() const override { return sr_; }

    Snapshot snapshot() const {
        const std::lock_guard<std::mutex> lock(mutex_);
        return snapshot_;
    }

private:
    uint32_t sr_;
    mutable std::mutex mutex_;
    Snapshot snapshot_;
};

struct Harness {
    std::filesystem::path dir;
    std::filesystem::path in_path;
    std::filesystem::path out_path;
    int peer_fd = -1;

    explicit Harness(const char* tag)
        : dir(std::filesystem::temp_directory_path()
              / ("mozart-rvc-pump-" + std::to_string(::getpid()) + "-" + tag))
        , in_path(dir / "rvc-in.sock")
        , out_path(dir / "rvc-out.sock") {
        std::filesystem::remove_all(dir);
        std::filesystem::create_directories(dir);
        peer_fd = ::socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
        int rcv = 32 * kOutputPacketBytes;
        ::setsockopt(peer_fd, SOL_SOCKET, SO_RCVBUF, &rcv, sizeof(rcv));
        sockaddr_un addr{};
        addr.sun_family = AF_UNIX;
        std::strncpy(addr.sun_path, out_path.c_str(), sizeof(addr.sun_path) - 1);
        socklen_t len = static_cast<socklen_t>(
            offsetof(sockaddr_un, sun_path) + out_path.native().size() + 1);
        if (::bind(peer_fd, reinterpret_cast<const sockaddr*>(&addr), len) != 0) {
            std::perror("bind peer");
            std::exit(1);
        }
    }
    ~Harness() {
        if (peer_fd >= 0) ::close(peer_fd);
        std::error_code ignored;
        std::filesystem::remove_all(dir, ignored);
    }
};

mozart_input_frame_t input_frame(uint32_t idx, uint8_t vad) {
    mozart_input_frame_t frame{};
    frame.meta.pts_ns = 1'000'000'000ull + idx * 20'000'000ull;
    frame.meta.frame_idx = idx;
    frame.meta.vad_flag = vad;
    frame.meta.segment_id = 1;
    for (int i = 0; i < MOZART_INPUT_SAMPLES; ++i) {
        frame.pcm[i] = 0.1f + static_cast<float>(i) * 0.001f;
    }
    return frame;
}

void send_frame(const std::filesystem::path& path,
                const mozart_input_frame_t& frame) {
    uint8_t packet[kInputPacketBytes];
    encode_input_packet(frame, packet, sizeof(packet));
    const int fd = ::socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, path.c_str(), sizeof(addr.sun_path) - 1);
    socklen_t len = static_cast<socklen_t>(
        offsetof(sockaddr_un, sun_path) + path.native().size() + 1);
    ::sendto(fd, packet, sizeof(packet), 0,
             reinterpret_cast<const sockaddr*>(&addr), len);
    ::close(fd);
}

std::vector<mozart_output_frame_t> drain_outputs(
    int peer_fd, std::atomic<size_t>& wanted) {
    std::vector<mozart_output_frame_t> frames;
    while (frames.size() < wanted.load()) {
        uint8_t packet[kOutputPacketBytes];
        const ssize_t n = ::recv(peer_fd, packet, sizeof(packet), MSG_DONTWAIT);
        if (n >= 0) {
            mozart_output_frame_t frame{};
            if (decode_output_packet(packet, static_cast<size_t>(n), frame)) {
                frames.push_back(frame);
            }
            continue;
        }
        pollfd pfd{peer_fd, POLLIN, 0};
        if (::poll(&pfd, 1, 2000) == 0) break;
    }
    return frames;
}

// 等待谓词成立；超时返回 false。
bool wait_for(const std::function<bool()>& predicate, int timeout_ms) {
    for (int waited = 0; waited < timeout_ms; waited += 10) {
        if (predicate()) return true;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return predicate();
}

} // namespace

int main() {
    std::printf("test_mock_pipeline_passthrough_pump\n");
    {
        Harness h("mock");
        UnixDgramStream stream(h.in_path, h.out_path, 8);
        PassThroughPipeline pipeline;
        AudioWorker worker(stream, pipeline, AudioWorker::Config{});
        worker.start();
        CHECK(worker.running(), "worker runs in frame mode");

        std::atomic<size_t> wanted{5};
        std::vector<mozart_output_frame_t> outputs;
        std::thread reader([&] { outputs = drain_outputs(h.peer_fd, wanted); });
        for (uint32_t idx = 1; idx <= 5; ++idx) {
            send_frame(h.in_path, input_frame(idx, 1));
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
        reader.join();
        CHECK(outputs.size() == 5, "1:1 pump returns one frame per input");

        worker.stop();
        CHECK(!worker.running(), "worker stops");
        const auto stats = worker.get_stream_stats();
        CHECK(stats.frames_in == 5 && stats.frames_out == 5,
              "frame counters match");
        CHECK(stats.blocks == 0, "mock pipeline does no block inference");
    }

    std::printf("test_streaming_pump_sliding_window\n");
    {
        Harness h("stream");
        UnixDgramStream stream(h.in_path, h.out_path, 8);
        Upsample3Pipeline pipeline;
        AudioWorker::Config cfg;
        cfg.realtime_kind = true;   // 无实时引擎 → 内部按能力退到 2s 滑窗 preset
        AudioWorker worker(stream, pipeline, cfg);
        worker.start();
        CHECK(worker.running(), "worker runs in stream mode");

        constexpr size_t kFrames = 150;
        std::atomic<size_t> wanted{kFrames};
        std::vector<mozart_output_frame_t> outputs;
        std::thread reader([&] { outputs = drain_outputs(h.peer_fd, wanted); });
        for (uint32_t idx = 1; idx <= kFrames; ++idx) {
            send_frame(h.in_path, input_frame(idx, 1));
            std::this_thread::sleep_for(std::chrono::milliseconds(2));
        }
        reader.join();
        CHECK(outputs.size() == kFrames,
              "streaming pump emits one frame per input");
        CHECK(outputs[0].meta.frame_idx == 1,
              "output meta echoes input meta");
        CHECK(wait_for([&] { return worker.get_stream_stats().blocks >= 1; }, 5000),
              "first sliding-window block completes asynchronously");

        const auto stats = worker.get_stream_stats();
        // 首块就绪前每帧欠载补零：窗口 100 帧攒块，至少 90 帧欠载。
        CHECK(stats.output_underruns >= 90,
              "pre-first-block frames are zero-padded and counted");
        CHECK(stats.frames_in == kFrames && stats.frames_out == kFrames,
              "frame counters match");
        CHECK(stats.blocks >= 1, "block counter visible while running");
        worker.stop();
    }

    std::printf("test_stop_unblocks_idle_pump\n");
    {
        Harness h("idle");
        UnixDgramStream stream(h.in_path, h.out_path, 8);
        Upsample3Pipeline pipeline;
        AudioWorker worker(stream, pipeline, AudioWorker::Config{});
        worker.start();
        // 无输入帧：泵线程阻塞在 ReadFrame。stop() 必须按次序解除阻塞并 join。
        const auto started = std::chrono::steady_clock::now();
        worker.stop();
        const auto waited = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - started).count();
        CHECK(!worker.running() && waited < 1500,
              "stop closes the stream and joins promptly");
        CHECK(!stream.IsOpen(), "stop closes the data plane stream");
    }

    // ---- 实时几何对拍：旋钮换算链 vs 资产形状 ----
    // 引擎资产按最大 overlap 固定（front_frames/return_frames），AudioWorker 却
    // 按旋钮值喂 StreamingRvc。两者一旦不一致，推理层每块抛异常、输出补零。
    // 这里走真实换算链（AudioWorker→StreamingRvc→process_realtime），把两侧钉住。
    std::printf("test_realtime_knob_geometry_matches_engine_shape\n");
    for (uint32_t sr : {32000u, 40000u, 48000u}) {
        const std::string tag = "rtgeom-" + std::to_string(sr);
        Harness h(tag.c_str());
        UnixDgramStream stream(h.in_path, h.out_path, 8);
        RealtimeShapePipeline pipeline(sr);
        AudioWorker::Config cfg;   // 规范默认旋钮：overlap 锁为 60 ms
        AudioWorker worker(stream, pipeline, cfg);
        worker.start();
        const auto geom = derive_realtime_geometry(sr);

        // 12 帧 = 240 ms，恰好攒满一个实时块。入站 SO_RCVBUF 只有 8 帧，
        // 无间隔连发会偶发丢帧致超时假红，故留 2 ms 节拍。
        for (uint32_t idx = 1; idx <= 12; ++idx) {
            send_frame(h.in_path, input_frame(idx, 1));
            std::this_thread::sleep_for(std::chrono::milliseconds(2));
        }
        CHECK(wait_for([&] { return pipeline.snapshot().calls >= 1; }, 5000),
              "realtime block reaches the pipeline");
        const auto snap = pipeline.snapshot();
        CHECK_NUM(snap.input_samples / kRealtimeFeatureHop, geom.front_frames,
                  "分析窗帧数 == 资产 front_frames");
        CHECK_NUM(snap.request.block_samples_16k, geom.window_samples,
                  "请求块长 == 资产 window_samples");
        CHECK_NUM(snap.request.skip_head_frames, geom.skip_head,
                  "请求 skip_head == 资产 skip_head");
        CHECK_NUM(snap.request.return_frames, geom.return_frames,
                  "请求 return_frames == 资产 return_frames");
        CHECK_NUM(snap.request.output_samples, geom.decoder_out_samples,
                  "请求输出样本 == 资产 decoder_out_samples");
        // 推理层自身的覆盖校验：skip + return 必须铺满分析窗。
        CHECK_NUM(snap.request.skip_head_frames + snap.request.return_frames,
                  snap.input_samples / kRealtimeFeatureHop,
                  "skip + return 铺满分析窗");
        CHECK(worker.get_stream_stats().inference_errors == 0,
              "合法旋钮下无推理失败");
        worker.stop();
    }

    if (g_fail) {
        std::printf("\n%d test(s) FAILED\n", g_fail);
        return 1;
    }
    std::printf("\nall tests passed\n");
    return 0;
}
