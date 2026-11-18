#!/bin/bash
# Jetson 实时多模型板端实测（SPEC-03 多采样率 / SPEC-04 model use / SPEC-11 预设联动）。
# 正常由 tools/jetson-e2e.sh --realtime-mm 调用，也可在设备上直接运行。
# 前置：本仓库已 rsync 到设备并完成构建（tools/deploy-jetson.sh），BIN_DIR 内有
#   mozart-ctl/daemon/pre/rvc；snd-aloop 已加载；两个模型资产齐全且采样率不同。
# 模型文件经符号链接放入 SMOKE_DIR 下的临时模型目录：预设写临时目录，不污染资产树。
# 场景：48k 实时基线 / overlap 定值拒绝 / startup 可调 / 跨采样率 model use /
#   切回与预设联动 / 未知模型失败恢复 / 文件占 GPU 时 Busy / 40k 文件链 / 资产无残留。
set -u

BIN_DIR=${BIN_DIR:-$HOME/mozart-runtime/bin}
MODELS_DIR=${MODELS_DIR:-$HOME/mozart-assets/models/rvc}
MODEL_A=${MODEL_A:-qiqi-zh}
MODEL_B=${MODEL_B:-frieren-v2-40k}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-realtime-mm}
CAP_SUB=${CAP_SUB:-2}
PB_SUB=${PB_SUB:-3}
DURATION_S=${DURATION_S:-3}
JOB_TIMEOUT=${JOB_TIMEOUT:-420}
RESTORE_RUNTIME=${RESTORE_RUNTIME:-0}
SRC_A=$MODELS_DIR/$MODEL_A
SRC_B=$MODELS_DIR/$MODEL_B
FEATURE_DIR=${FEATURE_DIR:-$SRC_A}
RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/mm.toml
FILES=$SMOKE_DIR/files
WORK_MODELS=$SMOKE_DIR/models
WORK_A=$WORK_MODELS/$MODEL_A
WORK_B=$WORK_MODELS/$MODEL_B
LOG=$RUN/logs/rvc.log
RUNTIME_CFG=$HOME/mozart-runtime/mozart.toml

ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
jget() { python3 -c 'import json,sys;d=json.load(sys.stdin)
for k in sys.argv[1].split("."):
    d=d[k] if isinstance(d,dict) else d[int(k)]
print(d)' "$1"; }

cleanup() {
  if [ -S "$RUN/daemon.sock" ]; then ctl down >/dev/null 2>&1; fi
  pkill -f "mozart-daemon --config $CFG" 2>/dev/null
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

for tool in python3 ffmpeg ffprobe; do
  command -v "$tool" >/dev/null || { echo "MM FAILED: 缺少 $tool" >&2; exit 1; }
done
for bin in mozart-ctl mozart-daemon mozart-pre mozart-rvc; do
  [ -x "$BIN_DIR/$bin" ] || { echo "MM FAILED: 缺 $BIN_DIR/$bin" >&2; exit 1; }
done
for m in "$MODEL_A" "$MODEL_B"; do
  d=$MODELS_DIR/$m
  [ -d "$d" ] || { echo "MM FAILED: 缺模型目录 $d" >&2; exit 1; }
  [ -f "$d/config.json" ] || { echo "MM FAILED: 缺 $d/config.json" >&2; exit 1; }
  if [ ! -e "$d/$m-front.engine" ] && [ ! -e "$d/$m-front.onnx" ]; then
    echo "MM FAILED: $m 缺实时 front 资产，无法测实时" >&2; exit 1
  fi
done
for asset in hubert_base_dynamic.onnx rmvpe_dynamic.onnx hubert-realtime.onnx \
             rmvpe-realtime.onnx; do
  [ -e "$FEATURE_DIR/$asset" ] || { echo "MM FAILED: 缺特征引擎 $FEATURE_DIR/$asset" >&2; exit 1; }
done
[ -d /proc/asound/Loopback ] || { echo "MM FAILED: snd-aloop 未加载" >&2; exit 1; }

# 两模型采样率须不同，否则跨采样率场景无意义。
sr_of() { python3 -c "import json;print(json.load(open('$1/config.json'))['data']['sampling_rate'])" 2>/dev/null; }
SR_A=$(sr_of "$SRC_A"); SR_B=$(sr_of "$SRC_B")
[ -n "$SR_A" ] && [ -n "$SR_B" ] || { echo "MM FAILED: 读不到采样率（config.json data.sampling_rate）" >&2; exit 1; }
if [ "$SR_A" = "$SR_B" ]; then
  echo "MM FAILED: 两模型采样率相同（$SR_A），跨采样率场景无意义；换 MODEL_B" >&2; exit 1
fi
echo "模型采样率：$MODEL_A=$SR_A  $MODEL_B=$SR_B"

cleanup
rm -rf "$RUN" "$FILES" "$WORK_MODELS"
mkdir -p "$SMOKE_DIR" "$FILES" "$WORK_A" "$WORK_B"
find "$SRC_A" -mindepth 1 -maxdepth 1 ! -name presets -exec ln -s {} "$WORK_A/" \;
find "$SRC_B" -mindepth 1 -maxdepth 1 ! -name presets -exec ln -s {} "$WORK_B/" \;
ASSET_BEFORE_A=$(find "$SRC_A" -maxdepth 1 -name presets -print)
ASSET_BEFORE_B=$(find "$SRC_B" -maxdepth 1 -name presets -print)

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
model_id = "$MODEL_A"
f0_method = "rmvpe"
models_dir = "$WORK_MODELS"
device = "cuda"
half = false
cache_limit = 2
hubert_path = "$FEATURE_DIR/hubert_base_dynamic.onnx"
rmvpe_path = "$FEATURE_DIR/rmvpe_dynamic.onnx"
realtime_hubert_path = "$FEATURE_DIR/hubert-realtime.onnx"
realtime_rmvpe_path = "$FEATURE_DIR/rmvpe-realtime.onnx"

[rvc.realtime]
skip_silence = false

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
printf 'scenario\tstate\tdetail\n' > "$SMOKE_DIR/summary.tsv"

st() { ctl status --json > "$SMOKE_DIR/st.json" 2>/dev/null; }
rvc_field() { st; jget "rvc.$1" < "$SMOKE_DIR/st.json" 2>/dev/null || true; }
wait_streaming() {
  local deadline=$((SECONDS + ${1:-40}))
  while :; do
    st
    if python3 - "$SMOKE_DIR/st.json" <<'PY' 2>/dev/null
import json, sys
d = json.load(open(sys.argv[1]))
r = d.get("rvc") or {}
sys.exit(0 if r.get("state") == "streaming" and r.get("stream_kind") == "realtime" else 1)
PY
    then return 0; fi
    [ "$SECONDS" -ge "$deadline" ] && return 1
    sleep 1
  done
}
# 实时链健康判据：帧守恒、有块完成、无推理/传输错误。
assert_stream_healthy() {
  local tag=$1
  local fin fout blocks err
  fin=$(rvc_field frames_in); fout=$(rvc_field frames_out)
  blocks=$(rvc_field blocks); err=$(rvc_field last_error)
  assert_eq "$tag blocks>0" "$(if [ "${blocks:-0}" -gt 0 ]; then echo yes; else echo no; fi)" "yes"
  assert_eq "$tag 帧守恒" "$fin" "$fout"
  assert_eq "$tag last_error" "${err:-None}" "None"
}
loaded_sr() { grep -o "RVC model '$1' loaded (sr=[0-9]*" "$LOG" 2>/dev/null | tail -1 | grep -o '[0-9]*$'; }

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
submit_model() { # <wav> <model>：回 job_id
  ctl file submit "$1" --model "$2" --json 2>"$SMOKE_DIR/submit.err" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["job_id"])' 2>/dev/null
}
wait_job() {
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
ctl up || { echo "MM FAILED: ctl up 失败" >&2; exit 1; }
deadline=$((SECONDS + 120))
while :; do
  st
  if python3 - "$SMOKE_DIR/st.json" <<'PY' 2>/dev/null
import json, sys
d = json.load(open(sys.argv[1]))
sys.exit(0 if d.get("pre") and (d.get("rvc") or {}).get("state") else 1)
PY
  then break; fi
  [ "$SECONDS" -ge "$deadline" ] && { echo "MM FAILED: pre/rvc 未就绪" >&2; exit 1; }
  sleep 1
done

INPUT=$SMOKE_DIR/input.wav
gen_wav "$INPUT" "$DURATION_S"

scenario "48k 实时基线"
ctl route set rvc >/dev/null 2>&1 || bad "route set rvc 失败"
if wait_streaming 60; then ok "实时流已启动"; else bad "实时流未启动"; fi
assert_eq "已加载模型" "$(rvc_field model_id)" "$MODEL_A"
assert_eq "日志采样率" "$(loaded_sr "$MODEL_A")" "$SR_A"
sleep 3
assert_stream_healthy "48k"
end_scenario

scenario "overlap 锁为定值，偏离在入口被拒"
BEFORE=$(sha256sum "$CFG" | cut -d' ' -f1)
if ctl config set rvc.realtime.crossfade_ms 40 >/dev/null 2>"$SMOKE_DIR/overlap.err"; then
  bad "crossfade 40 应被拒（overlap 须为 60 ms）"
elif grep -q 'must equal 60' "$SMOKE_DIR/overlap.err"; then
  ok "拒绝原因说明须等于 60 ms"
else
  bad "拒绝原因不符：$(cat "$SMOKE_DIR/overlap.err")"
fi
assert_eq "拒绝后基线不变" "$(sha256sum "$CFG" | cut -d' ' -f1)" "$BEFORE"
if wait_streaming 20; then ok "拒绝不打断在跑的流"; else bad "流被误断"; fi
assert_stream_healthy "overlap 拒绝后"
end_scenario

scenario "startup_delay 仍可调"
ctl config set rvc.realtime.startup_delay_ms 200 >/dev/null 2>&1 \
  || bad "startup_delay 200 应被接受"
if wait_streaming 40; then ok "改 startup 后流重建"; else bad "改 startup 后流未恢复"; fi
assert_stream_healthy "startup 200"
ctl config set rvc.realtime.startup_delay_ms 400 >/dev/null 2>&1
wait_streaming 40 >/dev/null
end_scenario

scenario "SetParams 在线调参不断流"
START_BEFORE=$(grep -F "StartStream kind=realtime" "$LOG" 2>/dev/null | wc -l)
SETPARAMS_BEFORE=$(grep -F "SetParams model=$MODEL_A" "$LOG" 2>/dev/null | wc -l)
FIN_BEFORE=$(rvc_field frames_in)
BLOCKS_BEFORE=$(rvc_field blocks)

for cmd in \
  "config set rvc.pitch_shift 4" \
  "config set rvc.index_rate 0.25" \
  "config set rvc.realtime.skip_silence true" \
  "config set rvc.realtime.skip_silence false" \
  "config set rvc.pitch_shift 0" \
  "config set rvc.index_rate 0.0"; do
  ctl $cmd >/dev/null 2>"$SMOKE_DIR/setparams.err" \
    || bad "$cmd 失败：$(cat "$SMOKE_DIR/setparams.err")"
done

sleep 2
START_AFTER=$(grep -F "StartStream kind=realtime" "$LOG" 2>/dev/null | wc -l)
SETPARAMS_AFTER=$(grep -F "SetParams model=$MODEL_A" "$LOG" 2>/dev/null | wc -l)
FIN_AFTER=$(rvc_field frames_in)
BLOCKS_AFTER=$(rvc_field blocks)

assert_eq "未重起实时流 (StartStream 次数不变)" "$START_AFTER" "$START_BEFORE"
assert_eq "SetParams 日志增加" \
  "$(if [ "${SETPARAMS_AFTER:-0}" -gt "${SETPARAMS_BEFORE:-0}" ]; then echo yes; else echo no; fi)" "yes"
assert_eq "frames_in 单调递增 (未重建 worker)" \
  "$(if [ "${FIN_AFTER:-0}" -gt "${FIN_BEFORE:-0}" ]; then echo yes; else echo no; fi)" "yes"
assert_eq "blocks 单调递增 (skip_silence=false 恢复推理)" \
  "$(if [ "${BLOCKS_AFTER:-0}" -gt "${BLOCKS_BEFORE:-0}" ]; then echo yes; else echo no; fi)" "yes"
assert_stream_healthy "SetParams 热更"
end_scenario

scenario "model use 跨采样率 $SR_A → $SR_B"
ctl preset save >/dev/null 2>&1 || bad "切换前保存 A 预设失败"
A_PITCH=$(ctl preset list --json 2>/dev/null | jget preset_values.pitch_shift 2>/dev/null || true)
if ctl model use "$MODEL_B" --json > "$SMOKE_DIR/use-b.json" 2>"$SMOKE_DIR/use-b.err"; then
  ok "model use 成功"
else
  bad "model use 失败：$(cat "$SMOKE_DIR/use-b.err")"
fi
if wait_streaming 60; then ok "切换后实时流在跑"; else bad "切换后流未起"; fi
assert_eq "已加载模型" "$(rvc_field model_id)" "$MODEL_B"
assert_eq "日志采样率" "$(loaded_sr "$MODEL_B")" "$SR_B"
sleep 3
assert_stream_healthy "$SR_B 实时"
assert_eq "model_id 写回基线" "$(grep -c "model_id = \"$MODEL_B\"" "$CFG")" "1"
DIRTY=$(ctl preset list --json 2>/dev/null | jget dirty 2>/dev/null || true)
assert_eq "切换后预设不 dirty" "$DIRTY" "False"
end_scenario

scenario "model use 切回 $SR_A 并载入 A 预设"
if ctl model use "$MODEL_A" --json >/dev/null 2>"$SMOKE_DIR/use-a.err"; then
  ok "切回成功"
else
  bad "切回失败：$(cat "$SMOKE_DIR/use-a.err")"
fi
if wait_streaming 60; then ok "切回后流在跑"; else bad "切回后流未起"; fi
assert_eq "日志采样率" "$(loaded_sr "$MODEL_A")" "$SR_A"
LIVE_PITCH=$(ctl preset list --json 2>/dev/null | jget live_values.pitch_shift 2>/dev/null || true)
assert_eq "切回载入 A 预设 pitch" "$LIVE_PITCH" "$A_PITCH"
sleep 3
assert_stream_healthy "切回 $SR_A"
end_scenario

scenario "model use 未知模型：失败恢复旧模型旧参数"
LIVE_BEFORE=$(ctl preset list --json 2>/dev/null | jget live_values.pitch_shift 2>/dev/null || true)
CFG_BEFORE=$(sha256sum "$CFG" | cut -d' ' -f1)
if ctl model use no-such-model --json >/dev/null 2>"$SMOKE_DIR/use-bad.err"; then
  bad "未知模型应失败"
else
  ok "未知模型被拒：$(head -c 120 "$SMOKE_DIR/use-bad.err")"
fi
if wait_streaming 40; then ok "失败后旧流恢复"; else bad "失败后流未恢复"; fi
assert_eq "仍是旧模型" "$(rvc_field model_id)" "$MODEL_A"
assert_eq "live 参数不变" "$(ctl preset list --json 2>/dev/null | jget live_values.pitch_shift 2>/dev/null || true)" "$LIVE_BEFORE"
assert_eq "基线不变" "$(sha256sum "$CFG" | cut -d' ' -f1)" "$CFG_BEFORE"
assert_stream_healthy "失败恢复后"
end_scenario

scenario "文件占 GPU 时 model use 回 Busy"
ctl route set direct >/dev/null 2>&1 || bad "切回 direct 失败"
sleep 2
JOB=$(submit_model "$INPUT" "$MODEL_A")
if [ -n "$JOB" ]; then
  BUSY_SEEN=no
  for _ in $(seq 1 60); do
    S=$(ctl file status "$JOB" --json 2>/dev/null | jget state 2>/dev/null || true)
    [ "$S" = "processing" ] && break
    case "$S" in completed|failed|cancelled) break ;; esac
    sleep 0.2
  done
  if ctl model use "$MODEL_B" --json >/dev/null 2>"$SMOKE_DIR/use-busy.err"; then
    bad "文件占 GPU 时 model use 应回 Busy"
  elif grep -qi 'busy' "$SMOKE_DIR/use-busy.err"; then
    ok "回 Busy"
  else
    bad "错误码不是 Busy：$(cat "$SMOKE_DIR/use-busy.err")"
  fi
  assert_eq "文件任务仍完成" "$(wait_job "$JOB")" "completed"
