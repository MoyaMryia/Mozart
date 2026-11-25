#!/bin/bash
# Jetson 语言链路板端实测：daemon + 真 pre + 真 ASR/TTS/bridge/llama，CLI 驱动。
# 正常由 tools/jetson-e2e.sh 调用，也可在设备上直接运行。
# 前置：本仓库已 rsync 到设备并完成构建（tools/deploy-jetson.sh）；构建须链接
#   sherpa（SHERPA_ONNX_LIB_DIR 指向资产根 lib），否则 asr/tts 是 stub。
# 拓扑（snd-aloop）：aplay 语音 → Loopback DEV=0,SUBDEV=$CAP_SUB → pre 采集
#   DEV=1,SUBDEV=$CAP_SUB；pre 播放 DEV=0,SUBDEV=$PB_SUB → arecord DEV=1,SUBDEV=$PB_SUB。
#   默认 4/5，避开 smoke 与文件链路的 2/3，允许并发不同测试。
# 场景：六进程 running → broadcast 独占播报稳定 → 播放语音 + 尾静音 → 全链路队列排空
#   → 自动化核验（verify-language-run.py）→ 关停无残留。
#   ASSETS_DIR        仓外资产根（默认 $HOME/mozart-assets）
#   BIN_DIR           部署二进制目录（默认 $REPO_DIR/target/debug）
#   REPO_DIR          仓库目录（默认 $HOME/Mozart）
#   SMOKE_DIR         产物目录（默认 /tmp/mozart-language-test）
#   SAMPLE_M4A        真实语音样本（默认 $HOME/mozart-samples/jieshe.m4a）
#   SS_S / T_S        截取起点与时长秒（默认 60 / 30）
#   TAIL_SILENCE_S    播完语音后补喂的尾静音秒数（默认 2，须 >= 2）
#   CAP_SUB / PB_SUB  Loopback 子设备（默认 4 / 5）
#   READY_TIMEOUT     等待六进程与 broadcast 模式稳定上限秒（默认 90）
#   DRAIN_TIMEOUT     等待全链路队列排空上限秒（默认 60；兼容旧 RESULT_TIMEOUT）
#   LLAMA_MODEL       翻译模型 GGUF（默认优先 Hy-MT3-1.8B，缺失回退 Qwen3.5-0.8B）
#   LLAMA_MODEL_ID    模型注册 ID（默认取模型文件名去 .gguf）
#   LLAMA_PROFILE     提示词模板：hy_mt | qwen_chat（默认按文件名自动判别）
#   LLAMA_CTX_SIZE    可选覆盖 ctx_size（缺省：hy_mt 沿用 config.rs 默认 1024，qwen_chat 写 2048）
#   LLAMA_CACHE_RAM_MB 可选覆盖 cache_ram_mb（缺省：hy_mt 沿用 config.rs 默认 64，qwen_chat 写 128）
#   LLAMA_MAX_TOKENS  可选覆盖 max_tokens（缺省沿用 config.rs 默认 256）
#   LLAMA_CTX_WINDOW  可选覆盖 context_window（缺省：hy_mt 写 2，qwen_chat 沿用 config.rs 默认 0）
#   GLOSSARY_PATH     可选术语表路径（为空则不写入）
#   LLAMA_LEGACY_CONFIG 设为 1 退回旧单键 bridge.llama_model 写法（默认 0）
#   VERIFY_MODE       自动化核验级别：smoke | strict（默认 smoke）
set -u

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ASSETS_DIR=${ASSETS_DIR:-${MOZART_ASSETS_DIR:-$HOME/mozart-assets}}
REPO_DIR=${REPO_DIR:-$HOME/Mozart}
BIN_DIR=${BIN_DIR:-$REPO_DIR/target/debug}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-language-test}
SAMPLE_M4A=${SAMPLE_M4A:-$HOME/mozart-samples/jieshe.m4a}
SS_S=${SS_S:-60}
T_S=${T_S:-30}
TAIL_SILENCE_S=${TAIL_SILENCE_S:-2}
CAP_SUB=${CAP_SUB:-4}
PB_SUB=${PB_SUB:-5}
READY_TIMEOUT=${READY_TIMEOUT:-90}
DRAIN_TIMEOUT=${DRAIN_TIMEOUT:-${RESULT_TIMEOUT:-60}}
LLAMA_LEGACY_CONFIG=${LLAMA_LEGACY_CONFIG:-0}
GLOSSARY_PATH=${GLOSSARY_PATH:-}
VERIFY_MODE=${VERIFY_MODE:-smoke}

RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/language.toml
SUB=$SMOKE_DIR/subtitles.jsonl
EXCERPT=$SMOKE_DIR/speech.wav
VERIFY_JSON=$SMOKE_DIR/verify.json

# 语言链路要真实推理：清掉 stub 注入口；共享库与 llama-server 走资产根。
unset MOZART_ASR_STUB MOZART_ASR_STUB_TEXT MOZART_TTS_STUB
export LD_LIBRARY_PATH="$ASSETS_DIR/lib:$ASSETS_DIR/llama${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export MOZART_LLAMA_BIN="$ASSETS_DIR/llama/llama-server"
# 运行时状态与桥开关存档独立到本次产物目录：语言链路以 TOML 为基线
# （startup_mode=broadcast、speech_enabled=true），不继承设备全局状态改动。
export MOZART_STATE_DIR="$SMOKE_DIR/state"

ts() { printf "[%5ss]" "$SECONDS"; }
ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
# status JSON 取值函数与 smoke/measure/file-test 共用（jget.sh 有键位说明）。
. "$SCRIPT_DIR/jget.sh"

FAILS=0
ok() { echo "  ok   $*"; }
bad() { echo "  FAIL $*" >&2; FAILS=$((FAILS + 1)); }
assert_eq() { if [ "$2" = "$3" ]; then ok "$1=$3"; else bad "$1=$2（期望 $3）"; fi; }

LAST_STATUS_ERR=""
# 原子抓取 status JSON：先写临时文件，确认成功且非空后再覆盖目标，保留错误原因。
fetch_status() {
  local dest=$1 tmp="${1}.tmp" err_file="${1}.err"
  if ctl status --json > "$tmp" 2> "$err_file" && [ -s "$tmp" ]; then
    mv -f "$tmp" "$dest"
    rm -f "$err_file"
    LAST_STATUS_ERR=""
    return 0
  fi
  LAST_STATUS_ERR=$(tr '\n' ' ' < "$err_file" 2>/dev/null | sed 's/[[:space:]]*$//')
  [ -n "$LAST_STATUS_ERR" ] || LAST_STATUS_ERR="ctl status 返回空输出或非零退出码"
  rm -f "$tmp" "$err_file"
  return 1
}

cleanup() {
  ctl down >/dev/null 2>&1 || true
  [ -n "${AREC_PID:-}" ] && kill "$AREC_PID" 2>/dev/null
  return 0
}
trap cleanup EXIT

fail_fast() { echo "LANGUAGE TEST FAILED: $*" >&2; exit 1; }

# ---- 参数与翻译模型选择 ----
case "$VERIFY_MODE" in
  smoke|strict) ;;
  *) fail_fast "非法 VERIFY_MODE=$VERIFY_MODE（仅允许 smoke 或 strict）" ;;
esac
case "$LLAMA_LEGACY_CONFIG" in
  0|1) ;;
  *) fail_fast "非法 LLAMA_LEGACY_CONFIG=$LLAMA_LEGACY_CONFIG（仅允许 0 或 1）" ;;
esac
for num_var in TAIL_SILENCE_S READY_TIMEOUT DRAIN_TIMEOUT; do
  num_val=${!num_var}
  case "$num_val" in
    ""|*[!0-9]*) fail_fast "非法 ${num_var}=${num_val}（须为非负整数）" ;;
  esac
done
if [ "$TAIL_SILENCE_S" -lt 2 ]; then
  fail_fast "TAIL_SILENCE_S=$TAIL_SILENCE_S 不足 2 s，无法保证尾句触发 endpoint"
