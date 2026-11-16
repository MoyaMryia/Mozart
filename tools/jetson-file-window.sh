#!/bin/bash
# 文件链分窗基准 runner（docs/RVC-FILE-BENCHMARK.md）。
# MODEL_IDS × DURATIONS 矩阵，输入 $INPUT_DIR/gkw_male_<dur>s.wav（原始 WAV 直提，
# daemon 解码）。一次起栈串行跑完，证据留 $SMOKE_DIR。
# 判据：completed、frames_in==frames_out、输出样本=frames_in×960、48 kHz 单声道、
#   引擎加载不回退 ONNX（"加载失败，回退 ONNX" 计数不增）。
#   HuBERT/RMVPE 超 profile 的动态 ONNX 兜底（"Lazy-loaded ONNX fallback"）只记录
#   不判失败——长文件整段 RMVPE 超 profile 属预期（SPEC-12、benchmark §2.1）。
# 互斥：有 mozart-daemon 在跑时默认中止；STOP_RUNTIME=1 先停运行栈再跑。
set -u

BIN_DIR=${BIN_DIR:-$HOME/mozart-runtime/bin}
MODELS_DIR=${MODELS_DIR:-$HOME/mozart-assets/models/rvc}
MODEL_IDS=${MODEL_IDS:-"trump-v2 miku-v2-48k"}
DURATIONS=${DURATIONS:-"10 30 60 120"}
INPUT_DIR=${INPUT_DIR:-/tmp/mozart-window-inputs}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-window-bc}
STOP_RUNTIME=${STOP_RUNTIME:-0}
RESTORE_RUNTIME=${RESTORE_RUNTIME:-1}
JOB_TIMEOUT=${JOB_TIMEOUT:-900}
CAP_SUB=${CAP_SUB:-2}
PB_SUB=${PB_SUB:-3}
RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/window.toml
FILES=$SMOKE_DIR/files
RUNTIME_CFG=$HOME/mozart-runtime/mozart.toml
LOG=$RUN/logs/rvc.log

ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
count_log() {
  local n
  n=$(grep -c "$1" "$LOG" 2>/dev/null || true)
  printf '%s' "${n:-0}"
}
wait_log_count() {
  local deadline=$((SECONDS + $3))
  while [ "$(count_log "$1")" -le "$2" ]; do
    [ "$SECONDS" -ge "$deadline" ] && return 1
    sleep 0.2
  done
  return 0
}
cleanup() {
  if [ -S "$RUN/daemon.sock" ]; then ctl down >/dev/null 2>&1; fi
  sleep 0.5
}
restore_runtime() {
  [ "$RESTORE_RUNTIME" = 1 ] || return 0
  "$BIN_DIR/mozart-ctl" --config "$RUNTIME_CFG" up >/dev/null 2>&1 || true
}
trap 'cleanup; restore_runtime' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for tool in python3 ffprobe ffmpeg; do
  command -v "$tool" >/dev/null || { echo "WINDOW FAILED: 缺少 $tool" >&2; exit 1; }
done
python3 -c 'import numpy' 2>/dev/null \
  || { echo "WINDOW FAILED: python3 缺 numpy" >&2; exit 1; }
for bin in mozart-ctl mozart-daemon mozart-pre mozart-rvc; do
  [ -x "$BIN_DIR/$bin" ] || { echo "WINDOW FAILED: 缺 $BIN_DIR/$bin" >&2; exit 1; }
done
[ -d /proc/asound/Loopback ] || { echo "WINDOW FAILED: snd-aloop 未加载" >&2; exit 1; }

if pgrep -f mozart-daemon >/dev/null 2>&1; then
  if [ "$STOP_RUNTIME" = 1 ]; then
    "$BIN_DIR/mozart-ctl" --config "$RUNTIME_CFG" down >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      pgrep -f mozart-daemon >/dev/null 2>&1 || break
      sleep 0.5
    done
  fi
  if pgrep -f mozart-daemon >/dev/null 2>&1; then
    echo "WINDOW FAILED: 仍有 mozart-daemon 在跑（互斥），先等完或停掉" >&2
    exit 1
  fi
fi

FEATURE_MODEL=""
for m in $MODEL_IDS; do
  d=$MODELS_DIR/$m
  if [ -e "$d/hubert_base_dynamic.onnx" ] && [ -e "$d/rmvpe_dynamic.onnx" ] \
     && [ -e "$d/hubert-realtime.onnx" ] && [ -e "$d/rmvpe-realtime.onnx" ]; then
    FEATURE_MODEL=$m; break
  fi
