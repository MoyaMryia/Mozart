#!/bin/bash
# Jetson 语言链路板端实测：daemon + 真 pre + 真 ASR/TTS/bridge/llama，CLI 驱动。
# 前置：本仓库已 rsync 到设备并完成构建（tools/deploy-jetson.sh）；构建须链接
#   sherpa（SHERPA_ONNX_LIB_DIR 指向资产根 lib），否则 asr/tts 是 stub。
# 拓扑（snd-aloop）：aplay 语音 → Loopback DEV=0,SUBDEV=$CAP_SUB → pre 采集
#   DEV=1,SUBDEV=$CAP_SUB；pre 播放 DEV=0,SUBDEV=$PB_SUB → arecord DEV=1,SUBDEV=$PB_SUB。
#   默认 4/5，避开 smoke 与文件链路的 2/3，允许并发不同测试。
# 场景：六进程 running → 播放中文语音 → ASR final → 翻议落字幕 → TTS 合成
#   → 回录非静音 → 关停无残留。
#   ASSETS_DIR  仓外资产根（默认 $HOME/mozart-assets）
#   BIN_DIR     部署二进制目录（默认 $HOME/Mozart/target/debug）
#   SMOKE_DIR   产物目录（默认 /tmp/mozart-language-test）
#   SAMPLE_M4A  真实语音样本（默认 $HOME/mozart-samples/jieshe.m4a）
#   SS_S / T_S  截取起点与时长秒（默认 60 / 30）
#   CAP_SUB / PB_SUB  Loopback 子设备（默认 4 / 5）
#   READY_TIMEOUT  等待六进程 ready 上限秒（默认 90）
#   RESULT_TIMEOUT 等待 ASR/翻译/TTS 产出的上限秒（默认 90）
set -u

ASSETS_DIR=${ASSETS_DIR:-${MOZART_ASSETS_DIR:-$HOME/mozart-assets}}
BIN_DIR=${BIN_DIR:-$HOME/Mozart/target/debug}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-language-test}
SAMPLE_M4A=${SAMPLE_M4A:-$HOME/mozart-samples/jieshe.m4a}
SS_S=${SS_S:-60}
T_S=${T_S:-30}
CAP_SUB=${CAP_SUB:-4}
PB_SUB=${PB_SUB:-5}
READY_TIMEOUT=${READY_TIMEOUT:-90}
RESULT_TIMEOUT=${RESULT_TIMEOUT:-90}

RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/language.toml
SUB=$SMOKE_DIR/subtitles.jsonl
EXCERPT=$SMOKE_DIR/speech.wav

# 语言链路要真实推理：清掉 stub 注入口；共享库与 llama-server 走资产根。
unset MOZART_ASR_STUB MOZART_ASR_STUB_TEXT MOZART_TTS_STUB
export LD_LIBRARY_PATH="$ASSETS_DIR/lib:$ASSETS_DIR/llama${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export MOZART_LLAMA_BIN="$ASSETS_DIR/llama/llama-server"

mkdir -p "$SMOKE_DIR"
rm -rf "$RUN" "$SUB" "$EXCERPT" "$SMOKE_DIR"/rec.raw "$SMOKE_DIR"/st-*.json

ts() { printf "[%5ss]" "$SECONDS"; }
ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
# status JSON 取值函数与 smoke/measure/file-test 共用（jget.sh 有键位说明）。
. "$(cd "$(dirname "$0")" && pwd)/jget.sh"

FAILS=0
ok() { echo "  ok   $*"; }
bad() { echo "  FAIL $*" >&2; FAILS=$((FAILS + 1)); }
assert_eq() { if [ "$2" = "$3" ]; then ok "$1=$3"; else bad "$1=$2（期望 $3）"; fi; }

cleanup() {
  ctl down >/dev/null 2>&1 || true
  [ -n "${AREC_PID:-}" ] && kill "$AREC_PID" 2>/dev/null
  return 0
}
trap cleanup EXIT

fail_fast() { echo "LANGUAGE TEST FAILED: $*" >&2; exit 1; }

# ---- 前置检查 ----
for bin in mozart-ctl mozart-daemon mozart-pre mozart-rvc mozart-asr mozart-tts mozart-bridge; do
  [ -x "$BIN_DIR/$bin" ] || fail_fast "缺二进制 $BIN_DIR/$bin（先运行 tools/deploy-jetson.sh）"
