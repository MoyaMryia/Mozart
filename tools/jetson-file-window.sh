#!/bin/bash
# 文件链路分窗基准 runner（docs/RVC-FILE-BENCHMARK.md）。
# MODEL_IDS × DURATIONS 矩阵，输入 $INPUT_DIR/gkw_male_<dur>s.wav（原始 WAV 直提）。
# 判据：completed、frames_in == frames_out、输出样本 = frames_in × 960、48 kHz 单声道、
#   引擎加载不得回退 ONNX。HuBERT/RMVPE 超 profile 的动态 ONNX 兜底是长文件预期行为，
#   只记录文件名不判失败（benchmark 文档 2.1）。
# 互斥：有 mozart-daemon 在跑时默认拒绝启动；STOP_RUNTIME=1 先停生产运行时再跑。
# 证据留 $SMOKE_DIR：config-used.toml、各 run 的 submit/job/status/fetch/wav、summary.tsv。
set -u

BIN_DIR=${BIN_DIR:-$HOME/mozart-runtime/bin}
MODELS_DIR=${MODELS_DIR:-$HOME/mozart-assets/models/rvc}
MODEL_IDS=${MODEL_IDS:-"trump-v2 miku-v2-48k"}
DURATIONS=${DURATIONS:-"30 60 120"}
INPUT_DIR=${INPUT_DIR:-/tmp/mozart-window-inputs}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-window-bc}
MAX_DURATION_S=${MAX_DURATION_S:-300}
JOB_TIMEOUT=${JOB_TIMEOUT:-900}
STOP_RUNTIME=${STOP_RUNTIME:-0}
RESTORE_RUNTIME=${RESTORE_RUNTIME:-1}
CAP_SUB=${CAP_SUB:-2}
PB_SUB=${PB_SUB:-3}
RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/multi.toml
FILES=$SMOKE_DIR/files
WORK_MODELS=$SMOKE_DIR/models
RUNTIME_CFG=$HOME/mozart-runtime/mozart.toml
LOG=$RUN/logs/rvc.log

ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
count_log() {
  local n
  n=$(grep -c "$1" "$LOG" 2>/dev/null || true)
  printf '%s' "${n:-0}"
}
wait_log_count() { # <模式> <下限> <超时秒>
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
  command -v "$tool" >/dev/null || { echo "WINDOW-BC FAILED: 缺少 $tool" >&2; exit 1; }
done
python3 -c 'import numpy' 2>/dev/null \
  || { echo "WINDOW-BC FAILED: python3 缺 numpy" >&2; exit 1; }
for bin in mozart-ctl mozart-daemon mozart-pre mozart-rvc; do
  [ -x "$BIN_DIR/$bin" ] || { echo "WINDOW-BC FAILED: 缺 $BIN_DIR/$bin" >&2; exit 1; }
done
[ -d /proc/asound/Loopback ] || { echo "WINDOW-BC FAILED: snd-aloop 未加载" >&2; exit 1; }

# 互斥检查：任何 daemon 在跑都先停下（本 runner 或生产运行时），不静默杀别人的测试。
if pgrep -f mozart-daemon >/dev/null 2>&1; then
  if [ "$STOP_RUNTIME" = 1 ]; then
    "$BIN_DIR/mozart-ctl" --config "$RUNTIME_CFG" down >/dev/null 2>&1 || true
    sleep 1
    if pgrep -f mozart-daemon >/dev/null 2>&1; then
      echo "WINDOW-BC FAILED: 仍有 daemon 在跑（可能是其他测试），等完再试" >&2
      exit 1
    fi
  else
    echo "WINDOW-BC FAILED: 有 mozart-daemon 在跑；等完或设 STOP_RUNTIME=1" >&2
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
[ -n "$FEATURE_MODEL" ] || { echo "WINDOW-BC FAILED: 无模型含完整特征引擎" >&2; exit 1; }
FEATURE_DIR=$MODELS_DIR/$FEATURE_MODEL

mkdir -p "$SMOKE_DIR" "$FILES"
rm -rf "$RUN" "$WORK_MODELS"
for m in $MODEL_IDS; do
  mkdir -p "$WORK_MODELS/$m"
  find "$MODELS_DIR/$m" -mindepth 1 -maxdepth 1 ! -name presets \
    -exec ln -s {} "$WORK_MODELS/$m/" \;
done
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
ctl up || { echo "WINDOW-BC FAILED: ctl up 失败" >&2; exit 1; }
wait_ready 120 || { echo "WINDOW-BC FAILED: pre/rvc 未就绪" >&2; exit 1; }
echo "feature_model=$FEATURE_MODEL models=$MODEL_IDS durations=$DURATIONS"

printf 'label\tmodel\tdur\tstate\tframes_in\tframes_out\tout_samples\twindows\tinfer_ms\twall_s\trvc_vmhwm_kib\tfeature_fallback\treason\n' \
  > "$SMOKE_DIR/summary.tsv"
FAIL_N=0
for m in $MODEL_IDS; do
  for dur in $DURATIONS; do
    LABEL="$m-gkw-$dur"
    INPUT=$INPUT_DIR/gkw_male_${dur}s.wav
    echo "=== $LABEL ==="
    if [ ! -f "$INPUT" ]; then
      printf '%s\t%s\t%s\tNO_INPUT\t-\t-\t-\t-\t-\t-\t-\t-\t缺输入 %s\n' \
        "$LABEL" "$m" "$dur" "$INPUT" >> "$SMOKE_DIR/summary.tsv"
      FAIL_N=$((FAIL_N + 1)); continue
    fi
    T0=$SECONDS
    FINISHED_BEFORE=$(count_log "FileWorker finished")
    LOADFAIL_BEFORE=$(count_log "加载失败，回退 ONNX")
    LAZY_BEFORE=$(count_log "Lazy-loaded ONNX fallback")
    SUB=$(ctl file submit "$INPUT" --model "$m" --json 2>"$SMOKE_DIR/$LABEL.submit.err") || SUB=""
    ID=$(printf '%s' "$SUB" | python3 -c 'import json,sys;print(json.load(sys.stdin)["job_id"])' 2>/dev/null || true)
    if [ -z "$ID" ]; then
      printf '%s\t%s\t%s\tSUBMIT_FAIL\t-\t-\t-\t-\t-\t%s\t-\t-\t%s\n' \
        "$LABEL" "$m" "$dur" "$((SECONDS - T0))" \
        "$(tr '\n' ' ' < "$SMOKE_DIR/$LABEL.submit.err" | cut -c1-160)" >> "$SMOKE_DIR/summary.tsv"
      FAIL_N=$((FAIL_N + 1)); continue
    fi
    STATE=""
    DEADLINE=$((SECONDS + JOB_TIMEOUT))
    while :; do
      ctl file status "$ID" --json > "$SMOKE_DIR/$LABEL.job.json" 2>/dev/null
      STATE=$(python3 -c 'import json;print(json.load(open("'"$SMOKE_DIR/$LABEL.job.json"'")).get("state",""))' 2>/dev/null || true)
      case "$STATE" in completed|failed|cancelled) break ;; esac
      if [ "$SECONDS" -ge "$DEADLINE" ]; then STATE="timeout"; break; fi
      sleep 1
    done
    WALL=$((SECONDS - T0))
    ctl status --json > "$SMOKE_DIR/$LABEL.status.json" 2>/dev/null
    RVC_PID=$(pgrep -f "mozart-rvc --socket" | head -1)
    VMHWM=$(awk '/VmHWM/{print $2}' "/proc/$RVC_PID/status" 2>/dev/null || true)
    FETCH_OK=0
    if [ "$STATE" = completed ]; then
      if ctl file fetch "$ID" -o "$SMOKE_DIR/$LABEL.wav" --json > "$SMOKE_DIR/$LABEL.fetch.json" 2>&1; then
        FETCH_OK=1
      fi
    fi
    FRAMES_IN=""; FRAMES_OUT=""; OUT_SAMPLES=""; WINDOWS=""
    if [ "$STATE" = completed ] && wait_log_count "FileWorker finished" "$FINISHED_BEFORE" 10; then
      LINE=$(grep "FileWorker finished" "$LOG" | sed -n "$((FINISHED_BEFORE + 1))p")
      FRAMES_IN=$(printf '%s' "$LINE" | sed -n 's/.*finished: \([0-9]*\) frames in, \([0-9]*\) frames out.*/\1/p')
      FRAMES_OUT=$(printf '%s' "$LINE" | sed -n 's/.*finished: \([0-9]*\) frames in, \([0-9]*\) frames out.*/\2/p')
      OUT_SAMPLES=$(printf '%s' "$LINE" | sed -n 's/.*frames out, \([0-9]*\) output samples.*/\1/p')
      WINDOWS=$(printf '%s' "$LINE" | sed -n 's/.*output samples, \([0-9]*\) windows.*/\1/p')
    fi
    LOADFAIL_AFTER=$(count_log "加载失败，回退 ONNX")
    FALLBACK_NAMES=$(grep "Lazy-loaded ONNX fallback" "$LOG" | sed -n "$((LAZY_BEFORE + 1)),\$p" \
      | sed 's/.*fallback: //' | sort -u | tr '\n' ',' | sed 's/,$//')
    REASON=""
    EXPECTED_OUT=""
    if [ -n "$FRAMES_IN" ]; then EXPECTED_OUT=$((FRAMES_IN * 960)); fi
    if [ "$STATE" != completed ]; then
      REASON=$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