fi
if [ "$READY_TIMEOUT" -lt 1 ] || [ "$DRAIN_TIMEOUT" -lt 1 ]; then
  fail_fast "READY_TIMEOUT=$READY_TIMEOUT 与 DRAIN_TIMEOUT=$DRAIN_TIMEOUT 均须 >= 1"
fi

DEFAULT_HY_MT_MODEL="$ASSETS_DIR/models/llm/Hy-MT3-1.8B-Q4_K_M.gguf"
DEFAULT_QWEN_MODEL="$ASSETS_DIR/models/llm/qwen35-0.8b-text-Q4_K_M.gguf"
MODEL_FALLBACK_NOTE=""
if [ -z "${LLAMA_MODEL:-}" ]; then
  if [ -e "$DEFAULT_HY_MT_MODEL" ]; then
    LLAMA_MODEL="$DEFAULT_HY_MT_MODEL"
  elif [ -e "$DEFAULT_QWEN_MODEL" ]; then
    LLAMA_MODEL="$DEFAULT_QWEN_MODEL"
    MODEL_FALLBACK_NOTE="未找到默认翻译模型 $DEFAULT_HY_MT_MODEL，自动回退至 $LLAMA_MODEL"
  else
    LLAMA_MODEL="$DEFAULT_HY_MT_MODEL"
  fi
fi

LLAMA_BASENAME=$(basename "$LLAMA_MODEL")
if [ -z "${LLAMA_MODEL_ID:-}" ]; then
  LLAMA_MODEL_ID="${LLAMA_BASENAME%.gguf}"
fi
if [ -z "${LLAMA_PROFILE:-}" ]; then
  case "$LLAMA_BASENAME" in
    *Hy-MT*|*HY-MT*|*hy-mt*) LLAMA_PROFILE="hy_mt" ;;
    *) LLAMA_PROFILE="qwen_chat" ;;
  esac
fi
if [ "$LLAMA_LEGACY_CONFIG" = "0" ]; then
  case "$LLAMA_PROFILE" in
    hy_mt|qwen_chat) ;;
    custom) fail_fast "LLAMA_PROFILE=custom 暂不支持环境变量透传 user_template，请改用自定义配置文件" ;;
    *) fail_fast "非法 LLAMA_PROFILE=$LLAMA_PROFILE（仅允许 hy_mt 或 qwen_chat）" ;;
  esac
fi

# 可选模型参数覆盖：与 core/src/config.rs LlmModelConfig::default() 一致的默认值不重复写入 TOML。
LLAMA_CTX_SIZE=${LLAMA_CTX_SIZE:-}
LLAMA_CACHE_RAM_MB=${LLAMA_CACHE_RAM_MB:-}
LLAMA_MAX_TOKENS=${LLAMA_MAX_TOKENS:-}
LLAMA_CTX_WINDOW=${LLAMA_CTX_WINDOW:-}
for num_var in LLAMA_CTX_SIZE LLAMA_CACHE_RAM_MB LLAMA_MAX_TOKENS LLAMA_CTX_WINDOW; do
  num_val=${!num_var}
  if [ -n "$num_val" ]; then
    case "$num_val" in
      *[!0-9]*) fail_fast "非法 ${num_var}=${num_val}（须为非负整数）" ;;
    esac
  fi
done

[ -n "$MODEL_FALLBACK_NOTE" ] && echo "提示：$MODEL_FALLBACK_NOTE"

mkdir -p "$SMOKE_DIR"
rm -rf "$RUN" "$SUB" "$EXCERPT" "$SMOKE_DIR"/rec.raw "$SMOKE_DIR"/st-*.json \
       "$SMOKE_DIR"/st-*.tmp "$SMOKE_DIR"/st-*.err "$SMOKE_DIR"/drain.err \
       "$VERIFY_JSON" "$SMOKE_DIR/state"

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
[ -e "$LLAMA_MODEL" ] || fail_fast "缺翻译模型 $LLAMA_MODEL"
[ -e "$SAMPLE_M4A" ] || fail_fast "缺语音样本 $SAMPLE_M4A"
[ -f "$SCRIPT_DIR/verify-language-run.py" ] || fail_fast "缺核验工具 $SCRIPT_DIR/verify-language-run.py"
if [ -n "$GLOSSARY_PATH" ]; then
  [ -e "$GLOSSARY_PATH" ] || fail_fast "缺术语表文件 $GLOSSARY_PATH"
