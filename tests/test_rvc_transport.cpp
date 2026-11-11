// test_rvc_transport.cpp — rvc 数据面单测（无 GPU 依赖）
// ============================================================================
// 1. 线包字节序与 core/src/dgram.rs 逐字节对拍（金标字节手写）
// 2. 手工线包解码不依赖编码路径
// 3. 绑定/连接往返：帧内容与 meta 逐字段一致
// 4. 坏包（坏魔数/短包/超长包）计数丢弃且好包仍可达
// 5. 对端接收停摆时发送丢弃计数
// 6. ReadFrame 阻塞可被 Close 解除
// 7. 对端重启后自动重连，后续帧恢复送达
#include "transport/unix_dgram_stream.h"

#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <filesystem>
#include <string>
#include <thread>

#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

using namespace rvc;

namespace {

int g_fail = 0;
#define CHECK(cond, msg) do { \
    if (cond) std::printf("  ok  %s\n", msg); \
    else { std::printf("  FAIL %s\n", msg); ++g_fail; } } while (0)

struct Sockets {
    std::filesystem::path dir;
    std::filesystem::path in_path;
    std::filesystem::path out_path;
    int peer_out_fd = -1;   // 测试扮演 pre：绑定 output 收 rvc 输出

    Sockets(const char* tag)
        : dir(std::filesystem::temp_directory_path()
              / ("mozart-rvc-transport-" + std::to_string(::getpid()) + "-" + tag))
        , in_path(dir / "rvc-in.sock")
        , out_path(dir / "rvc-out.sock") {
        std::filesystem::remove_all(dir);
        std::filesystem::create_directories(dir);
        peer_out_fd = ::socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
        sockaddr_un addr{};
        addr.sun_family = AF_UNIX;
        std::strncpy(addr.sun_path, out_path.c_str(), sizeof(addr.sun_path) - 1);
        socklen_t len = static_cast<socklen_t>(
            offsetof(sockaddr_un, sun_path) + out_path.native().size() + 1);
        if (::bind(peer_out_fd, reinterpret_cast<const sockaddr*>(&addr), len) != 0) {
            std::perror("bind peer out");
            std::exit(1);
        }
        int rcv = 8 * kOutputPacketBytes;
        ::setsockopt(peer_out_fd, SOL_SOCKET, SO_RCVBUF, &rcv, sizeof(rcv));
    }
    ~Sockets() {
        if (peer_out_fd >= 0) ::close(peer_out_fd);
        std::error_code ignored;
        std::filesystem::remove_all(dir, ignored);
    }
};

// 原始 socket 发送任意字节到 rvc 绑定的输入路径（绕过编码路径）。
void send_raw(const std::filesystem::path& path, const uint8_t* bytes, size_t n) {
    const int fd = ::socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, path.c_str(), sizeof(addr.sun_path) - 1);
    socklen_t len = static_cast<socklen_t>(
        offsetof(sockaddr_un, sun_path) + path.native().size() + 1);
    const ssize_t sent = ::sendto(fd, bytes, n, MSG_DONTWAIT,
                                  reinterpret_cast<const sockaddr*>(&addr), len);
    if (sent != static_cast<ssize_t>(n)) {
        std::perror("send_raw");
        std::exit(1);
    }
    ::close(fd);
}

mozart_input_frame_t sample_input(uint32_t idx) {
    mozart_input_frame_t frame{};
    frame.meta.pts_ns = 0x0102'0304'0506'0708ull + idx;
    frame.meta.frame_idx = idx;
    frame.meta.vad_flag = 1;
    frame.meta.energy_db = 2;
    frame.meta.conf = 3;
    frame.meta.segment_id = 4;
    for (int i = 0; i < MOZART_INPUT_SAMPLES; ++i) {
        frame.pcm[i] = i * 0.5f + idx;
    }
    return frame;
}

mozart_output_frame_t sample_output(uint32_t idx) {
    mozart_output_frame_t frame{};
    frame.meta = sample_input(idx).meta;
    for (int i = 0; i < MOZART_OUTPUT_SAMPLES; ++i) {
        frame.pcm[i] = i * 0.25f + idx;
    }
    return frame;
}

bool same_meta(const mozart_frame_meta_t& a, const mozart_frame_meta_t& b) {
    return a.pts_ns == b.pts_ns && a.frame_idx == b.frame_idx
        && a.vad_flag == b.vad_flag && a.energy_db == b.energy_db
        && a.conf == b.conf && a.segment_id == b.segment_id;
}

} // namespace