done
[ -n "$FEATURE_MODEL" ] || { echo "WINDOW FAILED: 无模型含完整特征引擎" >&2; exit 1; }
FEATURE_DIR=$MODELS_DIR/$FEATURE_MODEL

mkdir -p "$SMOKE_DIR" "$FILES"
rm -rf "$RUN"
rm -f "$SMOKE_DIR"/*.json "$SMOKE_DIR"/*.wav "$SMOKE_DIR"/*.tsv "$SMOKE_DIR"/*.toml

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
index_rate = 0.0
models_dir = "$MODELS_DIR"
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
max_duration_seconds = 300
max_upload_bytes = 104857600
ffmpeg_path = "/usr/bin/ffmpeg"
ffmpeg_timeout_ms = 120000
EOF
cp "$CFG" "$SMOKE_DIR/config-used.toml"

wait_ready() {
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
ctl up || { echo "WINDOW FAILED: ctl up 失败" >&2; exit 1; }
wait_ready 120 || { echo "WINDOW FAILED: pre/rvc 未就绪" >&2; exit 1; }
echo "feature_model=$FEATURE_MODEL models=$MODEL_IDS durations=$DURATIONS"

printf 'model\tdur\tstate\tframes_in\tframes_out\twindows\tout_samples\tinfer_ms\twall_s\trms\tpeak\tvmhwm_mib\tfeature_fallback\treason\n' \
  > "$SMOKE_DIR/summary.tsv"
FAIL_N=0
for m in $MODEL_IDS; do
  for dur in $DURATIONS; do
    INPUT=$INPUT_DIR/gkw_male_${dur}s.wav
    TAG=$m-$dur
    echo "=== $TAG ==="
    if [ ! -f "$INPUT" ]; then
      printf '%s\t%s\tNO_INPUT\t-\t-\t-\t-\t-\t-\t-\t-\t-\t-\t%s\n' \
        "$m" "$dur" "$INPUT" >> "$SMOKE_DIR/summary.tsv"
      FAIL_N=$((FAIL_N + 1)); continue
    fi
    T0=$SECONDS
    FINISHED_BEFORE=$(count_log "FileWorker finished")
    LOADFAIL_BEFORE=$(count_log "加载失败，回退 ONNX")
    LAZY_BEFORE=$(count_log "Lazy-loaded ONNX fallback")
    SUB=$(ctl file submit "$INPUT" --model "$m" --json 2>"$SMOKE_DIR/$TAG.submit.err") || SUB=""
    ID=$(printf '%s' "$SUB" | python3 -c 'import json,sys;print(json.load(sys.stdin)["job_id"])' 2>/dev/null || true)
    if [ -z "$ID" ]; then
      printf '%s\t%s\tSUBMIT_FAIL\t-\t-\t-\t-\t-\t-\t-\t-\t-\t-\t%s\n' \
        "$m" "$dur" "$(tr '\n' ' ' < "$SMOKE_DIR/$TAG.submit.err" | cut -c1-160)" \
        >> "$SMOKE_DIR/summary.tsv"
      FAIL_N=$((FAIL_N + 1)); continue
    fi
    STATE=""
    DEADLINE=$((SECONDS + JOB_TIMEOUT))
    while :; do
      ctl file status "$ID" --json > "$SMOKE_DIR/$TAG.job.json" 2>/dev/null
      STATE=$(python3 -c 'import json;print(json.load(open("'"$SMOKE_DIR/$TAG.job.json"'")).get("state",""))' 2>/dev/null || true)
      case "$STATE" in completed|failed|cancelled) break ;; esac
      if [ "$SECONDS" -ge "$DEADLINE" ]; then STATE="timeout"; break; fi
      sleep 1
    done
    WALL=$((SECONDS - T0))
    ctl status --json > "$SMOKE_DIR/$TAG.status.json" 2>/dev/null
    FETCH_OK=0
    if [ "$STATE" = completed ]; then
      ctl file fetch "$ID" -o "$SMOKE_DIR/$TAG.wav" --json > "$SMOKE_DIR/$TAG.fetch.json" 2>&1 \
        && FETCH_OK=1
    fi
    FRAMES_IN=""; FRAMES_OUT=""; OUT_SAMPLES=""; WINDOWS=""
    if [ "$STATE" = completed ] && wait_log_count "FileWorker finished" "$FINISHED_BEFORE" 15; then
      LINE=$(grep "FileWorker finished" "$LOG" | sed -n "$((FINISHED_BEFORE + 1))p")
      FRAMES_IN=$(printf '%s' "$LINE" | sed -n 's/.*finished: \([0-9]*\) frames in, \([0-9]*\) frames out.*/\1/p')
      FRAMES_OUT=$(printf '%s' "$LINE" | sed -n 's/.*finished: \([0-9]*\) frames in, \([0-9]*\) frames out.*/\2/p')
      OUT_SAMPLES=$(printf '%s' "$LINE" | sed -n 's/.*frames out, \([0-9]*\) output samples.*/\1/p')
      WINDOWS=$(printf '%s' "$LINE" | sed -n 's/.*output samples, \([0-9]*\) windows.*/\1/p')
    fi
    LOADFAIL_AFTER=$(count_log "加载失败，回退 ONNX")
    LAZY_AFTER=$(count_log "Lazy-loaded ONNX fallback")
    FEATURE_FALLBACK=""
    if [ "$LAZY_AFTER" -gt "$LAZY_BEFORE" ]; then
      FEATURE_FALLBACK=$(grep "Lazy-loaded ONNX fallback" "$LOG" | tail -n $((LAZY_AFTER - LAZY_BEFORE)) | sed 's/.*fallback: //' | tr '\n' ',' )
    fi
    RVC_PID=$(pgrep -f "mozart-rvc --socket" | head -1)
    VMHWM=$(awk '/VmHWM/{print int($2/1024)}' "/proc/$RVC_PID/status" 2>/dev/null || echo "")
    REASON=""
    if [ "$STATE" != "completed" ]; then
      REASON=$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