fi

SHERPA_LINK=$(ldd "$BIN_DIR/mozart-asr" 2>/dev/null | grep sherpa-onnx || true)
case "$SHERPA_LINK" in
  *"not found"*) fail_fast "mozart-asr 的 sherpa 库不可加载：LD_LIBRARY_PATH 缺 $ASSETS_DIR/lib" ;;
  "") fail_fast "mozart-asr 未链接 sherpa（重跑部署或 SHERPA_ONNX_LIB_DIR=$ASSETS_DIR/lib cargo build --workspace --bins --examples）" ;;
esac

# 单次读取排空轮询所需的全部状态字段，严禁把缺失对象/字段静默补 0。
drain_snapshot() {
  python3 - "$1" "$2" <<'PY'
import json, os, sys

def req_int(mapping, name):
    val = mapping.get(name)
    if isinstance(val, bool) or not isinstance(val, int):
        raise ValueError(f"missing/invalid int field {name}={val!r}")
    return val

status_path, sub_path = sys.argv[1], sys.argv[2]
with open(status_path, encoding="utf-8") as fh:
    d = json.load(fh)
if not isinstance(d, dict):
    raise ValueError("status root is not JSON object")

missing_sections = [k for k in ("asr", "bridge", "tts", "pre") if not isinstance(d.get(k), dict)]
if missing_sections:
    raise ValueError(f"missing status sections: {', '.join(missing_sections)}")

asr = d["asr"]
br = d["bridge"]
tts = d["tts"]
pre = d["pre"]

finals = req_int(asr, "finals")
tq = req_int(br, "translate_queue_depth")
sq = req_int(br, "speech_queue_depth")
translated = req_int(br, "translated")
tr_failed = req_int(br, "translate_failed")
qc_rej = req_int(br, "qc_rejected")
tts_q = req_int(tts, "queue_depth")
synth = req_int(tts, "synthesized")
tts_played = req_int(pre, "tts_frames_played")

tts_state = tts.get("state")
if not isinstance(tts_state, str):
    raise ValueError(f"missing/invalid tts.state={tts_state!r}")
tts_idle = 1 if tts_state == "idle" else 0

sub_finals = 0
sub_pending = 0
if os.path.isfile(sub_path):
    latest = {}
    with open(sub_path, encoding="utf-8") as sf:
        for line_no, line in enumerate(sf, 1):
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError as exc:
                raise ValueError(f"incomplete subtitles.jsonl line {line_no}: {exc}") from exc
            uid = row.get("utterance_id")
            if not uid:
                continue
            rev = row.get("revision", -1)
            prev = latest.get(uid)
            if prev is None or rev >= prev[0]:
                latest[uid] = (rev, row)
    for _, row in latest.values():
        if row.get("final") is True:
            sub_finals += 1
            if row.get("refinement_status") == "pending" or row.get("translation_status") == "pending":
                sub_pending += 1

print(finals, tq, sq, translated, tr_failed, qc_rej, tts_q, tts_idle, synth, tts_played, sub_finals, sub_pending)
PY
}

# ---- 语音样本：真实录音截取 ----
ffmpeg -y -v error -ss "$SS_S" -t "$T_S" -i "$SAMPLE_M4A" -ar 48000 -ac 2 "$EXCERPT" \
  || fail_fast "ffmpeg 截取失败"

# ---- 配置（broadcast 独占播报隔离 + 可扩展 [[bridge.models]] 契约） ----
cat > "$CFG" <<EOF
[daemon]
startup_mode = "broadcast"
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
EOF

if [ "$LLAMA_LEGACY_CONFIG" = "1" ]; then
  cat >> "$CFG" <<EOF

[bridge]
subtitles_path = "$SUB"
llama_model = "$LLAMA_MODEL"
translate_enabled = true
speech_enabled = true
EOF
  if [ -n "$GLOSSARY_PATH" ]; then
    echo "glossary_path = \"$GLOSSARY_PATH\"" >> "$CFG"
  fi
  echo "=== config ==="
  echo "  model=$LLAMA_MODEL (legacy=1, bridge.llama_model; 推理参数沿用 config.rs legacy_default) verify_mode=$VERIFY_MODE"
