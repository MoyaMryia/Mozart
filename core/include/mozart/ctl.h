// ctl.h — 控制面信封契约唯一定义源（C-ABI，Rust 侧用 #[repr(C)] 镜像）
// ============================================================================
// 各进程共用：daemon、pre、rvc、asr、tts、bridge。只定义跨进程信封；JSON 体
// 字段见各模块规格（SPEC-02/03/04/05）。修改本文件后必须同步 core/src/ctl.rs
// 与编译期断言。
#ifndef MOZART_CTL_H
#define MOZART_CTL_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifdef __cplusplus
  #define MOZART_CTL_STATIC_ASSERT(cond, msg) static_assert(cond, msg)
#else
  #define MOZART_CTL_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#endif

#define MOZART_CTL_MAGIC           0x4C54434Du   // 线上字节序 'M' 'C' 'T' 'L'
#define MOZART_CTL_VERSION         1
#define MOZART_CTL_HEADER_BYTES    16
#define MOZART_CTL_MAX_BODY_BYTES  65536

// ---- 16 字节定长头，全部小端 ---------------------------------------------------
#pragma pack(push, 1)
typedef struct {
    uint32_t magic;       // MOZART_CTL_MAGIC
    uint16_t version;     // MOZART_CTL_VERSION
    uint16_t type;        // 命令号；响应回显请求号
    uint32_t request_id;  // 请求方分配，响应原样带回
    uint32_t body_len;    // JSON 体字节数，0..MOZART_CTL_MAX_BODY_BYTES
} mozart_ctl_header_t;
#pragma pack(pop)
MOZART_CTL_STATIC_ASSERT(sizeof(mozart_ctl_header_t) == MOZART_CTL_HEADER_BYTES,
                         "mozart_ctl_header_t must be 16 bytes");

// ---- 命令号按平面分段 ---------------------------------------------------------
enum {
    MOZART_CTL_DAEMON_HELLO      = 0x0101,
    MOZART_CTL_DAEMON_GET_STATUS = 0x0102,
    MOZART_CTL_DAEMON_ROUTE_SET  = 0x0103,
    MOZART_CTL_DAEMON_MUTE_SET   = 0x0104,
    MOZART_CTL_DAEMON_SHUTDOWN   = 0x0105,
    MOZART_CTL_DAEMON_TRANSLATE_SET = 0x0106,  // 体 {"enabled": bool}，转发桥
    MOZART_CTL_DAEMON_SPEECH_SET    = 0x0107,  // 体 {"enabled": bool}，转发桥
    MOZART_CTL_DAEMON_SPEECH_STOP   = 0x0108,  // 体 {}（空），转发桥

    MOZART_CTL_PRE_HELLO      = 0x0201,
    MOZART_CTL_PRE_SET_ROUTE  = 0x0202,
    MOZART_CTL_PRE_SET_MUTE   = 0x0203,
    MOZART_CTL_PRE_GET_STATUS = 0x0204,

    MOZART_CTL_RVC_HELLO        = 0x0301,
    MOZART_CTL_RVC_LOAD_MODEL   = 0x0302,
    MOZART_CTL_RVC_START_STREAM = 0x0303,
    MOZART_CTL_RVC_STOP_STREAM  = 0x0304,
    MOZART_CTL_RVC_RESET_STREAM = 0x0305,
    MOZART_CTL_RVC_GET_STATUS   = 0x0306,

    MOZART_CTL_ASR_HELLO      = 0x0401,
    MOZART_CTL_ASR_GET_STATUS = 0x0402,

    MOZART_CTL_TTS_HELLO      = 0x0501,
    MOZART_CTL_TTS_GET_STATUS = 0x0502,
    // 任务面（tts-jobs.sock，自有协议复用本信封）：
    MOZART_CTL_TTS_JOB_SUBMIT = 0x0510,  // 桥→TTS，体 {"text","utterance_id","voice_params"?}
    MOZART_CTL_TTS_JOB_REPLY  = 0x0511,  // TTS→桥，体 {"status","utterance_id","reason"?}

    MOZART_CTL_BRIDGE_HELLO         = 0x0601,
    MOZART_CTL_BRIDGE_GET_STATUS    = 0x0602,
    MOZART_CTL_BRIDGE_TRANSLATE_SET = 0x0603,  // 体 {"enabled": bool}
    MOZART_CTL_BRIDGE_SPEECH_SET    = 0x0604,  // 体 {"enabled": bool}
    MOZART_CTL_BRIDGE_SPEECH_STOP   = 0x0605   // 体 {}（空）
};

// ---- 响应体 {"status","code","reason","data"} 的 code 值 -----------------------
#define MOZART_CTL_CODE_OK               "Ok"
#define MOZART_CTL_CODE_INVALID_ARGUMENT "InvalidArgument"
#define MOZART_CTL_CODE_NOT_LOADED       "NotLoaded"
#define MOZART_CTL_CODE_NOT_READY        "NotReady"
#define MOZART_CTL_CODE_BUSY             "Busy"
#define MOZART_CTL_CODE_UNSUPPORTED      "Unsupported"
#define MOZART_CTL_CODE_INFERENCE_FAILED "InferenceFailed"
#define MOZART_CTL_CODE_UNKNOWN_COMMAND  "UnknownCommand"
#define MOZART_CTL_CODE_INTERNAL         "Internal"

#ifdef __cplusplus
}
#endif

#undef MOZART_CTL_STATIC_ASSERT

#endif // MOZART_CTL_H
