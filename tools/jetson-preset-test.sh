#!/bin/bash
# Jetson 模型预设板端实测（SPEC-11）：真 daemon + pre + rvc，CLI 驱动。
# 正常由 tools/jetson-e2e.sh --preset 调用，也可在设备上直接运行。
# 前置：本仓库已 rsync 到设备并完成构建（tools/deploy-jetson.sh），BIN_DIR 内
#   有 mozart-ctl/daemon/pre/rvc；snd-aloop 已加载；MODELS_DIR/<MODEL_ID> 是模型。
# 模型文件经符号链接放入 SMOKE_DIR 下的临时模型目录：预设写临时目录，
#   不污染只读资产树。
# 场景：无模型 NotReady / 首次加载自动建 default / save-use-list 与 dirty /
#   preset save 不改配置 TOML / 实时载入 / 资产目录无 presets 残留。
set -u

BIN_DIR=${BIN_DIR:-$HOME/mozart-runtime/bin}
MODELS_DIR=${MODELS_DIR:-$HOME/mozart-assets/models/rvc}
MODEL_ID=${MODEL_ID:-qiqi-zh}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-preset-test}
CAP_SUB=${CAP_SUB:-2}
PB_SUB=${PB_SUB:-3}
DURATION_S=${DURATION_S:-3}
JOB_TIMEOUT=${JOB_TIMEOUT:-420}
RESTORE_RUNTIME=${RESTORE_RUNTIME:-0}
SRC_MODEL=$MODELS_DIR/$MODEL_ID
FEATURE_DIR=${FEATURE_DIR:-$SRC_MODEL}
RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/preset.toml
FILES=$SMOKE_DIR/files
WORK_MODELS=$SMOKE_DIR/models
WORK_MODEL=$WORK_MODELS/$MODEL_ID
PRESET=$WORK_MODEL/presets/default.toml
RUNTIME_CFG=$HOME/mozart-runtime/mozart.toml

ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
jget() { python3 -c 'import json,sys;d=json.load(sys.stdin)
for k in sys.argv[1].split("."):
    d=d[k] if isinstance(d,dict) else d[int(k)]
print(d)' "$1"; }

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

CURRENT=""
CURRENT_FAIL=0
PASS_N=0
FAIL_N=0
scenario() { CURRENT=$1; CURRENT_FAIL=0; printf '\n=== %s ===\n' "$1"; }
ok()  { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; CURRENT_FAIL=$((CURRENT_FAIL + 1)); }
assert_eq() { if [ "$2" = "$3" ]; then ok "$1=$2"; else bad "$1：实际 '$2'，期望 '$3'"; fi; }
end_scenario() {
  if [ "$CURRENT_FAIL" -eq 0 ]; then
    PASS_N=$((PASS_N + 1)); printf 'PASS\t%s\n' "$CURRENT" >> "$SMOKE_DIR/summary.tsv"
    echo "$CURRENT PASS"
  else
    FAIL_N=$((FAIL_N + 1)); printf 'FAIL\t%s\t%s 项\n' "$CURRENT" "$CURRENT_FAIL" >> "$SMOKE_DIR/summary.tsv"
    echo "$CURRENT FAIL（$CURRENT_FAIL 项）"
  fi
}

for tool in python3 ffmpeg; do
  command -v "$tool" >/dev/null || { echo "PRESET FAILED: 缺少 $tool" >&2; exit 1; }
done
for bin in mozart-ctl mozart-daemon mozart-pre mozart-rvc; do
  [ -x "$BIN_DIR/$bin" ] || { echo "PRESET FAILED: 缺 $BIN_DIR/$bin" >&2; exit 1; }
done
[ -d "$SRC_MODEL" ] || { echo "PRESET FAILED: 缺模型目录 $SRC_MODEL" >&2; exit 1; }
for asset in hubert_base_dynamic.onnx rmvpe_dynamic.onnx hubert-realtime.onnx \
             rmvpe-realtime.onnx; do
  [ -e "$FEATURE_DIR/$asset" ] || { echo "PRESET FAILED: 缺特征引擎 $FEATURE_DIR/$asset" >&2; exit 1; }
done
[ -d /proc/asound/Loopback ] || { echo "PRESET FAILED: snd-aloop 未加载" >&2; exit 1; }

cleanup
rm -rf "$RUN" "$FILES" "$WORK_MODELS"
mkdir -p "$SMOKE_DIR" "$FILES" "$WORK_MODEL"
find "$SRC_MODEL" -mindepth 1 -maxdepth 1 ! -name presets -exec ln -s {} "$WORK_MODEL/" \;
ASSET_PRESETS_BEFORE=$(find "$SRC_MODEL" -maxdepth 1 -name presets -print)

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
model_id = "$MODEL_ID"
f0_method = "rmvpe"
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
max_duration_seconds = 300
max_upload_bytes = 104857600
ffmpeg_path = "/usr/bin/ffmpeg"
ffmpeg_timeout_ms = 120000
EOF
cp "$CFG" "$SMOKE_DIR/config-used.toml"
printf 'model\tstate\tdetail\n' > "$SMOKE_DIR/summary.tsv"

wait_ready() {
  local deadline=$((SECONDS + $1))
  while :; do
    ctl status --json > "$SMOKE_DIR/st-poll.json" 2>/dev/null
    if python3 - "$SMOKE_DIR/st-poll.json" <<'PY' 2>/dev/null
import json, sys
d = json.load(open(sys.argv[1]))
sys.exit(0 if d.get("pre") and (d.get("rvc") or {}).get("state") else 1)
PY
    then return 0; fi
    [ "$SECONDS" -ge "$deadline" ] && return 1
    sleep 1
  done
}

wait_streaming() {
  local deadline=$((SECONDS + 30))
  while :; do
    ctl status --json > "$SMOKE_DIR/st-route.json" 2>/dev/null
    if python3 - "$SMOKE_DIR/st-route.json" <<'PY' 2>/dev/null
import json, sys
d = json.load(open(sys.argv[1]))
rvc = d.get("rvc") or {}
sys.exit(0 if rvc.get("state") == "streaming" and rvc.get("stream_kind") == "realtime" else 1)
PY
    then return 0; fi
    [ "$SECONDS" -ge "$deadline" ] && return 1
    sleep 1
  done
}

gen_wav() {
  python3 - "$1" "$2" <<'PY'
import sys, wave
path, dur = sys.argv[1], float(sys.argv[2])
sr = 16000
n = int(sr * dur)
with wave.open(path, "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr)
    w.writeframes(b"".join(
        (int(12000 * ((i % 400) / 400.0 * 2 - 1))).to_bytes(2, "little", signed=True)
        for i in range(n)))
PY
}

submit_job() { # <wav>：回 job_id；失败回空
  ctl file submit "$1" --model "$MODEL_ID" --json 2>"$SMOKE_DIR/submit.err" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["job_id"])' 2>/dev/null
}

wait_job() { # <job_id>：回终态
  local id=$1 state="" deadline=$((SECONDS + JOB_TIMEOUT))
  while :; do
    ctl file status "$id" --json > "$SMOKE_DIR/job.json" 2>/dev/null
    state=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("state",""))' \
      "$SMOKE_DIR/job.json" 2>/dev/null || true)
    case "$state" in completed|failed|cancelled) break ;; esac
    [ "$SECONDS" -ge "$deadline" ] && { state=timeout; break; }
    sleep 1
  done
  printf '%s' "$state"
}