else
  # 与 core/src/config.rs LlmModelConfig::default()（ctx_size=1024, cache_ram_mb=64,
  # max_tokens=256, temperature=0.0, context_window=0）相同的键不重复写入，仅写差异项与显式覆盖。
  TOML_CTX_SIZE=$LLAMA_CTX_SIZE
  TOML_CACHE_RAM_MB=$LLAMA_CACHE_RAM_MB
  TOML_MAX_TOKENS=$LLAMA_MAX_TOKENS
  TOML_CTX_WINDOW=$LLAMA_CTX_WINDOW
  if [ "$LLAMA_PROFILE" = "hy_mt" ]; then
    [ -n "$TOML_CTX_WINDOW" ] || TOML_CTX_WINDOW=2
  else
    [ -n "$TOML_CTX_SIZE" ] || TOML_CTX_SIZE=2048
    [ -n "$TOML_CACHE_RAM_MB" ] || TOML_CACHE_RAM_MB=128
  fi

  cat >> "$CFG" <<EOF

[bridge]
subtitles_path = "$SUB"
translate_enabled = true
speech_enabled = true
llama_model_id = "$LLAMA_MODEL_ID"
EOF
  if [ -n "$GLOSSARY_PATH" ]; then
    echo "glossary_path = \"$GLOSSARY_PATH\"" >> "$CFG"
  fi
  cat >> "$CFG" <<EOF

[[bridge.models]]
id = "$LLAMA_MODEL_ID"
path = "$LLAMA_MODEL"
prompt_profile = "$LLAMA_PROFILE"
EOF
  [ -n "$TOML_CTX_SIZE" ] && echo "ctx_size = $TOML_CTX_SIZE" >> "$CFG"
  [ -n "$TOML_CACHE_RAM_MB" ] && echo "cache_ram_mb = $TOML_CACHE_RAM_MB" >> "$CFG"
  [ -n "$TOML_MAX_TOKENS" ] && echo "max_tokens = $TOML_MAX_TOKENS" >> "$CFG"
  [ -n "$TOML_CTX_WINDOW" ] && echo "context_window = $TOML_CTX_WINDOW" >> "$CFG"

  echo "=== config ==="
  echo "  model=$LLAMA_MODEL (id=$LLAMA_MODEL_ID profile=$LLAMA_PROFILE legacy=0) verify_mode=$VERIFY_MODE"
  echo "  toml_overrides: ctx_size=${TOML_CTX_SIZE:-default(1024)} cache_ram_mb=${TOML_CACHE_RAM_MB:-default(64)} max_tokens=${TOML_MAX_TOKENS:-default(256)} context_window=${TOML_CTX_WINDOW:-default(0)}"
fi
[ -n "$GLOSSARY_PATH" ] && echo "  glossary_path=$GLOSSARY_PATH"

# ---- 启动与就绪 ----
echo "=== up ==="
ctl up || fail_fast "up 失败"
READY=0
STATES=""
for i in $(seq 1 "$READY_TIMEOUT"); do
  if fetch_status "$SMOKE_DIR/st-poll.json"; then
    STATES=""
    for role in pre rvc asr tts bridge llama; do
      s=$(jget "$SMOKE_DIR/st-poll.json" path "processes.$role.state" 2>/dev/null)
      STATES="$STATES $role=$s"
      [ "$s" = "running" ] || { READY=0; break; }
      READY=1
    done
    [ "$READY" = 1 ] && break
  else
    STATES=" status_err=($LAST_STATUS_ERR)"
    READY=0
  fi
  sleep 1
done
if [ "$READY" != 1 ]; then
  [ -f "$SMOKE_DIR/st-poll.json" ] && cp "$SMOKE_DIR/st-poll.json" "$SMOKE_DIR/st-ready.json" 2>/dev/null || true
  tail -20 "$RUN/logs/llama.log" >&2 2>/dev/null
  fail_fast "六进程 ${i}s 内未全部 running：$STATES"