e = d.get("error") or ""
print(e.get("message") if isinstance(e, dict) else e)
' "$SMOKE_DIR/$TAG.job.json" 2>/dev/null | tr '\t' ' ')
      FAIL_N=$((FAIL_N + 1))
    else
      [ "$FETCH_OK" = 1 ] || { FAIL_N=$((FAIL_N + 1)); REASON="fetch 失败"; }
      { [ -n "$FRAMES_IN" ] && [ "$FRAMES_IN" = "$FRAMES_OUT" ]; } \
        || { FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }frames $FRAMES_IN/$FRAMES_OUT"; }
      EXPECTED=$((FRAMES_IN * 960))
      { [ -n "$OUT_SAMPLES" ] && [ "$OUT_SAMPLES" = "$EXPECTED" ]; } \
        || { FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }samples $OUT_SAMPLES != $EXPECTED"; }
      [ "$LOADFAIL_AFTER" = "$LOADFAIL_BEFORE" ] \
        || { FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }引擎加载回退 ONNX"; }
    fi
    STATS=$(python3 - "$SMOKE_DIR/$TAG.wav" <<'PY' 2>/dev/null || echo "-"
import subprocess, sys
import numpy as np
raw = subprocess.run(["ffmpeg","-v","error","-i",sys.argv[1],"-f","f32le","-ac","1","-ar","48000","-"],
                     capture_output=True, check=True).stdout
x = np.frombuffer(raw, dtype="<f4")
print(f"{np.sqrt(np.mean(x**2)):.5f}\t{np.max(np.abs(x)):.5f}")
PY
)
    RMS=$(printf '%s' "$STATS" | cut -f1)
    PEAK=$(printf '%s' "$STATS" | cut -f2)
    INFER_MS=$(python3 -c 'import json;print((json.load(open("'"$SMOKE_DIR/$TAG.status.json"'")).get("rvc") or {}).get("inference_ms_last",""))' 2>/dev/null || true)
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$m" "$dur" "$STATE" "$FRAMES_IN" "$FRAMES_OUT" "$WINDOWS" "$OUT_SAMPLES" \
      "$INFER_MS" "$WALL" "$RMS" "$PEAK" "$VMHWM" "$FEATURE_FALLBACK" "$REASON" \
      >> "$SMOKE_DIR/summary.tsv"
    tail -1 "$SMOKE_DIR/summary.tsv"
  done
done

echo "=== summary ==="
cat "$SMOKE_DIR/summary.tsv"
if [ "$FAIL_N" -ne 0 ]; then
  echo "WINDOW FAILED: $FAIL_N 项" >&2
  exit 1
fi
echo "WINDOW PASSED"