echo "=== up ==="
ctl up || { echo "PRESET FAILED: ctl up 失败" >&2; exit 1; }
wait_ready 120 || { echo "PRESET FAILED: pre/rvc 未就绪" >&2; exit 1; }

scenario "无模型时 preset 命令 NotReady"
for cmd in save use list; do
  if ctl preset "$cmd" --json >/dev/null 2>"$SMOKE_DIR/$cmd.err"; then
    bad "preset $cmd 应失败"
  elif grep -q 'NotReady' "$SMOKE_DIR/$cmd.err"; then
    ok "preset $cmd 回 NotReady"
  else
    bad "preset $cmd 错误码不是 NotReady：$(cat "$SMOKE_DIR/$cmd.err")"
  fi
done
end_scenario

INPUT=$SMOKE_DIR/input.wav
gen_wav "$INPUT" "$DURATION_S"

scenario "首次加载自动建 default 并用于文件任务"
JOB=$(submit_job "$INPUT")
if [ -n "$JOB" ]; then
  STATE=$(wait_job "$JOB")
else
  bad "submit 未返回 job_id：$(cat "$SMOKE_DIR/submit.err" 2>/dev/null)"
  STATE=submit_failed
fi
assert_eq "job state" "$STATE" "completed"
if [ -f "$PRESET" ]; then
  ok "已创建 $PRESET"
  for key in 'version = 1' 'pitch_shift = 0' 'index_rate = 0.0' 'rms_mix_rate = 1.0' \
             'protect = 0.33' 'f0_method = "rmvpe"'; do
    grep -qF "$key" "$PRESET" && ok "预设含 $key" || bad "预设缺 $key"
  done