fi
echo "$(ts) processes running after ${i}s:$STATES"

# 等待 broadcast 独占模式与 bridge.speech_enabled 同步稳定，消除启动初期 1 s 竞态。
MODE_READY=0
MODE_STABLE=0
SPEECH=""
APPLIED_MODE=""
PLAYOUT_POLICY=""
MODE_PHASE=""
LAST_MODE_ERR=""
for m in $(seq 1 "$READY_TIMEOUT"); do
  if fetch_status "$SMOKE_DIR/st-ready.json"; then
    SPEECH=$(jget "$SMOKE_DIR/st-ready.json" path bridge.speech_enabled 2>/dev/null || true)
    APPLIED_MODE=$(jget "$SMOKE_DIR/st-ready.json" path output_mode.applied 2>/dev/null || true)
    PLAYOUT_POLICY=$(jget "$SMOKE_DIR/st-ready.json" path pre.playout_policy 2>/dev/null || true)
    MODE_PHASE=$(jget "$SMOKE_DIR/st-ready.json" path output_mode.phase 2>/dev/null || true)
    if [ "$SPEECH" = "true" ] && [ "$APPLIED_MODE" = "broadcast" ] && \
       [ "$PLAYOUT_POLICY" = "broadcast_only" ] && [ "$MODE_PHASE" = "applied" ]; then
      MODE_STABLE=$((MODE_STABLE + 1))
      if [ "$MODE_STABLE" -ge 2 ]; then
        MODE_READY=1
        break
      fi
    else
      MODE_STABLE=0
    fi
  else
    LAST_MODE_ERR="$LAST_STATUS_ERR"
    MODE_STABLE=0
  fi
  sleep 1
done

assert_eq "output_mode.applied" "${APPLIED_MODE:-}" "broadcast"
assert_eq "output_mode.phase" "${MODE_PHASE:-}" "applied"
assert_eq "pre.playout_policy" "${PLAYOUT_POLICY:-}" "broadcast_only"
assert_eq "bridge.speech_enabled（状态隔离）" "${SPEECH:-}" "true"
if [ "$MODE_READY" != 1 ]; then
  if [ -f "$SMOKE_DIR/st-ready.json" ]; then
    MODE_ERR=$(jget "$SMOKE_DIR/st-ready.json" path output_mode.last_error 2>/dev/null || true)
    DEGRADED=$(jget "$SMOKE_DIR/st-ready.json" path degraded 2>/dev/null || true)
    echo "  mode diagnostics: phase=${MODE_PHASE:-} last_error=${MODE_ERR:-} degraded=${DEGRADED:-}" >&2
  fi
  [ -n "$LAST_MODE_ERR" ] && echo "  last status error: $LAST_MODE_ERR" >&2
  fail_fast "broadcast 模式 ${READY_TIMEOUT}s 内未稳定就绪"
fi
echo "$(ts) broadcast mode stable after ${m}s"

# ---- 喂语音 + 尾静音，并回录播报输出端 ----
echo "=== play ${T_S}s speech + ${TAIL_SILENCE_S}s tail silence ==="
arecord -q -D "plughw:CARD=Loopback,DEV=1,SUBDEV=$PB_SUB" -f S16_LE -c 2 -r 48000 -t raw \
  "$SMOKE_DIR/rec.raw" &
AREC_PID=$!
aplay -q -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "$EXCERPT" \
  || fail_fast "aplay 语音样本失败"
# 补喂 48 kHz 立体声 S16_LE 零帧（192000 B/s），确保尾句可靠触发 ASR endpoint。
dd if=/dev/zero bs=192000 count="$TAIL_SILENCE_S" status=none \
  | aplay -q -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" -f S16_LE -c 2 -r 48000 -t raw \
  || fail_fast "aplay 尾静音失败"
echo "$(ts) playback + tail silence done, draining pipeline (timeout ${DRAIN_TIMEOUT}s)"

