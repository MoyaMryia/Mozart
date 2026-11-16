#!/bin/bash
# Jetson 多模型文件转换板端实测：同一输入依次提交 MODELS_DIR 下的全部模型。
# 正常由 tools/jetson-e2e.sh --file-models 调用，也可在设备上直接运行。
# 前置：本仓库已 rsync 到设备并完成构建（tools/deploy-jetson.sh），BIN_DIR 内
#   有 mozart-ctl/daemon/pre/rvc；snd-aloop 已加载；MODELS_DIR 下每个子目录是一
#   个模型（config.json + 生成器；特征引擎取 FEATURE_MODEL，缺省用第一个模型）。
# 模型文件经符号链接放入 SMOKE_DIR 下的临时模型目录：预设写入临时目录，
#   不污染只读资产树。
# 判据：每个模型 job completed、输出 48 kHz 单声道、输出采样数等于 3×输入采样数、
#   RMS 非零；rvc 日志的 FileWorker 行必须 frames_in == frames_out，且本轮不得
#   出现 TensorRT 回退 ONNX。MODEL_IDS 可指定子集（空格分隔），缺省为全部目录。
#   INDEX_RATE 可设 >0 以覆盖索引检索路径（缺省 0）。
# 证据留在 $SMOKE_DIR：配置、输入、各模型 job/fetch/stats 与 summary.tsv、run/logs。
set -u

BIN_DIR=${BIN_DIR:-$HOME/mozart-runtime/bin}
MODELS_DIR=${MODELS_DIR:-$HOME/mozart-assets/models/rvc}
FEATURE_MODEL=${FEATURE_MODEL:-}
MODEL_IDS=${MODEL_IDS:-}
SAMPLE_M4A=${SAMPLE_M4A:-${SAMPLE:-$HOME/mozart-samples/jieshe.m4a}}
DURATION_S=${DURATION_S:-10}
INDEX_RATE=${INDEX_RATE:-0.0}
MAX_DURATION_S=${MAX_DURATION_S:-60}
JOB_TIMEOUT=${JOB_TIMEOUT:-420}
RESTORE_RUNTIME=${RESTORE_RUNTIME:-1}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-multimodel}
CAP_SUB=${CAP_SUB:-2}
PB_SUB=${PB_SUB:-3}
RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/multi.toml
FILES=$SMOKE_DIR/files
WORK_MODELS=$SMOKE_DIR/models
RUNTIME_CFG=$HOME/mozart-runtime/mozart.toml
LOG=$RUN/logs/rvc.log

ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
count_log() { # <模式>：日志匹配行数；无匹配回 0。
  local n
  n=$(grep -c "$1" "$LOG" 2>/dev/null || true)
  printf '%s' "${n:-0}"
}
wait_log_count() { # <模式> <下限> <超时秒>：等待匹配行数超过下限。
  local deadline=$((SECONDS + $3))
  while [ "$(count_log "$1")" -le "$2" ]; do
    [ "$SECONDS" -ge "$deadline" ] && return 1
    sleep 0.2
  done
  return 0
}

cleanup() {
  if [ -S "$RUN/daemon.sock" ]; then ctl down >/dev/null 2>&1; fi
  pkill -f mozart-daemon 2>/dev/null
  pkill -f "mozart-pre --socket" 2>/dev/null
  pkill -f "mozart-rvc --socket" 2>/dev/null
  sleep 0.3
}
restore_runtime() {
  [ "$RESTORE_RUNTIME" = 1 ] || return 0
  "$BIN_DIR/mozart-ctl" --config "$RUNTIME_CFG" up >/dev/null 2>&1 || true
}
trap 'cleanup; restore_runtime' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for tool in python3 ffprobe ffmpeg; do
  command -v "$tool" >/dev/null || { echo "MULTIMODEL FAILED: 缺少 $tool" >&2; exit 1; }
done
python3 -c 'import numpy' 2>/dev/null \
  || { echo "MULTIMODEL FAILED: python3 缺 numpy" >&2; exit 1; }
for bin in mozart-ctl mozart-daemon mozart-pre mozart-rvc; do
  [ -x "$BIN_DIR/$bin" ] || { echo "MULTIMODEL FAILED: 缺 $BIN_DIR/$bin" >&2; exit 1; }
done
[ -d /proc/asound/Loopback ] || { echo "MULTIMODEL FAILED: snd-aloop 未加载" >&2; exit 1; }
[ -d "$MODELS_DIR" ] || { echo "MULTIMODEL FAILED: 缺 $MODELS_DIR" >&2; exit 1; }

