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
#define MOZART_CTL_IDLE_TIMEOUT_MS 30000

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
    MOZART_CTL_DAEMON_SPEECH_CLEAR  = 0x0109,  // 体 {}（空），清播报队列并中止当前合成，保持播报开关不变
    // 文件链路与管理命令（SPEC-04 CLI 命令面）；体字段见 core/src/ctl/schema.rs：
    MOZART_CTL_DAEMON_FILE_SUBMIT = 0x0110,  // 体 {"path","model_id"?}，data {"job_id","state","queue_position"}
    MOZART_CTL_DAEMON_FILE_PAUSE  = 0x0111,  // 体 {}（空），data {"paused"}
    MOZART_CTL_DAEMON_FILE_RESUME = 0x0112,  // 体 {}（空），data {"paused"}
    MOZART_CTL_DAEMON_FILE_CANCEL = 0x0113,  // 体 {"job_id"}，data {"job_id","state"}
    // 体 {"job_id","output"?}，data {"job_id","output_path","name"}；v1 同机 daemon 不复制，
    // 回受管结果绝对路径与缺省名（源文件名主干 + .wav），CLI 复制到 output；不可取拒绝 NotReady
    MOZART_CTL_DAEMON_FILE_FETCH  = 0x0114,
    MOZART_CTL_DAEMON_FILE_STATUS = 0x0115,  // 体 {"job_id"}，data FileJob
    MOZART_CTL_DAEMON_FILE_LIST   = 0x0116,  // 体 {}（空），data {"jobs":[FileJob],"paused"}
    MOZART_CTL_DAEMON_MODEL_LIST  = 0x0117,  // 体 {}（空），data {"models":[{"id","exists","loaded"}]}
    MOZART_CTL_DAEMON_RESTART     = 0x0118,  // 体 {"service"}
    MOZART_CTL_DAEMON_DEVICE_LIST = 0x0119,  // 体 {"all"?}，data {"devices":[{"id","label","kind"}]}
    MOZART_CTL_DAEMON_CONFIG_SET  = 0x011A,  // 体 {"key","value"}，data {"key","value","restart_required","affected"}
    // 体 {"key"?}，data {"value","pending_restart_keys"}（缺省 key 回整份基线；
    // pending_restart_keys 为尚未重启生效的键名，无则空数组）
    MOZART_CTL_DAEMON_CONFIG_GET  = 0x011B,
    MOZART_CTL_DAEMON_CONFIG_SAVE = 0x011C,  // 体 {}（空），data {"saved"}；状态覆盖批量写回基线后清空
    // 模型预设（SPEC-11）：体 {}（空，v1 固定 default、当前已加载模型）；
    // data 见各模块规格。无模型加载回 NotReady。
    MOZART_CTL_DAEMON_PRESET_SAVE = 0x011D,  // data {"model_id","preset","values"}
    MOZART_CTL_DAEMON_PRESET_USE  = 0x011E,  // data {"model_id","preset","values"}；编排失败回 error
    MOZART_CTL_DAEMON_PRESET_LIST = 0x011F,  // data {"model_id","preset","preset_values","live_values","dirty"}
    MOZART_CTL_DAEMON_MODEL_USE   = 0x0120,  // 体 {"model_id"}，data {"model_id","previous_model_id"}；实时/空闲切换模型
    MOZART_CTL_DAEMON_DEVICE_USE  = 0x0121,  // 体 {"kind","device"}，data {"kind","device","previous_device"}；临时热切换音频设备（不写回 TOML）
    MOZART_CTL_DAEMON_OUTPUT_MODE_SET = 0x0122, // {"mode":"direct"|"rvc"|"broadcast"}
    MOZART_CTL_DAEMON_SERVICES_START  = 0x0123, // 体 {}（空），恢复启动业务子服务组，data {"state":"running"|"already-running"}
    MOZART_CTL_DAEMON_SERVICES_STOP   = 0x0124, // 体 {}（空），停止业务子服务组（daemon 与 web 在线），data {"state":"stopped"|"not-running"}

    MOZART_CTL_PRE_HELLO      = 0x0201,
    MOZART_CTL_PRE_SET_ROUTE  = 0x0202,
    MOZART_CTL_PRE_SET_MUTE   = 0x0203,
    MOZART_CTL_PRE_GET_STATUS = 0x0204,
    MOZART_CTL_PRE_SPEECH_STOP = 0x0205,  // {"cutoff_ns":u64}；应答等待播放边界。空对象仅清旧式队列
    // 体 {"active": bool}；文件任务接管实时数据面（SPEC-03 复用 + 状态门控交接）：
    // active=true 暂停 rvc AI 发送并释放 output_socket（应答前确认释放完成），
    // active=false 恢复发送并重绑 output_socket。asr 支路不受影响。
    MOZART_CTL_PRE_SET_FILE_MODE = 0x0206,
    // 体 {"denoise": bool, "gate": bool, "agc": bool, "limiter": bool}（四键必需）；
    // 运行中切换 DSP 级（SPEC-02）：denoise 重新启用时重置 WOLA 流式状态，
    // gate/agc/limiter 直接切换。应答 data 回显四个开关。
    MOZART_CTL_PRE_SET_DSP = 0x0207,
    // 体 {"kind":"capture"|"playback","device":string}；运行中热替换采集/播放 ALSA 句柄（SPEC-02）：
    // 先试打开新设备再关旧句柄，失败保留旧设备；切 capture 清半帧并重置 WOLA，切 playback 首帧 10 ms 淡入。
    MOZART_CTL_PRE_SET_DEVICE = 0x0208,
    MOZART_CTL_PRE_SET_PLAYOUT = 0x0209,  // body: {policy, route, mute}; ACK at playback boundary

    MOZART_CTL_RVC_HELLO        = 0x0301,
    MOZART_CTL_RVC_LOAD_MODEL   = 0x0302,
    MOZART_CTL_RVC_START_STREAM = 0x0303,
    MOZART_CTL_RVC_STOP_STREAM  = 0x0304,
    MOZART_CTL_RVC_RESET_STREAM = 0x0305,
    MOZART_CTL_RVC_GET_STATUS   = 0x0306,
    MOZART_CTL_RVC_LIST_MODELS  = 0x0307,  // 体 {}（空），data {"models":[{"id","exists","loaded"}]}
    // 体 {"pitch_shift","index_rate","rms_mix_rate","protect","f0_method","skip_silence"?}；
    // 实时流活动时在下一推理块边界无中断生效（SPEC-03）。
    MOZART_CTL_RVC_SET_PARAMS   = 0x0308,

    MOZART_CTL_ASR_HELLO      = 0x0401,
    MOZART_CTL_ASR_GET_STATUS = 0x0402,

    MOZART_CTL_BRIDGE_HELLO         = 0x0501,
    MOZART_CTL_BRIDGE_GET_STATUS    = 0x0502,
    MOZART_CTL_BRIDGE_TRANSLATE_SET = 0x0503,  // 体 {"enabled": bool}
    MOZART_CTL_BRIDGE_SPEECH_SET    = 0x0504,  // 体 {"enabled": bool}
    MOZART_CTL_BRIDGE_SPEECH_STOP   = 0x0505,  // 体 {}（空）
    MOZART_CTL_BRIDGE_SPEECH_CLEAR  = 0x0506,  // 体 {}（空）或 {"fenced":true}；清队列、保持开关不变

    MOZART_CTL_TTS_HELLO      = 0x0601,
    MOZART_CTL_TTS_GET_STATUS = 0x0602,
    // 任务面（tts-jobs.sock，标准信封：响应 type 回显请求号，体 {"status","code","reason","data"}）：
    MOZART_CTL_TTS_JOB_SUBMIT = 0x0610,  // 桥→TTS，体 {"text","utterance_id","voice_params"?}；受理应答 data {"utterance_id"}
    MOZART_CTL_TTS_JOB_REPLY  = 0x0611,  // 受理应答：submitted→ok/Ok、队满→error/Busy、无效→error；type 回显请求号（0x0610/0x0612）
    MOZART_CTL_ENHANCE_HELLO = 0x0701,
    MOZART_CTL_ENHANCE_GET_STATUS = 0x0702,
    MOZART_CTL_TTS_JOB_STOP   = 0x0612   // 任务面或控制面；{"cutoff_ns":u64}，应答回显已安装的截止
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