e = d.get("error") or ""
print(e.get("message") if isinstance(e, dict) else e)
' "$SMOKE_DIR/$LABEL.job.json" 2>/dev/null | tr '\t' ' ')
      FAIL_N=$((FAIL_N + 1))
    else
      if [ "$FETCH_OK" -ne 1 ]; then FAIL_N=$((FAIL_N + 1)); REASON="fetch 失败"; fi
      if [ -z "$FRAMES_IN" ] || [ "$FRAMES_IN" != "$FRAMES_OUT" ]; then
        FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }frames_in!=frames_out ($FRAMES_IN/$FRAMES_OUT)"
      fi
      if [ -z "$OUT_SAMPLES" ] || [ "$OUT_SAMPLES" != "$EXPECTED_OUT" ]; then
        FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }out_samples $OUT_SAMPLES != $EXPECTED_OUT"
      fi
      if [ "$LOADFAIL_AFTER" -ne "$LOADFAIL_BEFORE" ]; then
        FAIL_N=$((FAIL_N + 1)); REASON="${REASON:+$REASON; }引擎加载回退 ONNX"
      fi
    fi
    python3 - "$LABEL" "$m" "$dur" "$STATE" "$WALL" "$SMOKE_DIR" "$REASON" \
      "$FRAMES_IN" "$FRAMES_OUT" "$OUT_SAMPLES" "$WINDOWS" "${VMHWM:-}" "$FALLBACK_NAMES" \
      <<'PY' >> "$SMOKE_DIR/summary.tsv"
import json, sys
label, m, dur, state, wall, root, reason, fi, fo, os_, win, vmhwm, fb = sys.argv[1:14]
st = json.load(open(f"{root}/{label}.status.json"))
rvc = st.get("rvc") or {}
print("\t".join(str(v) for v in [
    label, m, dur, state, fi, fo, os_, win,
    rvc.get("inference_ms_last"), wall, vmhwm, fb, reason,
]))
PY
    tail -1 "$SMOKE_DIR/summary.tsv"
  done
done

echo "=== summary ==="
cat "$SMOKE_DIR/summary.tsv"
if [ "$FAIL_N" -ne 0 ]; then
  echo "WINDOW-BC FAILED: $FAIL_N 项" >&2
  exit 1
fi
echo "WINDOW-BC PASSED"