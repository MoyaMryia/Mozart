// ctl_link.cpp — 控制面 SEQPACKET 传输实现（字节序与 core/src/ctl.rs 对拍）
#include "control/ctl_link.h"

#include <cerrno>
#include <cstring>
#include <stdexcept>
#include <string>

#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include "mozart/ctl.h"

namespace rvc {

namespace {

constexpr int kAcceptPollMs = 100;

sockaddr_un make_addr(const std::filesystem::path& path, socklen_t& len) {
    const auto bytes = path.native();
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    if (bytes.size() >= sizeof(addr.sun_path)) {
        throw std::runtime_error("control socket path too long: " + path.string());
    }
    std::memcpy(addr.sun_path, bytes.data(), bytes.size());
    len = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + bytes.size() + 1);
    return addr;
}

// 小端装拆（头为 packed 结构体，字段逐个搬避免非对齐引用）。
mozart_ctl_header_t decode_header(const uint8_t* bytes) {
    mozart_ctl_header_t header{};
    auto u32 = [&bytes](size_t off) {
        return static_cast<uint32_t>(bytes[off])
             | (static_cast<uint32_t>(bytes[off + 1]) << 8)
             | (static_cast<uint32_t>(bytes[off + 2]) << 16)
             | (static_cast<uint32_t>(bytes[off + 3]) << 24);
    };
    auto u16 = [&bytes](size_t off) {
        return static_cast<uint16_t>(bytes[off]
                                    | (static_cast<uint16_t>(bytes[off + 1]) << 8));
    };
    header.magic = u32(0);
    header.version = u16(4);
    header.type = u16(6);
    header.request_id = u32(8);
    header.body_len = u32(12);
    return header;
}

void encode_header(uint8_t* bytes, uint16_t type, uint32_t request_id,
                   uint32_t body_len) {
    auto put32 = [&bytes](size_t off, uint32_t v) {
        bytes[off] = static_cast<uint8_t>(v);
        bytes[off + 1] = static_cast<uint8_t>(v >> 8);
        bytes[off + 2] = static_cast<uint8_t>(v >> 16);
        bytes[off + 3] = static_cast<uint8_t>(v >> 24);
    };
    auto put16 = [&bytes](size_t off, uint16_t v) {
        bytes[off] = static_cast<uint8_t>(v);
        bytes[off + 1] = static_cast<uint8_t>(v >> 8);
    };
    put32(0, MOZART_CTL_MAGIC);
    put16(4, MOZART_CTL_VERSION);
    put16(6, type);
    put32(8, request_id);
    put32(12, body_len);
}

// poll 等待可读；0 = 超时，-1 = 出错，1 = 可读。EINTR 重试。
int wait_readable(int fd, int timeout_ms) {
    pollfd pfd{fd, POLLIN, 0};
    for (;;) {
        const int rc = ::poll(&pfd, 1, timeout_ms);
        if (rc >= 0) return rc;
        if (errno == EINTR) continue;
        return -1;
    }
}

} // namespace

CtlConnection::CtlConnection(int fd) : fd_(fd) {
    // 非阻塞 + poll：接收等待由 wait_readable 承担超时语义。
    const int flags = ::fcntl(fd_, F_GETFL, 0);
    if (flags >= 0) (void)::fcntl(fd_, F_SETFL, flags | O_NONBLOCK);
}

CtlConnection::~CtlConnection() {
    ::close(fd_);
}

CtlRecv CtlConnection::recv(CtlMessage& out, int timeout_ms) {
    // 帧上限：头 + MAX_BODY；超长帧被截断后按长度不符拒绝。
    uint8_t buffer[MOZART_CTL_HEADER_BYTES + MOZART_CTL_MAX_BODY_BYTES + 1];
    for (;;) {
        const ssize_t n = ::recv(fd_, buffer, sizeof(buffer), 0);
        if (n < 0) {
            if (errno == EINTR) continue;
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                const int rc = wait_readable(fd_, timeout_ms);
                if (rc == 0) return CtlRecv::Timeout;
                if (rc < 0) return CtlRecv::ProtocolError;
                continue;
            }
            return CtlRecv::ProtocolError;
        }
        if (n == 0) return CtlRecv::Closed;
        if (n < MOZART_CTL_HEADER_BYTES) return CtlRecv::ProtocolError;
        const mozart_ctl_header_t header =
            decode_header(buffer);
        if (header.magic != MOZART_CTL_MAGIC) return CtlRecv::ProtocolError;
        if (header.body_len > MOZART_CTL_MAX_BODY_BYTES) return CtlRecv::ProtocolError;
        if (header.body_len
            != static_cast<uint32_t>(n - MOZART_CTL_HEADER_BYTES)) {
            return CtlRecv::ProtocolError;
        }
        if (header.version != MOZART_CTL_VERSION) return CtlRecv::VersionError;
        out.type = header.type;
        out.request_id = header.request_id;
        out.body.assign(buffer + MOZART_CTL_HEADER_BYTES, buffer + n);
        return CtlRecv::Ok;
    }
}

bool CtlConnection::send(uint16_t type, uint32_t request_id,
                         const uint8_t* body, size_t len) {
    if (len > MOZART_CTL_MAX_BODY_BYTES) return false;
    uint8_t buffer[MOZART_CTL_HEADER_BYTES + MOZART_CTL_MAX_BODY_BYTES];
    encode_header(buffer, type, request_id, static_cast<uint32_t>(len));
    std::memcpy(buffer + MOZART_CTL_HEADER_BYTES, body, len);
    const size_t total = MOZART_CTL_HEADER_BYTES + len;
    for (;;) {
        const ssize_t sent = ::send(fd_, buffer, total, MSG_NOSIGNAL);
        if (sent == static_cast<ssize_t>(total)) return true;
        if (sent >= 0) return false;
        if (errno == EINTR) continue;
        return false;
    }
}

CtlListener::CtlListener(const std::filesystem::path& path) {
    fd_ = ::socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (fd_ < 0) {
        throw std::runtime_error(
            std::string("socket(SOCK_SEQPACKET): ") + std::strerror(errno));
    }
    std::error_code ignored;
    std::filesystem::remove(path, ignored);
    socklen_t len = 0;
    const sockaddr_un addr = make_addr(path, len);
    if (::bind(fd_, reinterpret_cast<const sockaddr*>(&addr), len) != 0) {
        const std::string reason = std::strerror(errno);
        ::close(fd_);
        throw std::runtime_error("bind " + path.string() + ": " + reason);
    }
    if (::listen(fd_, 4) != 0) {
        const std::string reason = std::strerror(errno);
        ::close(fd_);
        throw std::runtime_error("listen " + path.string() + ": " + reason);
    }
}

CtlListener::~CtlListener() {
    ::close(fd_);
}

std::unique_ptr<CtlConnection> CtlListener::accept(const std::atomic<bool>& stop) {
    while (!stop.load(std::memory_order_relaxed)) {
        const int rc = wait_readable(fd_, kAcceptPollMs);
        if (rc == 0) continue;
        if (rc < 0) return nullptr;
        for (;;) {
            const int fd = ::accept4(fd_, nullptr, nullptr, SOCK_CLOEXEC);
            if (fd >= 0) return std::make_unique<CtlConnection>(fd);
            if (errno == EINTR) continue;
            return nullptr;
        }
    }
    return nullptr;
}

} // namespace rvc
