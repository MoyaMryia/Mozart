// unix_dgram_stream.cpp — 数据面 Unix DGRAM 流实现（见头文件说明）
// 字节序与 core/src/dgram.rs 逐字节对拍（见 tests/test_rvc_transport.cpp）。

#include "transport/unix_dgram_stream.h"

#include <bit>
#include <cerrno>
#include <cstring>
#include <stdexcept>
#include <string>
#include <utility>

#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

namespace rvc {

namespace {

// 目标平台（x86_64 / aarch64 Jetson）均为小端；PCM 直接按字节搬运。
static_assert(std::endian::native == std::endian::little,
              "数据面线包假定小端平台");

void write_u32_le(uint8_t* p, uint32_t v) {
    p[0] = static_cast<uint8_t>(v);
    p[1] = static_cast<uint8_t>(v >> 8);
    p[2] = static_cast<uint8_t>(v >> 16);
    p[3] = static_cast<uint8_t>(v >> 24);
}

uint32_t read_u32_le(const uint8_t* p) {
    return static_cast<uint32_t>(p[0])
         | (static_cast<uint32_t>(p[1]) << 8)
         | (static_cast<uint32_t>(p[2]) << 16)
         | (static_cast<uint32_t>(p[3]) << 24);
}

void write_u64_le(uint8_t* p, uint64_t v) {
    for (int i = 0; i < 8; ++i) {
        p[i] = static_cast<uint8_t>(v >> (8 * i));
    }
}

uint64_t read_u64_le(const uint8_t* p) {
    uint64_t v = 0;
    for (int i = 7; i >= 0; --i) {
        v = (v << 8) | p[i];
    }
    return v;
}

// meta 编解码：线包偏移 4..20。packed 字段按值取出再写，避免非对齐引用。
template <typename Meta>
void encode_meta(const Meta& meta, uint8_t* out) {
    const uint64_t pts_ns = meta.pts_ns;
    const uint32_t frame_idx = meta.frame_idx;
    write_u64_le(out + 4, pts_ns);
    write_u32_le(out + 12, frame_idx);
    out[16] = meta.vad_flag;
    out[17] = meta.energy_db;
    out[18] = meta.conf;
    out[19] = meta.segment_id;
}

template <typename Meta>
Meta decode_meta(const uint8_t* bytes) {
    Meta meta{};
    meta.pts_ns = read_u64_le(bytes + 4);
    meta.frame_idx = read_u32_le(bytes + 12);
    meta.vad_flag = bytes[16];
    meta.energy_db = bytes[17];
    meta.conf = bytes[18];
    meta.segment_id = bytes[19];
    return meta;
}

void encode_pcm(const float* pcm, size_t samples, uint8_t* out) {
    std::memcpy(out + MOZART_PACKET_HEADER_SIZE, pcm, samples * sizeof(float));
}

void decode_pcm(const uint8_t* bytes, size_t samples, float* pcm) {
    std::memcpy(pcm, bytes + MOZART_PACKET_HEADER_SIZE, samples * sizeof(float));
}

int new_dgram_socket() {
    const int fd = ::socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        throw std::runtime_error(
            std::string("socket(AF_UNIX, SOCK_DGRAM): ") + std::strerror(errno));
    }
    return fd;
}

sockaddr_un make_addr(const std::filesystem::path& path, socklen_t& len) {
    auto bytes = path.native();
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    if (bytes.size() >= sizeof(addr.sun_path)) {
        throw std::runtime_error("frame socket path too long: " + path.string());
    }
    std::memcpy(addr.sun_path, bytes.data(), bytes.size());
    len = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + bytes.size() + 1);
    return addr;
}

} // namespace

size_t encode_input_packet(const mozart_input_frame_t& frame,
                           uint8_t* out, size_t cap) {
    if (cap < kInputPacketBytes) return 0;
    write_u32_le(out, MOZART_INPUT_PACKET_MAGIC);
    encode_meta(frame.meta, out);
    encode_pcm(frame.pcm, MOZART_INPUT_SAMPLES, out);
    return kInputPacketBytes;
}

bool decode_input_packet(const uint8_t* bytes, size_t len,
                         mozart_input_frame_t& out) {
    if (len != kInputPacketBytes) return false;
    if (read_u32_le(bytes) != MOZART_INPUT_PACKET_MAGIC) return false;
    out.meta = decode_meta<mozart_frame_meta_t>(bytes);
    decode_pcm(bytes, MOZART_INPUT_SAMPLES, out.pcm);
    return true;
}

size_t encode_output_packet(const mozart_output_frame_t& frame,
                            uint8_t* out, size_t cap) {
    if (cap < kOutputPacketBytes) return 0;
    write_u32_le(out, MOZART_OUTPUT_PACKET_MAGIC);
    encode_meta(frame.meta, out);
    encode_pcm(frame.pcm, MOZART_OUTPUT_SAMPLES, out);
    return kOutputPacketBytes;
}

bool decode_output_packet(const uint8_t* bytes, size_t len,
                           mozart_output_frame_t& out) {
    if (len != kOutputPacketBytes) return false;
    if (read_u32_le(bytes) != MOZART_OUTPUT_PACKET_MAGIC) return false;
    out.meta = decode_meta<mozart_frame_meta_t>(bytes);
    decode_pcm(bytes, MOZART_OUTPUT_SAMPLES, out.pcm);
    return true;
}