done
[ -x "$BIN_DIR/examples/rvc_stub" ] || fail_fast "缺 $BIN_DIR/examples/rvc_stub（部署会构建 examples）"
[ -e "$ASSETS_DIR/lib/libsherpa-onnx-c-api.so" ] || fail_fast "缺 $ASSETS_DIR/lib/libsherpa-onnx-c-api.so"
[ -x "$ASSETS_DIR/llama/llama-server" ] || fail_fast "缺 $ASSETS_DIR/llama/llama-server"
[ -d "$ASSETS_DIR/models/asr/zipformer-zh-14M" ] || fail_fast "缺 ASR 模型"
[ -d "$ASSETS_DIR/models/asr-review/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17" ] || fail_fast "缺复核模型"
[ -d "$ASSETS_DIR/models/tts/sherpa-onnx-pocket-tts-int8-2026-01-26" ] || fail_fast "缺 TTS 模型"
[ -e "$ASSETS_DIR/models/llm/qwen35-0.8b-text-Q4_K_M.gguf" ] || fail_fast "缺翻译模型"
[ -e "$SAMPLE_M4A" ] || fail_fast "缺语音样本 $SAMPLE_M4A"
SHERPA_LINK=$(ldd "$BIN_DIR/mozart-asr" 2>/dev/null | grep sherpa-onnx || true)
case "$SHERPA_LINK" in
  *"not found"*) fail_fast "mozart-asr 的 sherpa 库不可加载：LD_LIBRARY_PATH 缺 $ASSETS_DIR/lib" ;;
  "") fail_fast "mozart-asr 未链接 sherpa（重跑部署或 SHERPA_ONNX_LIB_DIR=$ASSETS_DIR/lib cargo build --workspace --bins --examples）" ;;
esac

# ---- 语音样本：真实录音截取 ----
ffmpeg -y -v error -ss "$SS_S" -t "$T_S" -i "$SAMPLE_M4A" -ar 48000 -ac 2 "$EXCERPT" \
  || fail_fast "ffmpeg 截取失败"

# ---- 配置 ----
cat > "$CFG" <<EOF
[daemon]
run_dir = "$RUN"
rpc_timeout_ms = 2000
start_timeout_ms = 30000
poll_interval_ms = 1000

[process]
pre_bin = "mozart-pre"
rvc_bin = "$BIN_DIR/examples/rvc_stub"
asr_bin = "mozart-asr"
tts_bin = "mozart-tts"
bridge_bin = "mozart-bridge"
llama_bin = "$ASSETS_DIR/llama/llama-server"

[pre]
socket = "pre.sock"
capture_device = "plughw:CARD=Loopback,DEV=1,SUBDEV=$CAP_SUB"
playback_device = "plughw:CARD=Loopback,DEV=0,SUBDEV=$PB_SUB"
capture_queue_frames = 8
[pre.dsp]
denoise = true

[rvc]
socket = "rvc.sock"
input_socket = "rvc-in.sock"
output_socket = "rvc-out.sock"
model_id = "demo"

[asr]
model_dir = "$ASSETS_DIR/models/asr/zipformer-zh-14M"
review_model_dir = "$ASSETS_DIR/models/asr-review/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17"

[tts]
model_dir = "$ASSETS_DIR/models/tts/sherpa-onnx-pocket-tts-int8-2026-01-26"
reference_wav = "$ASSETS_DIR/models/tts/sherpa-onnx-pocket-tts-int8-2026-01-26/test_wavs/bria.wav"
provider = "cpu"

[bridge]
subtitles_path = "$SUB"
llama_model = "$ASSETS_DIR/models/llm/qwen35-0.8b-text-Q4_K_M.gguf"
translate_enabled = true
speech_enabled = true
EOF

# ---- 启动与就绪 ----
echo "=== up ==="
ctl up || fail_fast "up 失败"
READY=0
for i in $(seq 1 "$READY_TIMEOUT"); do
  ctl status --json > "$SMOKE_DIR/st-poll.json" 2>/dev/null || true
  STATES=""
  for role in pre rvc asr tts bridge llama; do
    s=$(jget "$SMOKE_DIR/st-poll.json" path "processes.$role.state" 2>/dev/null)
    STATES="$STATES $role=$s"
    [ "$s" = "running" ] || { READY=0; break; }
    READY=1
  done
  [ "$READY" = 1 ] && break
  sleep 1
done
cp "$SMOKE_DIR/st-poll.json" "$SMOKE_DIR/st-ready.json" 2>/dev/null
if [ "$READY" != 1 ]; then
  tail -20 "$RUN/logs/llama.log" >&2 2>/dev/null
  fail_fast "六进程 ${i}s 内未全部 running：$STATES"
fi
echo "ready after ${i}s:$STATES"

# ---- 喂语音并回录播放端 ----
echo "=== play ${T_S}s speech ==="
arecord -q -D "plughw:CARD=Loopback,DEV=1,SUBDEV=$PB_SUB" -f S16_LE -c 2 -r 48000 -t raw \
  "$SMOKE_DIR/rec.raw" &
AREC_PID=$!
aplay -q -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "$EXCERPT" \
  || fail_fast "aplay 失败"
echo "playback done, waiting results"