# ---- 等待全链路队列排空并连续两次轮询计数收敛 ----
DRAINED=0
STABLE_POLLS=0
PREV_SIG=""
LAST_DRAIN_ERR=""
DRAIN_ERR_FILE="$SMOKE_DIR/drain.err"
FINALS=0; TQ=0; SQ=0; TRANSLATED=0; TR_FAILED=0; QC_REJ=0; TTS_Q=0; TTS_IDLE=0; SYNTH=0; TTS_PLAYED=0; SUB_FINALS=0; SUB_PENDING=0
for i in $(seq 1 "$DRAIN_TIMEOUT"); do
  if ! fetch_status "$SMOKE_DIR/st-wait.json"; then
    LAST_DRAIN_ERR="status: $LAST_STATUS_ERR"
    PREV_SIG=""
    STABLE_POLLS=0
    sleep 1
    continue
  fi
  if SNAP_LINE=$(drain_snapshot "$SMOKE_DIR/st-wait.json" "$SUB" 2>"$DRAIN_ERR_FILE"); then
    LAST_DRAIN_ERR=""
    read -r FINALS TQ SQ TRANSLATED TR_FAILED QC_REJ TTS_Q TTS_IDLE SYNTH TTS_PLAYED SUB_FINALS SUB_PENDING <<< "$SNAP_LINE"
    CUR_SIG="${FINALS}:${TRANSLATED}:${TR_FAILED}:${QC_REJ}:${SYNTH}:${TTS_PLAYED}:${SUB_FINALS}"
    if [ "$FINALS" -ge 1 ] && [ "$SUB_FINALS" -ge "$FINALS" ] && [ "$TQ" -eq 0 ] && [ "$SQ" -eq 0 ] && \
       [ "$TTS_Q" -eq 0 ] && [ "$TTS_IDLE" -eq 1 ] && [ "$SUB_PENDING" -eq 0 ]; then
      if [ "$CUR_SIG" = "$PREV_SIG" ]; then
        STABLE_POLLS=$((STABLE_POLLS + 1))
      else
        PREV_SIG="$CUR_SIG"
        STABLE_POLLS=1
      fi
      if [ "$STABLE_POLLS" -ge 3 ]; then
        DRAINED=1
        echo "$(ts) drained after ${i}s: finals=$FINALS sub_finals=$SUB_FINALS translated=$TRANSLATED failed=$TR_FAILED qc_rejected=$QC_REJ synth=$SYNTH tts_frames=$TTS_PLAYED"
        break
      fi
    else
      PREV_SIG=""
      STABLE_POLLS=0
    fi
  else
    LAST_DRAIN_ERR="snapshot: $(tr '\n' ' ' < "$DRAIN_ERR_FILE" 2>/dev/null | sed 's/[[:space:]]*$//')"
    PREV_SIG=""
    STABLE_POLLS=0
  fi
  sleep 1
done
rm -f "$DRAIN_ERR_FILE"

kill "$AREC_PID" 2>/dev/null || true
wait "$AREC_PID" 2>/dev/null || true
AREC_PID=""

FINAL_STATUS_OK=0
if fetch_status "$SMOKE_DIR/st-final.json"; then
  FINAL_STATUS_OK=1
fi

# ---- 链路基础断言与自动化核验（先于 down，避免 ASR SIGTERM 尾切污染存档） ----
echo "=== assertions ==="
if [ "$DRAINED" -eq 1 ]; then
  ok "全链路队列排空并收敛"
else
  DRAIN_MSG="全链路排空超时（${DRAIN_TIMEOUT}s）：finals=$FINALS sub_finals=$SUB_FINALS tq=$TQ sq=$SQ tts_q=$TTS_Q tts_idle=$TTS_IDLE sub_pending=$SUB_PENDING"
  [ -n "$LAST_DRAIN_ERR" ] && DRAIN_MSG="$DRAIN_MSG last_err=($LAST_DRAIN_ERR)"
  bad "$DRAIN_MSG"
fi