int main() {
    std::printf("test_wire_bytes_match_rust\n");
    {
        // 与 core/src/dgram.rs 的 input_packet_bytes_are_little_endian 对拍。
        mozart_input_frame_t frame{};
        frame.meta.pts_ns = 0x8877'6655'4433'2211ull;
        frame.meta.frame_idx = 0xD4D3'D2D1u;
        frame.meta.vad_flag = 1;
        frame.meta.energy_db = 2;
        frame.meta.conf = 3;
        frame.meta.segment_id = 4;
        frame.pcm[0] = -1.5f;
        frame.pcm[MOZART_INPUT_SAMPLES - 1] = 0.25f;
        uint8_t packet[kInputPacketBytes];
        CHECK(encode_input_packet(frame, packet, sizeof(packet)) == 1300,
              "input packet is 1300 bytes");
        static const uint8_t magic_le[4] = {0x54, 0x52, 0x5A, 0x4D};
        static const uint8_t pts_le[8] = {0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88};
        static const uint8_t idx_le[4] = {0xD1, 0xD2, 0xD3, 0xD4};
        static const uint8_t pcm0_le[4] = {0x00, 0x00, 0xC0, 0xBF};
        static const uint8_t pcm_tail_le[4] = {0x00, 0x00, 0x80, 0x3E};
        CHECK(std::memcmp(packet, magic_le, 4) == 0, "magic is little endian");
        CHECK(std::memcmp(packet + 4, pts_le, 8) == 0, "pts_ns is little endian");
        CHECK(std::memcmp(packet + 12, idx_le, 4) == 0, "frame_idx is little endian");
        CHECK(std::memcmp(packet + 16, "\x01\x02\x03\x04", 4) == 0,
              "meta bytes are in order");
        CHECK(std::memcmp(packet + 20, pcm0_le, 4) == 0, "pcm[0] = -1.5f LE");
        CHECK(std::memcmp(packet + kInputPacketBytes - 4, pcm_tail_le, 4) == 0,
              "pcm[319] = 0.25f LE");

        // 输出线包金标（与 Rust 对拍用例同值）。
        mozart_output_frame_t out{};
        out.meta = frame.meta;
        out.pcm[0] = 1.0f;
        out.pcm[MOZART_OUTPUT_SAMPLES - 1] = -0.5f;
        uint8_t out_packet[kOutputPacketBytes];
        CHECK(encode_output_packet(out, out_packet, sizeof(out_packet)) == 3860,
              "output packet is 3860 bytes");
        CHECK(std::memcmp(out_packet, "\x4D\x5A\x52\x54", 4) == 0,
              "output magic is little endian");
        CHECK(std::memcmp(out_packet + 20, "\x00\x00\x80\x3F", 4) == 0,
              "pcm[0] = 1.0f LE");
        CHECK(std::memcmp(out_packet + kOutputPacketBytes - 4, "\x00\x00\x00\xBF", 4) == 0,
              "pcm[959] = -0.5f LE");
    }

    std::printf("test_decode_hand_crafted_packet\n");
    {
        uint8_t packet[kInputPacketBytes]{};
        // 魔数小端：'MZRT' = 0x4D5A5254。
        packet[0] = 0x54; packet[1] = 0x52; packet[2] = 0x5A; packet[3] = 0x4D;
        const uint64_t pts = 0x8877'6655'4433'2211ull;
        for (int i = 0; i < 8; ++i) {
            packet[4 + i] = static_cast<uint8_t>(pts >> (8 * i));
        }
        const uint32_t idx = 0xD4D3'D2D1u;
        for (int i = 0; i < 4; ++i) {
            packet[12 + i] = static_cast<uint8_t>(idx >> (8 * i));
        }
        packet[16] = 1; packet[17] = 2; packet[18] = 3; packet[19] = 4;
        const float first = -1.5f;
        std::memcpy(packet + 20, &first, 4);
        mozart_input_frame_t frame{};
        CHECK(decode_input_packet(packet, sizeof(packet), frame),
              "hand-crafted packet decodes");
        CHECK(frame.meta.pts_ns == 0x8877'6655'4433'2211ull
              && frame.meta.frame_idx == 0xD4D3'D2D1u && frame.pcm[0] == -1.5f,
              "decoded fields match");
        CHECK(!decode_input_packet(packet, sizeof(packet) - 1, frame),
              "short packet is rejected");
        packet[0] ^= 0xFF;
        CHECK(!decode_input_packet(packet, sizeof(packet), frame),
              "bad magic is rejected");
    }

    std::printf("test_bind_connect_roundtrip\n");
    {
        Sockets s("round");
        UnixDgramStream stream(s.in_path, s.out_path, 8);
        CHECK(stream.IsOpen(), "stream is open after bind/connect");

        const mozart_input_frame_t sent = sample_input(3);
        uint8_t packet[kInputPacketBytes];
        encode_input_packet(sent, packet, sizeof(packet));
        send_raw(s.in_path, packet, sizeof(packet));
        mozart_input_frame_t got{};
        CHECK(stream.ReadFrame(&got, sizeof(got)), "ReadFrame returns the frame");
        CHECK(same_meta(got.meta, sent.meta), "input meta survives the wire");
        CHECK(std::memcmp(got.pcm, sent.pcm, sizeof(sent.pcm)) == 0,
              "input pcm survives the wire");

        const mozart_output_frame_t out = sample_output(4);
        CHECK(stream.WriteFrame(&out, sizeof(out)), "WriteFrame succeeds");
        uint8_t out_packet[kOutputPacketBytes];
        const ssize_t n = ::recv(s.peer_out_fd, out_packet, sizeof(out_packet), 0);
        mozart_output_frame_t out_got{};
        CHECK(n == static_cast<ssize_t>(kOutputPacketBytes)
              && decode_output_packet(out_packet, static_cast<size_t>(n), out_got),
              "peer receives a valid output packet");
        CHECK(same_meta(out_got.meta, out.meta)
              && std::memcmp(out_got.pcm, out.pcm, sizeof(out.pcm)) == 0,
              "output frame survives the wire");
        CHECK(stream.stats().send_dropped == 0
              && stream.stats().recv_dropped == 0, "no drops on healthy path");
    }

    std::printf("test_bad_packets_are_counted_and_skipped\n");
    {
        Sockets s("badpkt");
        UnixDgramStream stream(s.in_path, s.out_path, 8);

        uint8_t wrong[kInputPacketBytes]{};
        wrong[0] = 0xFF; wrong[1] = 0xFF; wrong[2] = 0xFF; wrong[3] = 0xFF;
        send_raw(s.in_path, wrong, sizeof(wrong));          // 坏魔数（等长）
        send_raw(s.in_path, wrong, 100);                    // 短包
        uint8_t over[kInputPacketBytes + 64]{};
        over[0] = 0x54; over[1] = 0x52; over[2] = 0x5A; over[3] = 0x4D;
        send_raw(s.in_path, over, sizeof(over));            // 超长包

        const mozart_input_frame_t sent = sample_input(5);
        uint8_t packet[kInputPacketBytes];
        encode_input_packet(sent, packet, sizeof(packet));
        send_raw(s.in_path, packet, sizeof(packet));
        mozart_input_frame_t got{};
        CHECK(stream.ReadFrame(&got, sizeof(got)), "good frame arrives after bad ones");
        CHECK(got.meta.frame_idx == 5, "good frame is intact");
        CHECK(stream.stats().recv_dropped == 3, "three malformed packets counted");
    }

    std::printf("test_send_counts_drops_when_peer_stalls\n");
    {
        Sockets s("stall");
        UnixDgramStream stream(s.in_path, s.out_path, 4);
        // 对端（peer_out_fd）绑定后不收：发送方按 SO_SNDBUF 记账丢弃。
        for (uint32_t i = 0; i < 32; ++i) {
            const mozart_output_frame_t out = sample_output(i);
            CHECK(stream.WriteFrame(&out, sizeof(out)), "send never blocks");
        }
        // 4 帧 SNDBUF 下实测入队约 5 条；保守断言下限（同 Rust 用例）。
        CHECK(stream.stats().send_dropped >= 16,
              "send drops counted when the peer stalls");
        CHECK(stream.stats().recv_dropped == 0, "recv path untouched");
        // 队列保持 FIFO：最早送出的 0 号帧未被丢弃。
        uint8_t packet[kOutputPacketBytes];
        const ssize_t n = ::recv(s.peer_out_fd, packet, sizeof(packet), MSG_DONTWAIT);
        mozart_output_frame_t got{};
        CHECK(n == static_cast<ssize_t>(kOutputPacketBytes)
              && decode_output_packet(packet, static_cast<size_t>(n), got)
              && got.meta.frame_idx == 0, "oldest queued frame survives");
    }

    std::printf("test_close_unblocks_read_frame\n");
    {
        Sockets s("close");
        UnixDgramStream stream(s.in_path, s.out_path, 8);
        std::atomic<bool> returned{false};
        std::thread reader([&] {
            mozart_input_frame_t frame{};
            if (!stream.ReadFrame(&frame, sizeof(frame))) returned = true;
        });
        std::this_thread::sleep_for(std::chrono::milliseconds(150));
        CHECK(!returned.load(), "ReadFrame blocks while no data arrives");
        stream.Close();
        // poll 步长 100 ms，关停应在 ~200 ms 内解除阻塞。
        const auto started = std::chrono::steady_clock::now();
        reader.join();
        const auto waited = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - started).count();
        CHECK(returned.load() && waited < 1000,
              "Close unblocks ReadFrame promptly");
        CHECK(!stream.IsOpen(), "stream is closed");
        // 关停后读写立即失败，不再触碰已关闭 fd。
        mozart_input_frame_t frame{};
        CHECK(!stream.ReadFrame(&frame, sizeof(frame)), "ReadFrame false after close");
        const mozart_output_frame_t out = sample_output(1);
        CHECK(!stream.WriteFrame(&out, sizeof(out)), "WriteFrame false after close");
    }

    std::printf("test_reconnect_after_peer_restart\n");
    {
        Sockets s("restart");
        UnixDgramStream stream(s.in_path, s.out_path, 4);
        const mozart_output_frame_t first = sample_output(1);
        CHECK(stream.WriteFrame(&first, sizeof(first)), "first frame delivered");
        uint8_t packet[kOutputPacketBytes];
        ssize_t n = ::recv(s.peer_out_fd, packet, sizeof(packet), MSG_DONTWAIT);
        CHECK(n == static_cast<ssize_t>(kOutputPacketBytes), "peer got first frame");

        // 对端关闭后 socket 文件留在原地：发送丢弃计数。
        ::close(s.peer_out_fd);
        s.peer_out_fd = -1;
        const mozart_output_frame_t lost = sample_output(2);
        CHECK(stream.WriteFrame(&lost, sizeof(lost)), "lost frame drops silently");
        const mozart_output_frame_t lost2 = sample_output(3);
        CHECK(stream.WriteFrame(&lost2, sizeof(lost2)), "second lost frame drops");
        CHECK(stream.stats().send_dropped == 2, "restart window drops counted");

        // 对端重绑：先清理陈旧 socket 文件（同 Rust bind 语义），发送方
        // 自动重连后恢复送达。
        std::filesystem::remove(s.out_path);
        s.peer_out_fd = ::socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
        sockaddr_un addr{};
        addr.sun_family = AF_UNIX;
        std::strncpy(addr.sun_path, s.out_path.c_str(), sizeof(addr.sun_path) - 1);
        socklen_t len = static_cast<socklen_t>(
            offsetof(sockaddr_un, sun_path) + s.out_path.native().size() + 1);
        CHECK(::bind(s.peer_out_fd, reinterpret_cast<const sockaddr*>(&addr), len) == 0,
              "peer rebinds");
        const mozart_output_frame_t again = sample_output(4);
        stream.WriteFrame(&again, sizeof(again));   // 仍走失败分支：丢弃 + 重连成功
        const mozart_output_frame_t live = sample_output(5);
        CHECK(stream.WriteFrame(&live, sizeof(live)), "channel restored");
        n = ::recv(s.peer_out_fd, packet, sizeof(packet), MSG_DONTWAIT);
        mozart_output_frame_t got{};
        CHECK(n == static_cast<ssize_t>(kOutputPacketBytes)
              && decode_output_packet(packet, static_cast<size_t>(n), got)
              && got.meta.frame_idx == 5, "post-restart frame arrives");
        CHECK(stream.stats().send_dropped == 3,
              "first post-restart send still drops (window semantics)");
    }

    if (g_fail) {
        std::printf("\n%d test(s) FAILED\n", g_fail);
        return 1;
    }
    std::printf("\nall tests passed\n");
    return 0;
}