# ---- 等 ASR final / 翻译 / TTS 合成 ----
FINALS=0; TRANSLATED=0; SYNTH=0
for i in $(seq 1 "$RESULT_TIMEOUT"); do
  ctl status --json > "$SMOKE_DIR/st-wait.json" 2>/dev/null || true
  FINALS=$(jget "$SMOKE_DIR/st-wait.json" path asr.finals 2>/dev/null || echo 0)
  TRANSLATED=$(jget "$SMOKE_DIR/st-wait.json" path bridge.translated 2>/dev/null || echo 0)
  SYNTH=$(jget "$SMOKE_DIR/st-wait.json" path tts.synthesized 2>/dev/null || echo 0)
  FINALS=${FINALS:-0}; TRANSLATED=${TRANSLATED:-0}; SYNTH=${SYNTH:-0}
  if [ "$FINALS" -ge 1 ] && [ "$TRANSLATED" -ge 1 ] && [ "$SYNTH" -ge 1 ]; then
    break
  fi
  sleep 1
done
kill "$AREC_PID" 2>/dev/null; wait "$AREC_PID" 2>/dev/null; AREC_PID=""
ctl status --json > "$SMOKE_DIR/st-final.json"

# ---- 断言 ----
FAILED_TTS=$(jget "$SMOKE_DIR/st-final.json" path tts.failed 2>/dev/null || echo 0)
DELIVERED=$(jget "$SMOKE_DIR/st-final.json" path bridge.speech_delivered 2>/dev/null || echo 0)
LLAMA_ON=$(jget "$SMOKE_DIR/st-final.json" path bridge.llama_connected 2>/dev/null || echo false)
FINALS=$(jget "$SMOKE_DIR/st-final.json" path asr.finals 2>/dev/null || echo 0)
TRANSLATED=$(jget "$SMOKE_DIR/st-final.json" path bridge.translated 2>/dev/null || echo 0)
SYNTH=$(jget "$SMOKE_DIR/st-final.json" path tts.synthesized 2>/dev/null || echo 0)

echo "=== assertions ==="
if [ "${FINALS:-0}" -ge 1 ]; then ok "asr finals=$FINALS"; else bad "asr finals=$FINALS（期望 ≥1）"; fi
if [ "${TRANSLATED:-0}" -ge 1 ]; then ok "bridge translated=$TRANSLATED"; else bad "bridge translated=$TRANSLATED（期望 ≥1）"; fi
if [ "${SYNTH:-0}" -ge 1 ]; then ok "tts synthesized=$SYNTH"; else bad "tts synthesized=$SYNTH（期望 ≥1）"; fi
assert_eq "bridge llama_connected" "$LLAMA_ON" "true"
assert_eq "tts failed" "${FAILED_TTS:-x}" "0"
if [ "${DELIVERED:-0}" -ge 1 ]; then ok "bridge speech_delivered=$DELIVERED"; else bad "bridge speech_delivered=$DELIVERED（期望 ≥1）"; fi

SUB_EN=$(python3 - "$SUB" <<'EOF'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
print(sum(1 for r in rows if r.get("final") and r.get("en")))
EOF
)
if [ "${SUB_EN:-0}" -ge 1 ]; then ok "subtitles 双语 final=$SUB_EN"; else bad "subtitles 双语 final=$SUB_EN（期望 ≥1）"; fi

RMS=$(python3 - "$SMOKE_DIR/rec.raw" <<'EOF'
import sys
import numpy as np
a = np.fromfile(sys.argv[1], dtype="<i2").astype(np.float32) / 32768
print(f"{np.sqrt((a ** 2).mean()):.4f}" if a.size else "0")
EOF
)
python3 - "$RMS" <<'EOF' && ok "playback RMS=$RMS" || bad "playback RMS=$RMS（期望 >0.01）"
import sys
sys.exit(0 if float(sys.argv[1]) > 0.01 else 1)
EOF

# ---- 关停与残留 ----
echo "=== down ==="
ctl down
FAILED_ORIG=$FAILS
if pgrep -f "mozart-daemon --config $CFG" >/dev/null; then
  bad "daemon 残留"
else
  ok "无 mozart 进程残留"
fi
for f in daemon.sock pre.sock rvc.sock asr.sock tts.sock bridge.sock llama.sock; do
  if [ -S "$RUN/$f" ]; then
    LIVE=$(python3 - "$RUN/$f" <<'EOF'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
try:
    s.connect(sys.argv[1]); print("live")
except OSError:
    print("dead")
EOF
)
    [ "$LIVE" = "dead" ] || bad "socket 仍可连接：$f"
  fi
done
[ "$FAILS" = "$FAILED_ORIG" ] && ok "控制 socket 全部失效"

echo
echo "=== LANGUAGE TEST SUMMARY ==="
echo "ASR finals=$FINALS  translated=$TRANSLATED  speech_delivered=$DELIVERED  TTS synthesized=$SYNTH failed=$FAILED_TTS  subtitle_finals=$SUB_EN  RMS=$RMS"
echo "证据目录：$SMOKE_DIR"
if [ "$FAILS" -eq 0 ]; then
  echo "LANGUAGE TEST PASSED"
  exit 0
fi
echo "LANGUAGE TEST FAILED: $FAILS 项断言失败" >&2
exit 1
