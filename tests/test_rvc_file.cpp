// test_rvc_file.cpp — FileWorker 整段批处理单测（无 GPU 依赖）
// ============================================================================
// 用 mock/假管线驱动「攒帧 → 整段推理 → 回帧」全链，验证 SPEC-03 文件模式：
//   1. N 帧入 → N 帧出：mock 3x 上采样内容逐样本可验证，meta.frame_idx 顺序，
//      blocks=1（整段一次过），批处理开始清零窗计数
//   2. 阻塞输出：对端小缓冲慢读不丢帧（区别于实时非阻塞丢弃）
//   3. 攒帧中取消：StopStream 解除 ReadFrame 阻塞并及时 join
//   4. 推理中取消：检查点抛 InferenceCancelled，worker 内收尾不外抛
//   5. 回帧中取消：阻塞发送被 Close() 解除，及时 join
//   6. 推理失败：原因记入 last_error 供 GetStatus 查询
//   7. 末尾不足一帧补零
#include "rvc/file_worker.hpp"
#include "transport/unix_dgram_stream.h"

#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <filesystem>
#include <functional>
#include <stdexcept>
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

// 对端（daemon 侧）绑定输出 socket；rcvbuf_frames 控制接收缓冲大小。
struct Harness {
    std::filesystem::path dir;
    std::filesystem::path in_path;
    std::filesystem::path out_path;
    int peer_fd = -1;