UnixDgramStream::UnixDgramStream(const std::filesystem::path& input_path,
                                 const std::filesystem::path& output_path,
                                 size_t queue_frames) {
    // ---- 输入侧：绑定收输入帧 ----
    const int in_fd = new_dgram_socket();
    int value = static_cast<int>(queue_frames * kInputPacketBytes);
    if (::setsockopt(in_fd, SOL_SOCKET, SO_RCVBUF, &value, sizeof(value)) != 0) {
        ::close(in_fd);
        throw std::runtime_error(std::string("SO_RCVBUF: ") + std::strerror(errno));
    }
    std::error_code ignored;
    std::filesystem::remove(input_path, ignored);
    socklen_t in_len = 0;
    const sockaddr_un in_addr = make_addr(input_path, in_len);
    if (::bind(in_fd, reinterpret_cast<const sockaddr*>(&in_addr), in_len) != 0) {
        const std::string reason = std::strerror(errno);
        ::close(in_fd);
        throw std::runtime_error("bind " + input_path.string() + ": " + reason);
    }
    input_fd_.store(in_fd, std::memory_order_release);

    // ---- 输出侧：connect 到 pre 绑定的输出 socket ----
    const int out_fd = new_dgram_socket();
    value = static_cast<int>(queue_frames * kOutputPacketBytes);
    if (::setsockopt(out_fd, SOL_SOCKET, SO_SNDBUF, &value, sizeof(value)) != 0) {
        ::close(out_fd);
        throw std::runtime_error(std::string("SO_SNDBUF: ") + std::strerror(errno));
    }
    peer_addr_ = make_addr(output_path, peer_len_);
    if (::connect(out_fd, reinterpret_cast<const sockaddr*>(&peer_addr_),
                  peer_len_) != 0) {
        const std::string reason = std::strerror(errno);
        ::close(out_fd);
        ::close(input_fd_.exchange(-1, std::memory_order_acq_rel));
        throw std::runtime_error("connect " + output_path.string() + ": " + reason);
    }
    output_fd_.store(out_fd, std::memory_order_release);
}

UnixDgramStream::~UnixDgramStream() {
    Close();
}

bool UnixDgramStream::IsOpen() const noexcept {
    return input_fd_.load(std::memory_order_acquire) >= 0;
}

bool UnixDgramStream::ReadFrame(void* out_frame_buf, size_t buf_size) {
    static_cast<void>(buf_size);  // 契约固定 sizeof(mozart_input_frame_t)，见 audio_stream.h
    auto* frame = static_cast<mozart_input_frame_t*>(out_frame_buf);

    for (;;) {
        const int fd = input_fd_.load(std::memory_order_acquire);
        if (fd < 0) return false;
        uint8_t packet[kInputPacketBytes + 1];
        // 非阻塞排空：坏包（长度/魔数不符）计数丢弃后继续收。
        ssize_t n = ::recv(fd, packet, sizeof(packet), MSG_DONTWAIT);
        while (n >= 0) {
            if (decode_input_packet(packet, static_cast<size_t>(n), *frame)) {
                return true;
            }
            recv_dropped_.fetch_add(1, std::memory_order_relaxed);
            n = ::recv(fd, packet, sizeof(packet), MSG_DONTWAIT);
        }
        if (errno == EBADF || errno == EINVAL) return false;  // 已关闭
        if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) {
            return false;
        }

        // 100 ms poll 等待：Close() 关闭 fd 后以 POLLNVAL 形式解除阻塞。
        pollfd pfd{fd, POLLIN, 0};
        const int rc = ::poll(&pfd, 1, 100);
        if (input_fd_.load(std::memory_order_acquire) < 0) return false;
        if (rc == 0) continue;
        if (rc < 0) {
            if (errno == EINTR) continue;
            return false;
        }
        if (pfd.revents & (POLLERR | POLLHUP | POLLNVAL)) return false;
    }
}

bool UnixDgramStream::WriteFrame(const void* in_frame_buf, size_t buf_size) {
    static_cast<void>(buf_size);  // 契约固定 sizeof(mozart_output_frame_t)
    const int fd = output_fd_.load(std::memory_order_acquire);
    if (fd < 0) return false;
    uint8_t packet[kOutputPacketBytes];
    encode_output_packet(*static_cast<const mozart_output_frame_t*>(in_frame_buf),
                         packet, sizeof(packet));

    for (;;) {
        const ssize_t n = ::send(fd, packet, sizeof(packet),
                                 MSG_DONTWAIT | MSG_NOSIGNAL);
        if (n == static_cast<ssize_t>(sizeof(packet))) return true;
        if (n >= 0) return true;   // 数据报全有或全无，实际不会走到
        switch (errno) {
            case EINTR:
                continue;
            case EAGAIN:
            case ENOBUFS:
                send_dropped_.fetch_add(1, std::memory_order_relaxed);
                return true;
            case ENOENT:
            case ECONNREFUSED:
            case ENOTCONN:
                // 对端不在：丢弃计数并尽力重连，只恢复后续帧通道。
                send_dropped_.fetch_add(1, std::memory_order_relaxed);
                try_reconnect();
                return true;
            default:
                return false;      // 不可恢复：调用方记录原因
        }
    }
}

void UnixDgramStream::Close() {
    const int in_fd = input_fd_.exchange(-1, std::memory_order_acq_rel);
    const int out_fd = output_fd_.exchange(-1, std::memory_order_acq_rel);
    if (in_fd >= 0) ::close(in_fd);
    if (out_fd >= 0) ::close(out_fd);
}

DgramStreamStats UnixDgramStream::stats() const noexcept {
    return {send_dropped_.load(std::memory_order_relaxed),
            recv_dropped_.load(std::memory_order_relaxed)};
}

bool UnixDgramStream::try_reconnect() {
    const int fd = output_fd_.load(std::memory_order_acquire);
    if (fd < 0) return false;
    return ::connect(fd, reinterpret_cast<const sockaddr*>(&peer_addr_),
                     peer_len_) == 0;
}

} // namespace rvc