else
  bad "submit 未返回 job_id：$(cat "$SMOKE_DIR/submit.err" 2>/dev/null)"
fi
end_scenario

scenario "$SR_B 文件链（收尾重采样到 48k）"
ctl route set direct >/dev/null 2>&1
sleep 1
JOB=$(submit_model "$INPUT" "$MODEL_B")
if [ -n "$JOB" ]; then
  assert_eq "job state" "$(wait_job "$JOB")" "completed"
  OUT=$FILES/$JOB.output.wav
  if [ -f "$OUT" ]; then
    # 输出是 IEEE Float WAV，python wave 模块只支持 PCM，用 ffprobe 读。
    OSR=$(ffprobe -v error -select_streams a:0 -show_entries stream=sample_rate \
      -of default=nw=1:nk=1 "$OUT" 2>/dev/null)
    assert_eq "输出采样率" "$OSR" "48000"
  else
    bad "未找到输出 $OUT"
  fi
else
  bad "submit 未返回 job_id：$(cat "$SMOKE_DIR/submit.err" 2>/dev/null)"
fi
end_scenario

scenario "资产目录无 presets 残留"
assert_eq "A 资产 presets 前后一致" \
  "$(find "$SRC_A" -maxdepth 1 -name presets -print)" "$ASSET_BEFORE_A"
assert_eq "B 资产 presets 前后一致" \
  "$(find "$SRC_B" -maxdepth 1 -name presets -print)" "$ASSET_BEFORE_B"
end_scenario

echo "=== summary ==="
cat "$SMOKE_DIR/summary.tsv"
if [ "$FAIL_N" -ne 0 ]; then
  echo "MM FAILED: $FAIL_N 个场景失败" >&2
  exit 1
fi
echo "MM PASSED ($PASS_N scenarios)"