    Harness(const char* tag, int rcvbuf_frames)
        : dir(std::filesystem::temp_directory_path()
              / ("mozart-rvc-file-" + std::to_string(::getpid()) + "-" + tag))
        , in_path(dir / "rvc-in.sock")
        , out_path(dir / "rvc-out.sock") {
        std::filesystem::remove_all(dir);
        std::filesystem::create_directories(dir);
        peer_fd = ::socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
        int rcv = rcvbuf_frames * static_cast<int>(kOutputPacketBytes);
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

// 全局斜坡输入：第 idx 帧第 i 样本 = 0.001 * (idx*320 + i)，整段可逐样本对拍。
mozart_input_frame_t ramp_frame(uint32_t idx) {
    mozart_input_frame_t frame{};
    frame.meta.frame_idx = idx;
    frame.meta.vad_flag = 1;
    for (int i = 0; i < MOZART_INPUT_SAMPLES; ++i) {
        frame.pcm[i] = 0.001f * static_cast<float>(idx * MOZART_INPUT_SAMPLES + i);
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

// 读一帧输出；超时返回 false。
bool recv_output(int peer_fd, mozart_output_frame_t& frame, int timeout_ms) {
    pollfd pfd{peer_fd, POLLIN, 0};
    if (::poll(&pfd, 1, timeout_ms) <= 0) return false;
    uint8_t packet[kOutputPacketBytes];
    const ssize_t n = ::recv(peer_fd, packet, sizeof(packet), MSG_DONTWAIT);
    if (n < 0) return false;
    return decode_output_packet(packet, static_cast<size_t>(n), frame);
}

// 等待谓词成立；超时返回 false。
bool wait_for(const std::function<bool()>& predicate, int timeout_ms) {
    for (int waited = 0; waited < timeout_ms; waited += 10) {
        if (predicate()) return true;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return predicate();
}

// 推理门控管线：进入 process_cancellable 后置 entered，release 前循环查取消。
class GatedPipeline : public RVCPipelineBase {
public:
    std::vector<float> process(const std::vector<float>& audio) override {
        return audio;
    }
    std::vector<float> process_cancellable(
        const std::vector<float>& audio,
        const std::atomic<bool>& cancelled) override {
        entered_.store(true);
        while (!release_.load()) {
            check_cancelled(&cancelled);
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
        return audio;
    }
    std::atomic<bool> entered_{false};
    std::atomic<bool> release_{false};
};

class ThrowingPipeline : public RVCPipelineBase {
public:
    std::vector<float> process(const std::vector<float>&) override {
        throw std::runtime_error("mock generator exploded");
    }
};

// 固定长度输出管线：2020 样本 = 2 整帧 + 100 样本尾巴（验证补零）。
class OddLengthPipeline : public RVCPipelineBase {
public:
    std::vector<float> process(const std::vector<float>&) override {
        std::vector<float> out(2 * MOZART_OUTPUT_SAMPLES + 100);
        for (size_t i = 0; i < out.size(); ++i) {
            out[i] = 0.5f + static_cast<float>(i) * 0.0001f;
        }
        return out;
    }
};

} // namespace

int main() {
    std::printf("test_batch_n_frames_in_n_out\n");
    {
        MockRVCPipeline pipeline;   // 两批共用，验证窗计数批处理开始清零
        constexpr uint32_t kFrames = 10;
        for (int batch = 0; batch < 2; ++batch) {
            Harness h(batch == 0 ? "batch-a" : "batch-b", 32);
            UnixDgramStream stream(h.in_path, h.out_path, 16,
                                   /*blocking_send=*/true);
            FileWorker worker(stream, pipeline, FileWorker::Config{kFrames});
            worker.start();
            for (uint32_t idx = 0; idx < kFrames; ++idx) {
                send_frame(h.in_path, ramp_frame(idx));
            }
            CHECK(wait_for([&] { return worker.stats().done; }, 5000),
                  "batch completes after total_frames input frames");

            std::vector<mozart_output_frame_t> outputs;
            for (uint32_t f = 0; f < kFrames; ++f) {
                mozart_output_frame_t frame{};
                if (!recv_output(h.peer_fd, frame, 2000)) break;
                outputs.push_back(frame);
            }
            CHECK(outputs.size() == kFrames,
                  "N input frames produce N output frames");
            bool content_ok = true;
            bool meta_ok = true;
            for (uint32_t f = 0; f < outputs.size(); ++f) {
                const auto& frame = outputs[f];
                meta_ok = meta_ok && frame.meta.frame_idx == f
                    && frame.meta.pts_ns
                           == static_cast<uint64_t>(f) * 20'000'000ull
                    && frame.meta.vad_flag == 1;
                for (uint32_t s = 0; s < MOZART_OUTPUT_SAMPLES; ++s) {
                    // mock 3x 上采样：输出第 j 样本 = 输入第 j/3 样本。
                    const float expected = 0.001f * static_cast<float>(
                        (f * MOZART_OUTPUT_SAMPLES + s) / 3);
                    content_ok = content_ok
                        && std::abs(frame.pcm[s] - expected) < 1e-6f;
                }
            }
            CHECK(content_ok, "output content is the whole-segment 3x upsample");
            CHECK(meta_ok, "meta.frame_idx is sequential with output cadence");

            const auto stats = worker.stats();
            CHECK(stats.frames_in == kFrames && stats.frames_out == kFrames,
                  "frame counters match the batch");
            CHECK(stats.blocks == 1,
                  "whole-segment-once counts one window (reset per batch)");
            CHECK(stats.inference_ms_last >= 0.0
                  && stats.inference_ms_avg == stats.inference_ms_last
                  && stats.inference_ms_p95 == stats.inference_ms_last,
                  "single batch inference timing is reported");
            CHECK(stats.last_error.empty(), "successful batch records no error");
            worker.stop();
            CHECK(!stream.IsOpen(), "stop closes the data plane stream");
        }
    }

    std::printf("test_blocking_output_slow_peer\n");
    {
        // 对端缓冲约 2 帧且慢读：非阻塞模式会丢弃，阻塞模式必须全数送达。
        Harness h("slow", 2);
        UnixDgramStream stream(h.in_path, h.out_path, 8,
                               /*blocking_send=*/true);
        MockRVCPipeline pipeline;
        constexpr uint32_t kFrames = 64;
        FileWorker worker(stream, pipeline, FileWorker::Config{kFrames});
        worker.start();
        for (uint32_t idx = 0; idx < kFrames; ++idx) {
            send_frame(h.in_path, ramp_frame(idx));
            std::this_thread::sleep_for(std::chrono::milliseconds(1));
        }

        std::vector<mozart_output_frame_t> outputs;
        std::thread reader([&] {
            for (uint32_t f = 0; f < kFrames; ++f) {
                mozart_output_frame_t frame{};
                if (!recv_output(h.peer_fd, frame, 5000)) break;
                outputs.push_back(frame);
                std::this_thread::sleep_for(std::chrono::milliseconds(2));
            }
        });
        reader.join();
        CHECK(wait_for([&] { return worker.stats().done; }, 5000),
              "batch completes against a slow reader");
        CHECK(outputs.size() == kFrames, "no output frame is dropped");
        bool ordered = true;
        for (uint32_t f = 0; f < outputs.size(); ++f) {
            ordered = ordered && outputs[f].meta.frame_idx == f;
        }
        CHECK(ordered, "frames arrive in order");
        CHECK(stream.stats().send_dropped == 0,
              "blocking send never counts a drop");
        worker.stop();
    }

    std::printf("test_cancel_while_feeding\n");
    {
        Harness h("feed", 8);
        UnixDgramStream stream(h.in_path, h.out_path, 8,
                               /*blocking_send=*/true);
        MockRVCPipeline pipeline;
        FileWorker worker(stream, pipeline, FileWorker::Config{1000});
        worker.start();
        for (uint32_t idx = 0; idx < 3; ++idx) {
            send_frame(h.in_path, ramp_frame(idx));
        }
        CHECK(wait_for([&] { return worker.stats().frames_in == 3; }, 2000),
              "worker accumulates the frames sent so far");
        const auto began = std::chrono::steady_clock::now();
        worker.stop();
        const auto waited = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - began).count();
        CHECK(waited < 1500, "stop unblocks ReadFrame and joins promptly");
        const auto stats = worker.stats();
        CHECK(!stats.done && stats.frames_out == 0 && stats.last_error.empty(),
              "cancel while feeding finishes cleanly without inference");
    }

    std::printf("test_cancel_during_inference\n");
    {
        Harness h("infer", 8);
        UnixDgramStream stream(h.in_path, h.out_path, 8,
                               /*blocking_send=*/true);
        GatedPipeline pipeline;
        FileWorker worker(stream, pipeline, FileWorker::Config{2});
        worker.start();
        send_frame(h.in_path, ramp_frame(0));
        send_frame(h.in_path, ramp_frame(1));
        CHECK(wait_for([&] { return pipeline.entered_.load(); }, 2000),
              "inference starts after total_frames accumulated");
        const auto began = std::chrono::steady_clock::now();
        worker.stop();   // 取消检查点抛 InferenceCancelled，worker 内收尾
        const auto waited = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - began).count();
        CHECK(waited < 1500, "stop responds at the inference checkpoint");
        const auto stats = worker.stats();
        CHECK(!stats.done && stats.frames_out == 0, "no output after cancel");
        CHECK(stats.last_error.empty(), "cancel is not recorded as an error");
    }

    std::printf("test_cancel_during_emission\n");
    {
        // 对端不读：AF_UNIX DGRAM 由发送方 SO_SNDBUF 记账限流，小 SNDBUF
        // 让阻塞发送停在缓冲满处；Close() 必须解除等待。
        Harness h("emit", 1);
        UnixDgramStream stream(h.in_path, h.out_path, 2,
                               /*blocking_send=*/true);
        MockRVCPipeline pipeline;
        constexpr uint32_t kFrames = 100;
        FileWorker worker(stream, pipeline, FileWorker::Config{kFrames});
        worker.start();
        for (uint32_t idx = 0; idx < kFrames; ++idx) {
            send_frame(h.in_path, ramp_frame(idx));
            // 输入缓冲同样只有 2 帧：慢喂避免输入侧溢出。
            std::this_thread::sleep_for(std::chrono::milliseconds(1));
        }
        CHECK(wait_for([&] {
                  const auto s = worker.stats();
                  return s.frames_out >= 1 && s.frames_out < kFrames;
              }, 5000),
              "emission blocks against a full peer buffer");
        const auto began = std::chrono::steady_clock::now();
        worker.stop();
        const auto waited = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - began).count();
        CHECK(waited < 1500, "stop unblocks the blocking send and joins");
        const auto stats = worker.stats();
        CHECK(!stats.done && stats.last_error.empty(),
              "cancel during emission is not a write error");
        CHECK(stream.stats().send_dropped == 0, "no drop is counted");
    }

    std::printf("test_inference_failure_recorded\n");
    {
        Harness h("fail", 8);
        UnixDgramStream stream(h.in_path, h.out_path, 8,
                               /*blocking_send=*/true);
        ThrowingPipeline pipeline;
        FileWorker worker(stream, pipeline, FileWorker::Config{1});
        worker.start();
        send_frame(h.in_path, ramp_frame(0));
        CHECK(wait_for([&] {
                  return !worker.stats().last_error.empty();
              }, 2000),
              "inference failure is recorded for GetStatus");
        const auto stats = worker.stats();
        CHECK(stats.last_error.find("mock generator exploded") != std::string::npos,
              "last_error carries the failure reason");
        CHECK(!stats.done && stats.frames_out == 0, "failed batch emits nothing");
        worker.stop();
    }

    std::printf("test_partial_tail_zero_padded\n");
    {
        Harness h("tail", 8);
        UnixDgramStream stream(h.in_path, h.out_path, 8,
                               /*blocking_send=*/true);
        OddLengthPipeline pipeline;
        FileWorker worker(stream, pipeline, FileWorker::Config{1});
        worker.start();
        send_frame(h.in_path, ramp_frame(0));
        CHECK(wait_for([&] { return worker.stats().done; }, 2000),
              "odd-length output completes");
        std::vector<mozart_output_frame_t> outputs;
        for (int f = 0; f < 3; ++f) {
            mozart_output_frame_t frame{};
            if (!recv_output(h.peer_fd, frame, 2000)) break;
            outputs.push_back(frame);
        }
        CHECK(outputs.size() == 3, "2020 samples emit three frames");
        bool tail_ok = outputs.size() == 3;
        if (tail_ok) {
            for (uint32_t s = 0; s < MOZART_OUTPUT_SAMPLES; ++s) {
                const float expected = s < 100
                    ? 0.5f + static_cast<float>(2 * MOZART_OUTPUT_SAMPLES + s) * 0.0001f
                    : 0.0f;
                tail_ok = tail_ok
                    && std::abs(outputs[2].pcm[s] - expected) < 1e-6f;
            }
        }
        CHECK(tail_ok, "the partial tail frame is zero-padded");
        worker.stop();
    }

    if (g_fail) {
        std::printf("\n%d test(s) FAILED\n", g_fail);
        return 1;
    }
    std::printf("\nall tests passed\n");
    return 0;
}