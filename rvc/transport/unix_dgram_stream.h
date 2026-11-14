// unix_dgram_stream.h — 数据面 Unix DGRAM 流（rvc 侧）
// ============================================================================
// 替换旧 UdpStream（Mozart-Old/IO/include/mozart/udp_stream.hpp）：线包布局与
// core/src/dgram.rs 逐字节一致（魔数小端 u32 + 16 B meta + 小端 f32 PCM），
// 方向约定照 SPEC-02/03：rvc 绑定 input_socket 收 1300 B 输入线包，
// connect 到 pre 绑定的 output_socket 发 3860 B 输出线包。
//
// 语义（对应 core/src/dgram.rs::FrameSocket）：
//   - 非阻塞发送（实时，默认）：过载与对端不在（ENOENT/ECONNREFUSED/ENOTCONN）
//     丢弃计数，对端不在时尽力重连，只恢复后续帧通道；其余错误返回 false
//     交调用方记录。
//   - 阻塞发送（文件批处理，blocking_send=true）：缓冲满等可写重试、对端不在
//     重连后重试，不丢弃；仅流关停或不可恢复错误返回 false。实现用 O_NONBLOCK
//     + poll(POLLOUT) 用户态等待：内核阻塞 send 无法被 Close() 唤醒，会挂死
//     StopStream。
//   - 接收：非阻塞排空 + 100 ms poll 等待，长度或魔数不符计数丢弃后继续收；
//     Close() 置关停标志并关闭 fd，解除阻塞中的 ReadFrame。
//   - 热路径只使用栈缓冲，不分配、不加长锁。
#ifndef RVC_UNIX_DGRAM_STREAM_H
#define RVC_UNIX_DGRAM_STREAM_H

#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <filesystem>

#include <sys/socket.h>
#include <sys/un.h>

#include "mozart/frame.h"
#include "transport/audio_stream.h"

namespace rvc {

// 线包总长：4 B 魔数 + 帧结构体（16 B meta + PCM）。与 Rust 侧 PACKET_BYTES 对齐。
inline constexpr size_t kInputPacketBytes = 4 + sizeof(mozart_input_frame_t);
inline constexpr size_t kOutputPacketBytes = 4 + sizeof(mozart_output_frame_t);
static_assert(kInputPacketBytes == 1300, "输入线包必须 1300 B（frame.h 契约）");
static_assert(kOutputPacketBytes == 3860, "输出线包必须 3860 B（frame.h 契约）");

// ---- 线包编解码（魔数与长度校验在 decode 内完成，单一校验点）----

// 编码输入线包到 out；返回写入字节数，cap 不足返回 0。
size_t encode_input_packet(const mozart_input_frame_t& frame,
                           uint8_t* out, size_t cap);

// 解码输入线包；长度或魔数不符返回 false（调用方计数丢弃）。
bool decode_input_packet(const uint8_t* bytes, size_t len,
                         mozart_input_frame_t& out);

size_t encode_output_packet(const mozart_output_frame_t& frame,
                            uint8_t* out, size_t cap);

bool decode_output_packet(const uint8_t* bytes, size_t len,
                          mozart_output_frame_t& out);

// 数据面丢弃计数快照，StreamSession 合并进 frames_dropped。
struct DgramStreamStats {
    uint64_t send_dropped = 0;
    uint64_t recv_dropped = 0;
};

class UnixDgramStream final : public RealTimeAudioStream {
public:
    // input_path：绑定并清理陈旧文件；output_path：connect（pre 侧已绑定）。
    // queue_frames：内核缓冲帧数，SO_RCVBUF/SNDBUF 换算基数，与 pre 侧
    // RVC_QUEUE_FRAMES（8）同量级；连接失败（对端未就绪）抛 runtime_error。
    // blocking_send：文件批处理输出模式（见文件头语义），实时保持默认 false。
    UnixDgramStream(const std::filesystem::path& input_path,
                    const std::filesystem::path& output_path,
                    size_t queue_frames, bool blocking_send = false);
    ~UnixDgramStream() override;

    UnixDgramStream(const UnixDgramStream&) = delete;
    UnixDgramStream& operator=(const UnixDgramStream&) = delete;

    bool IsOpen() const noexcept override;
    bool ReadFrame(void* out_frame_buf, size_t buf_size) override;
    bool WriteFrame(const void* in_frame_buf, size_t buf_size) override;
    void Close() override;

    DgramStreamStats stats() const noexcept;

private:
    const bool blocking_send_;
    std::atomic<int> input_fd_{-1};
    std::atomic<int> output_fd_{-1};
    std::atomic<uint64_t> send_dropped_{0};
    std::atomic<uint64_t> recv_dropped_{0};
    sockaddr_un peer_addr_{};
    socklen_t peer_len_ = 0;

    bool try_reconnect();
    bool send_blocking(const uint8_t* packet);
};

} // namespace rvc

#endif // RVC_UNIX_DGRAM_STREAM_H