if [ "$FINAL_STATUS_OK" -eq 1 ]; then
  LLAMA_ON=$(jget "$SMOKE_DIR/st-final.json" path bridge.llama_connected 2>/dev/null || echo false)
  DELIVERED=$(jget "$SMOKE_DIR/st-final.json" path bridge.speech_delivered 2>/dev/null || echo 0)
  assert_eq "bridge llama_connected" "$LLAMA_ON" "true"
  if [ "${DELIVERED:-0}" -ge 1 ]; then
    ok "bridge speech_delivered=$DELIVERED"
  else
    bad "bridge speech_delivered=${DELIVERED:-0}（期望 ≥1）"
  fi

  echo "=== verify-language-run.py (mode=$VERIFY_MODE) ==="
  python3 "$SCRIPT_DIR/verify-language-run.py" "$SMOKE_DIR" --mode "$VERIFY_MODE" --out-json "$VERIFY_JSON"
  VERIFY_RC=$?
  if [ "$VERIFY_RC" -eq 0 ]; then
    ok "verify-language-run.py (mode=$VERIFY_MODE) PASS"
  else
    bad "verify-language-run.py (mode=$VERIFY_MODE) FAIL（退出码 $VERIFY_RC）"
  fi
else
  bad "抓取 st-final.json 失败：$LAST_STATUS_ERR（已跳过依赖 st-final.json 的核验）"
fi

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
python3 - "$SMOKE_DIR/st-final.json" "$VERIFY_JSON" <<'PY'
import json, os, sys

def load_opt_json(path, label):
    if not os.path.isfile(path):
        return {}, None
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
        return (data if isinstance(data, dict) else {}), None
    except (OSError, json.JSONDecodeError) as exc:
        return {}, f"{label} 读取失败：{exc}"

st, st_err = load_opt_json(sys.argv[1], "st-final.json")
ver, ver_err = load_opt_json(sys.argv[2], "verify.json")

if st_err:
    print(f"st-final : {st_err}")
elif st:
    asr = st.get("asr") or {}
    br = st.get("bridge") or {}
    tts = st.get("tts") or {}
    pre = st.get("pre") or {}
    om = st.get("output_mode") or {}
    print(
        f"st-final : mode={om.get('applied')}/{om.get('phase')} policy={pre.get('playout_policy')} "
        f"asr_finals={asr.get('finals')} translated={br.get('translated')} "
        f"translate_failed={br.get('translate_failed')} qc_rejected={br.get('qc_rejected')} "
        f"speech_delivered={br.get('speech_delivered')} "
        f"tts(synth/skip/fail)={tts.get('synthesized')}/{tts.get('skipped')}/{tts.get('failed')}"
    )
else:
    print("st-final : MISSING")

if ver_err:
    print(f"verify   : {ver_err}")
elif ver:
    m = ver.get("metrics") or {}
    pb = m.get("playback") or {}
    s = ver.get("summary") or {}
    print(
        f"verify   : mode={ver.get('mode')} passed={ver.get('passed')} strict_passed={ver.get('strict_passed')} "
        f"required={s.get('required_passed', 0)}/{s.get('required_total', 0)} "
        f"advisory_warn={s.get('advisory_warn', 0)}/{s.get('advisory_total', 0)}"
    )
    print(
        f"metrics  : restarts={m.get('total_restarts')} bilingual={m.get('subtitle_bilingual_finals')} "
        f"dur_max={m.get('utt_dur_max_s')}s short_frag={m.get('short_frag_rate')} "
        f"unfinished={m.get('unfinished_tail_rate')} review_fail={m.get('review_failed_rate')} "
        f"silent_drop={m.get('silent_drop_rate')} "
        f"mt_p50/p95={m.get('final_to_mt_p50_ms')}/{m.get('final_to_mt_p95_ms')}ms "
        f"rms={pb.get('overall_rms')} gap_max={pb.get('gap_max_s')}s gaps>1.5s={pb.get('gaps_gt_1p5s')}"
    )
else:
    print("verify   : SKIPPED/MISSING")
PY
echo "证据目录：$SMOKE_DIR（st-final.json / verify.json / subtitles.jsonl / rec.raw）"
if [ "$FAILS" -eq 0 ]; then
  echo "LANGUAGE TEST PASSED"
  exit 0
fi
echo "LANGUAGE TEST FAILED: $FAILS 项断言失败" >&2
exit 1