if [ -z "$MODEL_IDS" ]; then
  MODEL_IDS=$(find "$MODELS_DIR" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
fi
[ -n "$MODEL_IDS" ] || { echo "MULTIMODEL FAILED: MODELS_DIR 下无模型" >&2; exit 1; }
if [ -z "$FEATURE_MODEL" ]; then
  for m in $MODEL_IDS; do
    d=$MODELS_DIR/$m
    if [ -e "$d/hubert_base_dynamic.onnx" ] && [ -e "$d/rmvpe_dynamic.onnx" ] \
       && [ -e "$d/hubert-realtime.onnx" ] && [ -e "$d/rmvpe-realtime.onnx" ]; then
      FEATURE_MODEL=$m; break
    fi
  done
fi
[ -n "$FEATURE_MODEL" ] || { echo "MULTIMODEL FAILED: 无模型含完整特征引擎" >&2; exit 1; }
FEATURE_DIR=$MODELS_DIR/$FEATURE_MODEL
for asset in hubert_base_dynamic.onnx rmvpe_dynamic.onnx hubert-realtime.onnx \
             rmvpe-realtime.onnx; do
  [ -e "$FEATURE_DIR/$asset" ] || { echo "MULTIMODEL FAILED: 缺特征引擎 $FEATURE_DIR/$asset" >&2; exit 1; }
done

cleanup
mkdir -p "$SMOKE_DIR" "$FILES"
rm -rf "$RUN" "$FILES" "$WORK_MODELS"
mkdir -p "$WORK_MODELS"
for m in $MODEL_IDS; do
  mkdir -p "$WORK_MODELS/$m"
  find "$MODELS_DIR/$m" -mindepth 1 -maxdepth 1 ! -name presets \
    -exec ln -s {} "$WORK_MODELS/$m/" \;
done
rm -f "$SMOKE_DIR"/*.json "$SMOKE_DIR"/*.wav "$SMOKE_DIR"/*.tsv "$SMOKE_DIR"/*.toml \
      "$SMOKE_DIR"/*.log

INPUT=$SMOKE_DIR/input.wav
if [ -f "$SAMPLE_M4A" ]; then
  ffmpeg -nostdin -y -v error -t "$DURATION_S" -i "$SAMPLE_M4A" \
    -ac 1 -ar 16000 -c:a pcm_s16le "$INPUT" \
    || { echo "MULTIMODEL FAILED: 截取样本失败" >&2; exit 1; }
else
  python3 - "$INPUT" "$DURATION_S" <<'PY' \
    || { echo "MULTIMODEL FAILED: 生成输入失败" >&2; exit 1; }
import sys, wave
import numpy as np
path, dur = sys.argv[1], float(sys.argv[2])
sr = 16000
t = np.arange(int(sr * dur)) / sr
env = 0.5 * (0.55 + 0.45 * np.sin(2 * np.pi * 3.0 * t))
v = env * (np.sin(2 * np.pi * 180 * t) + 0.5 * np.sin(2 * np.pi * 360 * t)
           + 0.25 * np.sin(2 * np.pi * 540 * t))
pcm = (np.clip(v * 0.5, -1.0, 1.0) * 32767).astype('<i2')
with wave.open(path, 'wb') as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr)
    w.writeframes(pcm.tobytes())
PY
fi

cat > "$CFG" <<EOF
[daemon]
run_dir = "$RUN"
rpc_timeout_ms = 5000
start_timeout_ms = 60000

[process]
pre_bin = "mozart-pre"
rvc_bin = "mozart-rvc"

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
model_id = "$FEATURE_MODEL"
f0_method = "rmvpe"
index_rate = $INDEX_RATE
models_dir = "$WORK_MODELS"
device = "cuda"
half = false
cache_limit = 2
hubert_path = "$FEATURE_DIR/hubert_base_dynamic.onnx"
rmvpe_path = "$FEATURE_DIR/rmvpe_dynamic.onnx"
realtime_hubert_path = "$FEATURE_DIR/hubert-realtime.onnx"
realtime_rmvpe_path = "$FEATURE_DIR/rmvpe-realtime.onnx"

[file]
storage_dir = "$FILES"
queue_capacity = 50
storage_limit_bytes = 1048576000
denoise = false
max_duration_seconds = $MAX_DURATION_S
max_upload_bytes = 104857600
ffmpeg_path = "/usr/bin/ffmpeg"
ffmpeg_timeout_ms = 120000
EOF
cp "$CFG" "$SMOKE_DIR/config-used.toml"

audio_stats() { # <音频> <输出 json>：48 kHz 单声道解码后统计整段 RMS/峰值。
  python3 - "$1" "$2" <<'PY'
import json, subprocess, sys
import numpy as np
src, dst = sys.argv[1], sys.argv[2]
probe = json.loads(subprocess.run(
    ["ffprobe", "-v", "error", "-show_entries",
     "format=duration:stream=sample_rate,channels", "-of", "json", src],
    capture_output=True, text=True, check=True).stdout)
raw = subprocess.run(
    ["ffmpeg", "-v", "error", "-i", src, "-f", "f32le", "-ac", "1", "-ar", "48000", "-"],
    capture_output=True, check=True).stdout
x = np.frombuffer(raw, dtype="<f4").astype(np.float64)
stream = probe["streams"][0]
out = {
    "sample_rate": int(stream["sample_rate"]),
    "channels": int(stream["channels"]),
    "duration_s": float(probe["format"]["duration"]),
    "decoded_samples": int(x.size),
    "rms": float(np.sqrt(np.mean(x ** 2))) if x.size else 0.0,
    "peak": float(np.max(np.abs(x))) if x.size else 0.0,
}
json.dump(out, open(dst, "w"), ensure_ascii=False, indent=2)
PY
}

wait_ready() { # <超时秒>
  local deadline=$((SECONDS + $1))
  while :; do
    ctl status --json > "$SMOKE_DIR/st-poll.json" 2>/dev/null
    if python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
sys.exit(0 if d.get("pre") and (d.get("rvc") or {}).get("state") else 1)
' "$SMOKE_DIR/st-poll.json" 2>/dev/null; then return 0; fi
    [ "$SECONDS" -ge "$deadline" ] && return 1
    sleep 1
  done
}

echo "=== up ==="
ctl up || { echo "MULTIMODEL FAILED: ctl up 失败" >&2; exit 1; }
wait_ready 120 || { echo "MULTIMODEL FAILED: pre/rvc 未就绪" >&2; exit 1; }
echo "feature_model=$FEATURE_MODEL index_rate=$INDEX_RATE models=$(printf '%s ' $MODEL_IDS)"

audio_stats "$INPUT" "$SMOKE_DIR/input-stats.json"
IN_SAMPLES=$(python3 -c 'import json;print(json.load(open("'"$SMOKE_DIR"'/input-stats.json"))["decoded_samples"])')
# 输入按 20 ms 帧向上取整后，每个输出帧 960 样本。
EXPECTED_OUT=$(( (IN_SAMPLES + 959) / 960 * 960 ))
printf 'model\tstate\tframes_in\tframes_out\tdropped\tblocks\tinfer_ms\tout_dur\trms\tpeak\twall_s\treason\n' \
  > "$SMOKE_DIR/summary.tsv"
FAIL_N=0
for m in $MODEL_IDS; do
  echo "=== $m ==="
  T0=$SECONDS
  FINISHED_BEFORE=$(count_log "FileWorker finished")
  FALLBACK_BEFORE=$(count_log "回退 ONNX")
  CPU_BEFORE=$(count_log "Engine backend: ONNX Runtime")
  SUB=$(ctl file submit "$INPUT" --model "$m" --json 2>"$SMOKE_DIR/$m.submit.err") \
    || SUB=""
  if [ -z "$SUB" ]; then
    printf '%s\tSUBMIT_FAIL\t-\t-\t-\t-\t-\t-\t-\t-\t%s\t%s\n' \
      "$m" "$((SECONDS - T0))" "$(tr '\n' ' ' < "$SMOKE_DIR/$m.submit.err" | cut -c1-160)" \
      >> "$SMOKE_DIR/summary.tsv"
    FAIL_N=$((FAIL_N + 1)); continue
  fi
  ID=$(printf '%s' "$SUB" | python3 -c 'import json,sys;print(json.load(sys.stdin)["job_id"])' 2>/dev/null || true)
  if [ -z "$ID" ]; then
    printf '%s\tSUBMIT_FAIL\t-\t-\t-\t-\t-\t-\t-\t-\t%s\tjob_id 解析失败\n' \
      "$m" "$((SECONDS - T0))" >> "$SMOKE_DIR/summary.tsv"
    FAIL_N=$((FAIL_N + 1)); continue
  fi
  STATE=""
  DEADLINE=$((SECONDS + JOB_TIMEOUT))
  while :; do
    ctl file status "$ID" --json > "$SMOKE_DIR/$m.job.json" 2>/dev/null
    STATE=$(python3 -c 'import json;print(json.load(open("'"$SMOKE_DIR/$m.job.json"'")).get("state",""))' 2>/dev/null || true)
    case "$STATE" in completed|failed|cancelled) break ;; esac
    if [ "$SECONDS" -ge "$DEADLINE" ]; then STATE="timeout"; break; fi
    sleep 1
  done
  WALL=$((SECONDS - T0))
  ctl status --json > "$SMOKE_DIR/$m.status.json" 2>/dev/null
  FETCH_OK=0
  if [ "$STATE" = completed ]; then
    if ctl file fetch "$ID" -o "$SMOKE_DIR/$m.wav" --json > "$SMOKE_DIR/$m.fetch.json" 2>&1 \
       && audio_stats "$SMOKE_DIR/$m.wav" "$SMOKE_DIR/$m.stats.json"; then
      FETCH_OK=1
    fi
  fi
  # daemon 在收到末帧后即可置 completed；FileWorker 的 finished 行随后落盘。
  # 以提交前的日志行数为锚，只取本轮新增的 finished 行，避免读到上一模型。
  FRAMES_IN=""; FRAMES_OUT=""; OUT_SAMPLES=""
  if [ "$STATE" = completed ] && wait_log_count "FileWorker finished" "$FINISHED_BEFORE" 10; then
    # 只取提交后新增的第一条 finished 行；上一任务的延迟行已在计数锚点之前。
    LINE=$(grep "FileWorker finished" "$LOG" | sed -n "$((FINISHED_BEFORE + 1))p")
    FRAMES_IN=$(printf '%s' "$LINE" | sed -n 's/.*finished: \([0-9]*\) frames in, \([0-9]*\) frames out.*/\1/p')
    FRAMES_OUT=$(printf '%s' "$LINE" | sed -n 's/.*finished: \([0-9]*\) frames in, \([0-9]*\) frames out.*/\2/p')
    OUT_SAMPLES=$(printf '%s' "$LINE" | sed -n 's/.*frames out, \([0-9]*\) output samples.*/\1/p')
  fi
  FALLBACK_AFTER=$(count_log "回退 ONNX")
  CPU_AFTER=$(count_log "Engine backend: ONNX Runtime")
  REASON=""
  if [ "$STATE" != completed ]; then
    REASON=$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
e = d.get("error") or ""
print(e.get("message") if isinstance(e, dict) else e)
' "$SMOKE_DIR/$m.job.json" 2>/dev/null | tr '\t' ' ')
    FAIL_N=$((FAIL_N + 1))
  else
    if [ "$FETCH_OK" -ne 1 ]; then
      FAIL_N=$((FAIL_N + 1)); REASON="fetch 或统计失败"
    fi
    if [ -z "$FRAMES_IN" ] || [ "$FRAMES_IN" != "$FRAMES_OUT" ]; then
      FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }frames_in!=frames_out ($FRAMES_IN/$FRAMES_OUT)"
    fi
    if [ -z "$OUT_SAMPLES" ] || [ "$OUT_SAMPLES" != "$EXPECTED_OUT" ]; then
      FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }output samples $OUT_SAMPLES != $EXPECTED_OUT"
    fi
    if [ "$FETCH_OK" -eq 1 ] && ! python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
