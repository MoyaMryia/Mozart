// frame.h — 帧契约唯一定义源（C-ABI，Rust 侧用 #[repr(C)] 镜像）
// ============================================================================
// 采样率约定：
//   RAW    = 48 kHz / 960 samples / 20 ms — 物理采集原始帧
//   INPUT  = 16 kHz / 320 samples / 20 ms — 预处理输出 / 推理输入契约帧
//   OUTPUT = 48 kHz / 960 samples / 20 ms — 推理输出帧
//
// 所有结构体严格 1 字节对齐，跨语言（C / C++ / Rust）ABI 稳定。
// 修改本文件后必须同步 core/src/lib.rs 的镜像与编译期断言。
#ifndef MOZART_FRAME_H
#define MOZART_FRAME_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifdef __cplusplus
  #define MOZART_STATIC_ASSERT(cond, msg) static_assert(cond, msg)
#else
  #define MOZART_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#endif

// ---- 采样率与帧长常量 --------------------------------------------------------
#define MOZART_RAW_SAMPLE_RATE     48000
#define MOZART_RAW_FRAME_MS        20
#define MOZART_RAW_SAMPLES         960

#define MOZART_INPUT_SAMPLE_RATE   16000
#define MOZART_INPUT_FRAME_MS      20
#define MOZART_INPUT_SAMPLES       320

#define MOZART_OUTPUT_SAMPLE_RATE  48000
#define MOZART_OUTPUT_FRAME_MS     20
#define MOZART_OUTPUT_SAMPLES      960

#define MOZART_CHANNELS            1

// ---- 16 字节帧元数据 ----------------------------------------------------------
#pragma pack(push, 1)
typedef struct {
    uint64_t pts_ns;       // 单调时钟时间戳（纳秒），采集时填充
    uint32_t frame_idx;    // 单调递增帧序号
    uint8_t  vad_flag;     // 语音激活: 0 = 静音, 1 = 有声
    uint8_t  energy_db;    // 当前帧能量 (dB, 0-255)
    uint8_t  conf;         // 降噪置信度 (0-255)
    uint8_t  segment_id;   // 声音分段 ID (0 = 静音间隔)
} mozart_frame_meta_t;
#pragma pack(pop)
MOZART_STATIC_ASSERT(sizeof(mozart_frame_meta_t) == 16,
                     "mozart_frame_meta_t must be exactly 16 bytes");

// ---- 48 kHz 原始采集帧（进程内使用，不跨进程边界）------------------------------
#pragma pack(push, 1)
typedef struct {
    mozart_frame_meta_t meta;
    float               pcm[MOZART_RAW_SAMPLES];
} mozart_raw_frame_t;
#pragma pack(pop)
MOZART_STATIC_ASSERT(sizeof(mozart_raw_frame_t) == 16 + 960 * 4,
                     "mozart_raw_frame_t size mismatch");

// ---- 16 kHz 输入契约帧（音频进程 → 推理进程）----------------------------------
#pragma pack(push, 1)
typedef struct {
    mozart_frame_meta_t meta;
    float               pcm[MOZART_INPUT_SAMPLES];
} mozart_input_frame_t;
#pragma pack(pop)
MOZART_STATIC_ASSERT(sizeof(mozart_input_frame_t) == 16 + 320 * 4,
                     "mozart_input_frame_t size mismatch");

// ---- 48 kHz 输出契约帧（推理进程 → 音频进程）----------------------------------
#pragma pack(push, 1)
typedef struct {
    mozart_frame_meta_t meta;
    float               pcm[MOZART_OUTPUT_SAMPLES];
} mozart_output_frame_t;
#pragma pack(pop)
MOZART_STATIC_ASSERT(sizeof(mozart_output_frame_t) == 16 + 960 * 4,
                     "mozart_output_frame_t size mismatch");

// ---- 线包常量 ----------------------------------------------------------------
#define MOZART_INPUT_PACKET_MAGIC   0x4D5A5254u   // 'MZRT'
#define MOZART_OUTPUT_PACKET_MAGIC  0x54525A4Du   // 'TRZM'
#define MOZART_PACKET_HEADER_SIZE   20            // 4B magic + 16B meta
// 输入线包: 20 + 320*4 = 1300 字节
// 输出线包: 20 + 960*4 = 3860 字节

#ifdef __cplusplus
}
#endif

#undef MOZART_STATIC_ASSERT

#endif // MOZART_FRAME_H
