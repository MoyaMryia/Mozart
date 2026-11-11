// audio_stream.h — 实时帧流抽象
// ============================================================================
// 搬运自 Mozart-Old/IO/include/mozart/audio_stream.hpp 的 RealTimeAudioStream
// 接口语义，裁掉 C-ABI 工厂与 PipeWire 方向枚举：rvc 进程的数据面天然是
// 「读输入帧 + 写输出帧」双向流，旧抽象中的 Open/GetDirection 由具体实现
// 在构造时一次完成，运行期只剩帧读写与关停。
//
// 语义契约（与旧实现一致，AudioWorker 泵线程依赖）：
//   ReadFrame  — 阻塞读一帧，直到帧到达或流被关停；关停后返回 false。
//                buf_size 必须等于 sizeof(mozart_input_frame_t)。
//   WriteFrame — 非阻塞写一帧：过载与对端不在按「丢弃计数」处理并返回
//                true；仅不可恢复错误返回 false。
//   Close      — 解除 ReadFrame 阻塞并释放资源；幂等。
#ifndef RVC_AUDIO_STREAM_H
#define RVC_AUDIO_STREAM_H

#include <cstdint>
#include <cstddef>

namespace rvc {

class RealTimeAudioStream {
public:
    virtual ~RealTimeAudioStream() = default;

    virtual bool IsOpen() const noexcept = 0;

    // 阻塞读一帧输入；返回 false 表示流已关停，调用方应退出泵循环。
    virtual bool ReadFrame(void* out_frame_buf, size_t buf_size) = 0;

    // 非阻塞写一帧输出；false 仅在不可恢复错误时出现。
    virtual bool WriteFrame(const void* in_frame_buf, size_t buf_size) = 0;

    // 关停流并释放资源；同时解除阻塞中的 ReadFrame。幂等。
    virtual void Close() = 0;
};

} // namespace rvc

#endif // RVC_AUDIO_STREAM_H
