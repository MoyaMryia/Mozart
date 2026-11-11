// ctl_link.h — 控制面 SEQPACKET 传输（rvc 侧）
// ============================================================================
// 对应 core/src/ctl.rs 的 Seqpacket 服务端子集：信封帧（16 B 头 + JSON 体）、
// bind/listen/accept、按超时接收。头字段布局由 core/include/mozart/ctl.h 定义，
// 本文件只做帧的装拆与校验，不重复契约常量。
#ifndef RVC_CTL_LINK_H
#define RVC_CTL_LINK_H

#include <atomic>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <vector>

namespace rvc {

struct CtlMessage {
    uint16_t type = 0;
    uint32_t request_id = 0;
    std::vector<uint8_t> body;
};

enum class CtlRecv {
    Ok,             // 收到一条合法消息
    Closed,         // 对端关闭（正常轮转）
    Timeout,        // 等待超时
    ProtocolError,  // 魔数/长度/body_len 不符：调用方应断链
    VersionError,   // 信封版本不兼容：调用方应答拒绝并断链
};

class CtlConnection {
public:
    explicit CtlConnection(int fd);
    ~CtlConnection();   // 关闭 fd

    CtlConnection(const CtlConnection&) = delete;
    CtlConnection& operator=(const CtlConnection&) = delete;

    // timeout_ms <= 0 表示无限等待（SIGTERM 关停由 accept 侧轮询承担；
    // 连接内用 1 s 轮询以便空闲断开，见 stream_session）。
    CtlRecv recv(CtlMessage& out, int timeout_ms);

    // 发送一条应答帧；失败（对端已关等）返回 false。
    bool send(uint16_t type, uint32_t request_id,
              const uint8_t* body, size_t len);

private:
    int fd_;
};

class CtlListener {
public:
    // 清理陈旧 socket 文件后绑定并 listen(4)；失败抛 runtime_error。
    explicit CtlListener(const std::filesystem::path& path);
    ~CtlListener();   // 关闭 fd（socket 文件留给下次绑定清理）

    CtlListener(const CtlListener&) = delete;
    CtlListener& operator=(const CtlListener&) = delete;

    // 100 ms 轮询 accept；stop 置位后返回 nullptr。
    std::unique_ptr<CtlConnection> accept(const std::atomic<bool>& stop);

private:
    int fd_;
};

} // namespace rvc

#endif // RVC_CTL_LINK_H
