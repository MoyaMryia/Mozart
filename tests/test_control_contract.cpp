// 控制信封契约：C 头的尺寸、字节序与命令号分段必须与 Rust 镜像一致。
#include <cstdio>
#include <cstring>

#include "mozart/ctl.h"

static_assert(sizeof(mozart_ctl_header_t) == MOZART_CTL_HEADER_BYTES);
static_assert(MOZART_CTL_HEADER_BYTES == 16);
static_assert(MOZART_CTL_VERSION == 1);
static_assert(MOZART_CTL_MAX_BODY_BYTES == 65536);
static_assert(MOZART_CTL_DAEMON_HELLO == 0x0101);
static_assert(MOZART_CTL_PRE_GET_STATUS == 0x0204);
static_assert(MOZART_CTL_RVC_GET_STATUS == 0x0306);
static_assert(MOZART_CTL_ASR_HELLO == 0x0401);
static_assert(MOZART_CTL_ASR_GET_STATUS == 0x0402);
static_assert(MOZART_CTL_TTS_HELLO == 0x0501);
static_assert(MOZART_CTL_TTS_GET_STATUS == 0x0502);
static_assert(MOZART_CTL_BRIDGE_HELLO == 0x0601);
static_assert(MOZART_CTL_BRIDGE_GET_STATUS == 0x0602);
static_assert(MOZART_CTL_BRIDGE_TRANSLATE_SET == 0x0603);
static_assert(MOZART_CTL_BRIDGE_SPEECH_SET == 0x0604);
static_assert(MOZART_CTL_BRIDGE_SPEECH_STOP == 0x0605);

int main() {
    const mozart_ctl_header_t header{
        MOZART_CTL_MAGIC, MOZART_CTL_VERSION, MOZART_CTL_DAEMON_HELLO, 7, 3};

    const auto* bytes = reinterpret_cast<const unsigned char*>(&header);
    if (std::memcmp(bytes, "MCTL", 4) != 0) {
        std::fprintf(stderr, "magic byte order mismatch\n");
        return 1;
    }
    if (header.version != 1 || header.type != MOZART_CTL_DAEMON_HELLO ||
        header.request_id != 7 || header.body_len != 3) {
        std::fprintf(stderr, "header field layout mismatch\n");
        return 2;
    }
    std::printf("mozart-control-contract\n");
    return 0;
}