ok = d["sample_rate"]==48000 and d["channels"]==1 and d["rms"]>0.0001
sys.exit(0 if ok else 1)' "$SMOKE_DIR/$m.stats.json" 2>/dev/null; then
      FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }输出格式或 RMS 不合格"
    fi
    if [ "$FALLBACK_AFTER" -ne "$FALLBACK_BEFORE" ] || [ "$CPU_AFTER" -ne "$CPU_BEFORE" ]; then
      FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }TensorRT 回退 ONNX"
    fi
  fi
  python3 - "$m" "$STATE" "$WALL" "$SMOKE_DIR" "$REASON" "$FRAMES_IN" "$FRAMES_OUT" \
    <<'PY' >> "$SMOKE_DIR/summary.tsv"
import json, sys
m, state, wall, root, reason, frames_in, frames_out = sys.argv[1:8]
st = json.load(open(f"{root}/{m}.status.json"))
rvc = st.get("rvc") or {}
try:
    stats = json.load(open(f"{root}/{m}.stats.json"))
except OSError:
    stats = {}
    reason = f"{reason}; stats missing".lstrip("; ")
print("\t".join(str(v) for v in [
    m, state, frames_in, frames_out,
    rvc.get("frames_dropped"), rvc.get("blocks"), rvc.get("inference_ms_last"),
    stats.get("duration_s"), stats.get("rms"), stats.get("peak"), wall, reason,
]))
PY
  tail -1 "$SMOKE_DIR/summary.tsv"
done

echo "=== summary ==="
cat "$SMOKE_DIR/summary.tsv"
if [ "$FAIL_N" -ne 0 ]; then
  echo "MULTIMODEL FAILED: $FAIL_N 项" >&2
  exit 1
fi
echo "MULTIMODEL PASSED ($(printf '%s\n' $MODEL_IDS | wc -l) models)"