else
  bad "未创建 $PRESET"
fi
ctl preset list --json > "$SMOKE_DIR/list-first.json" 2>/dev/null
LIST_DIRTY=$(jget dirty < "$SMOKE_DIR/list-first.json" 2>/dev/null || true)
assert_eq "list dirty" "$LIST_DIRTY" "False"
grep -q 'pitch_shift = 0' "$CFG" && ok "载入写 TOML 基线" || bad "TOML 基线未写五个参数"
end_scenario

scenario "preset save/use/list 与 dirty"
ctl config set rvc.pitch_shift 3 >/dev/null 2>&1
LIST_DIRTY=$(ctl preset list --json 2>/dev/null | jget dirty 2>/dev/null || true)
assert_eq "改 live 后 dirty" "$LIST_DIRTY" "True"
CFG_BEFORE=$(sha256sum "$CFG" | cut -d' ' -f1)
ctl preset save --json > "$SMOKE_DIR/save.json" 2>/dev/null || bad "preset save 失败"
CFG_AFTER=$(sha256sum "$CFG" | cut -d' ' -f1)
assert_eq "save 后配置 TOML 不变" "$CFG_AFTER" "$CFG_BEFORE"
grep -q 'pitch_shift = 3' "$PRESET" && ok "预设写入 pitch 3" || bad "预设未写入 pitch 3"
LIST_DIRTY=$(ctl preset list --json 2>/dev/null | jget dirty 2>/dev/null || true)
assert_eq "save 后 dirty" "$LIST_DIRTY" "False"

ctl config set rvc.pitch_shift 5 >/dev/null 2>&1
ctl preset use --json > "$SMOKE_DIR/use.json" 2>/dev/null || bad "preset use 失败"
LIVE=$(ctl status --json 2>/dev/null | jget rvc_params.pitch_shift 2>/dev/null || true)
assert_eq "use 后 live pitch" "$LIVE" "3"
LIST_DIRTY=$(ctl preset list --json 2>/dev/null | jget dirty 2>/dev/null || true)
assert_eq "use 后 dirty" "$LIST_DIRTY" "False"
end_scenario

scenario "实时载入"
ctl route set rvc >/dev/null 2>&1 || bad "route set rvc 失败"
if wait_streaming; then ok "实时流已启动"; else bad "实时流未启动"; fi
ctl config set rvc.pitch_shift 5 >/dev/null 2>&1
ctl preset use --json > "$SMOKE_DIR/use-rt.json" 2>/dev/null || bad "实时 preset use 失败"
LIVE=$(ctl status --json 2>/dev/null | jget rvc_params.pitch_shift 2>/dev/null || true)
assert_eq "实时 use 后 live pitch" "$LIVE" "3"
if wait_streaming; then ok "实时流仍在跑"; else bad "实时流中断未恢复"; fi
ctl route set direct >/dev/null 2>&1
end_scenario

scenario "资产目录无 presets 残留"
ASSET_PRESETS_AFTER=$(find "$SRC_MODEL" -maxdepth 1 -name presets -print)
assert_eq "资产 presets 前后一致" "$ASSET_PRESETS_AFTER" "$ASSET_PRESETS_BEFORE"
end_scenario

echo "=== summary ==="
cat "$SMOKE_DIR/summary.tsv"
if [ "$FAIL_N" -ne 0 ]; then
  echo "PRESET FAILED: $FAIL_N 个场景失败" >&2
  exit 1
fi
echo "PRESET PASSED ($PASS_N scenarios)"
