#!/bin/bash
# Jetson 文件转换板端实测：daemon + 真 pre + 真 rvc，CLI 驱动文件链路。
# 前置：本仓库已 rsync 到设备并完成构建（tools/deploy-jetson.sh），BIN_DIR 内
#   有 mozart-ctl/daemon/pre/rvc；snd-aloop 已加载；MODELS_DIR 内有模型资产；
#   设备有 python3 + numpy、ffmpeg、ffprobe。
# 拓扑与 [rvc] 配置沿用 tools/jetson-smoke.sh（Loopback DEV/SUBDEV 交叉），
#   实时链路照常拉起，文件链路另加 [file] 段（受管目录落在 SMOKE_DIR 内）。
# 场景：model list / 短 WAV 转换取回 / 压缩文件解码转换 / 取消与临时清理 /
#   GPU 互斥与路由等待槽 / 重启续转 / 入口拒绝 / 时长上限。
# 证据留在 $SMOKE_DIR：配置、样本、各阶段 status 与逐秒 progress-*.jsonl、
#   取回 WAV、audio stats、measure.json、summary.tsv。
#   MODELS_DIR   模型资产目录（默认 $HOME/mozart-assets/models/rvc/qiqi-zh-run）
#   BIN_DIR      部署二进制目录（默认 $HOME/Mozart/target/debug）
#   SMOKE_DIR    产物目录（默认 /tmp/mozart-file-test）
#   MODEL_ID     转换模型（默认 qiqi-zh）
#   SAMPLE_M4A   真实压缩样本（默认 $HOME/mozart-samples/jieshe.m4a）
#   JOB_TIMEOUT  单任务等待上限秒（默认 600）
#   SHORT_S/MID_S/LONG_S/EXCERPT_S  样本时长秒（默认 3/30/60/20）
#   LONG_BUDGET_S  近时长上限的长文件秒数，>0 时追加内存兜底场景（默认 0＝跳过）
set -u

MODELS_DIR=${MODELS_DIR:-$HOME/mozart-assets/models/rvc/qiqi-zh-run}
BIN_DIR=${BIN_DIR:-$HOME/Mozart/target/debug}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-file-test}
CAP_SUB=${CAP_SUB:-2}
PB_SUB=${PB_SUB:-3}
MODEL_ID=${MODEL_ID:-qiqi-zh}
SAMPLE_M4A=${SAMPLE_M4A:-$HOME/mozart-samples/jieshe.m4a}
JOB_TIMEOUT=${JOB_TIMEOUT:-600}
SHORT_S=${SHORT_S:-3}
MID_S=${MID_S:-30}
LONG_S=${LONG_S:-60}
EXCERPT_S=${EXCERPT_S:-20}
LONG_BUDGET_S=${LONG_BUDGET_S:-0}
RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/file.toml
FILES=$SMOKE_DIR/files

ts() { printf "[%5ss]" "$SECONDS"; }
ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
# status/任务 JSON 取值与 smoke、measure 共用（jget.sh 有键位说明）。
. "$(cd "$(dirname "$0")" && pwd)/jget.sh"

# ---- 场景断言与计数：明细打屏，场景末汇总一行进 summary.tsv。----
CURRENT=""
CURRENT_FAIL=0
PASS_N=0
FAIL_N=0
scenario() { CURRENT=$1; CURRENT_FAIL=0; printf '\n=== %s ===\n' "$1"; }
ok()  { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; CURRENT_FAIL=$((CURRENT_FAIL + 1)); }
assert_eq() { # <标签> <实际> <期望>
  if [ "$2" = "$3" ]; then ok "$1=$2"; else bad "$1：实际 '$2'，期望 '$3'"; fi
}
assert_gt() { # <标签> <实际> <下限>
  if python3 -c 'import sys; sys.exit(0 if float(sys.argv[1]) > float(sys.argv[2]) else 1)' \
       "$2" "$3" 2>/dev/null; then ok "$1=$2（> $3）"; else bad "$1：'$2' 未大于 $3"; fi
}
end_scenario() { # <备注>
  if [ "$CURRENT_FAIL" -eq 0 ]; then
    PASS_N=$((PASS_N + 1)); printf 'PASS\t%s\t%s\n' "$CURRENT" "$1" >> "$SMOKE_DIR/summary.tsv"
    printf '%s PASS：%s\n' "$(ts)" "$CURRENT"
  else
    FAIL_N=$((FAIL_N + 1)); printf 'FAIL\t%s\t%s 项断言失败：%s\n' "$CURRENT" "$CURRENT_FAIL" "$1" >> "$SMOKE_DIR/summary.tsv"
    printf '%s FAIL：%s（%s 项断言失败）\n' "$(ts)" "$CURRENT" "$CURRENT_FAIL"
  fi
}

# ---- 清理：退出时优雅关停 + 强制收尾，证据保留。----
cleanup() {
  trap - EXIT INT TERM
  if [ -S "$RUN/daemon.sock" ]; then ctl down >/dev/null 2>&1; fi
  pkill -f mozart-daemon 2>/dev/null
  pkill -f "mozart-pre --socket" 2>/dev/null
  pkill -f "mozart-rvc --socket" 2>/dev/null
  sleep 0.3
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ---- 前置检查：缺资产/缺工具立即报错，不进入半截测试。----
for tool in python3 ffmpeg ffprobe; do
  command -v "$tool" >/dev/null || { echo "FILE TEST FAILED: 缺少 $tool" >&2; exit 1; }
done
python3 -c 'import numpy' 2>/dev/null || { echo "FILE TEST FAILED: python3 缺 numpy" >&2; exit 1; }
for bin in mozart-ctl mozart-daemon mozart-pre mozart-rvc; do
  [ -x "$BIN_DIR/$bin" ] || { echo "FILE TEST FAILED: $BIN_DIR/$bin 不存在或不可执行" >&2; exit 1; }
done
for asset in "$MODEL_ID/config.json" hubert_base_dynamic.onnx rmvpe_dynamic.onnx \
             hubert-realtime.onnx rmvpe-realtime.onnx "$MODEL_ID.onnx"; do
  [ -e "$MODELS_DIR/$asset" ] || { echo "FILE TEST FAILED: 缺模型资产 $MODELS_DIR/$asset" >&2; exit 1; }
done
[ -f "$SAMPLE_M4A" ] || { echo "FILE TEST FAILED: 缺压缩样本 $SAMPLE_M4A" >&2; exit 1; }
[ -d /proc/asound/Loopback ] || { echo "FILE TEST FAILED: snd-aloop 未加载（/proc/asound/Loopback 不存在）" >&2; exit 1; }

cleanup
mkdir -p "$SMOKE_DIR"
rm -rf "$RUN" "$FILES"
rm -f "$SMOKE_DIR"/*.json "$SMOKE_DIR"/*.jsonl "$SMOKE_DIR"/*.wav "$SMOKE_DIR"/*.m4a \
      "$SMOKE_DIR"/*.txt "$SMOKE_DIR"/*.tsv "$SMOKE_DIR"/*.toml "$SMOKE_DIR"/*.log

# ---- 样本：确定性合成 WAV（180 Hz 基频 + 谐波，3 Hz 音节节拍），压缩样本
#      取真实录音前 EXCERPT_S 秒（保持原采样率与声道，验 ffmpeg 解码）。----
gen_wav() { # <路径> <秒>
  python3 - "$1" "$2" <<'PY'
import sys, wave
import numpy as np
path, dur = sys.argv[1], float(sys.argv[2])
sr = 48000
t = np.arange(int(sr * dur)) / sr
env = 0.5 * (0.55 + 0.45 * np.sin(2 * np.pi * 3.0 * t))
v = env * (np.sin(2 * np.pi * 180 * t) + 0.5 * np.sin(2 * np.pi * 360 * t)
           + 0.25 * np.sin(2 * np.pi * 540 * t))
pcm = (np.clip(v * 0.5, -1.0, 1.0) * 32767).astype('<i2')
with wave.open(path, 'wb') as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr)
    w.writeframes(pcm.tobytes())
PY
}
gen_wav "$SMOKE_DIR/short.wav" "$SHORT_S"
gen_wav "$SMOKE_DIR/mid.wav" "$MID_S"
gen_wav "$SMOKE_DIR/long.wav" "$LONG_S"
printf 'not audio\n' > "$SMOKE_DIR/bad.txt"
ffmpeg -nostdin -y -v error -i "$SAMPLE_M4A" -t "$EXCERPT_S" -c:a aac -b:a 96k \
  "$SMOKE_DIR/excerpt.m4a" || { echo "FILE TEST FAILED: 截取压缩样本失败" >&2; exit 1; }

# ---- 音频客观统计：ffprobe 取容器格式，ffmpeg 解到 48 kHz 单声道 f32 后
#      用 numpy 算 RMS 与峰值。统计范围＝整段解码样本，非窗口抽样。----
audio_stats() { # <音频路径> <输出 json>
  python3 - "$1" "$2" <<'PY'
import json, subprocess, sys
import numpy as np
src, dst = sys.argv[1], sys.argv[2]
probe = json.loads(subprocess.run(
    ["ffprobe", "-v", "error", "-show_entries",
     "format=format_name,duration:stream=codec_name,sample_rate,channels",
     "-of", "json", src], capture_output=True, text=True, check=True).stdout)
raw = subprocess.run(
    ["ffmpeg", "-v", "error", "-i", src, "-f", "f32le", "-ac", "1", "-ar", "48000", "-"],
    capture_output=True, check=True).stdout
x = np.frombuffer(raw, dtype='<f4').astype(np.float64)
stream = probe["streams"][0]
out = {
    "file": src,
    "format": probe["format"]["format_name"],
    "codec": stream["codec_name"],
    "sample_rate": int(stream["sample_rate"]),
    "channels": int(stream["channels"]),
    "container_duration_s": float(probe["format"]["duration"]),
    "decoded_samples": int(x.size),
    "decoded_duration_s": x.size / 48000.0,
    "rms": float(np.sqrt(np.mean(x ** 2))) if x.size else 0.0,
    "peak": float(np.max(np.abs(x))) if x.size else 0.0,
}
with open(dst, "w") as f:
    json.dump(out, f, ensure_ascii=False, indent=2)
print(json.dumps(out, ensure_ascii=False))
PY
}

# ---- 逐秒采样：status + 任务进度 + daemon/rvc 的 /proc 内存，落 JSONL。
#      计时起点＝该任务第一次采样，粒度 1 s（阶段边界由 daemon 落盘进度给出）。----
sample_once() { # <标签> <job_id>
  ctl status --json > "$SMOKE_DIR/st-$1.json" 2>/dev/null
  ctl file status "$2" --json > "$SMOKE_DIR/job-$1.json" 2>/dev/null
  python3 - "$SMOKE_DIR/st-$1.json" "$SMOKE_DIR/job-$1.json" "$SMOKE_DIR/progress-$1.jsonl" <<'PY'
import json, sys, time
st_p, job_p, out_p = sys.argv[1:4]
def load(p):
    try:
        with open(p) as f: return json.load(f)
    except (OSError, ValueError):
        return {}
def mem(pid):
    if not pid: return {}
    out = {}
    try:
        with open("/proc/%s/status" % pid) as f:
            for line in f:
                k, _, v = line.partition(":")
                if k in ("VmHWM", "VmRSS"):
                    out[k] = int(v.split()[0]) * 1024
    except (OSError, ValueError):
        pass
    return out
st, job = load(st_p), load(job_p)
rvc = st.get("rvc") or {}
prog = job.get("progress") or {}
daemon = st.get("daemon") or {}
line = {
    "t": round(time.time(), 3),
    "state": job.get("state"), "stage": prog.get("stage"), "percent": prog.get("percent"),
    "blocks_done": prog.get("blocks_done"),
    "route_desired": (st.get("route") or {}).get("desired"),
    "route_active": (st.get("route") or {}).get("active"),
    "gpu_owner": ((st.get("arbiter") or {}).get("gpu") or {}).get("owner"),
    "rvc_state": rvc.get("state"), "stream_kind": rvc.get("stream_kind"),
    "blocks": rvc.get("blocks"), "frames_in": rvc.get("frames_in"),
    "frames_out": rvc.get("frames_out"), "frames_dropped": rvc.get("frames_dropped"),
    "inference_ms_last": rvc.get("inference_ms_last"),
    "inference_ms_avg": rvc.get("inference_ms_avg"),
    "inference_ms_p95": rvc.get("inference_ms_p95"),
    "daemon_pid": daemon.get("pid"), "daemon_mem": mem(daemon.get("pid")),
    "rvc_mem": mem(((st.get("processes") or {}).get("rvc") or {}).get("pid")),
    "hw_used_bytes": ((st.get("hw") or {}).get("memory") or {}).get("used_bytes"),
    "pre_file_mode": (st.get("pre") or {}).get("file_mode"),
    "degraded": st.get("degraded") or [],
}
with open(out_p, "a") as f:
    f.write(json.dumps(line, ensure_ascii=False) + "\n")
PY
}

# ---- 等任务进入指定状态之一；期间逐秒采样。终态失败（failed/cancelled
#      不在期望集合）立即返回 2，超时返回 1。状态打到 stdout 供调用方记录。----
wait_job() { # <job_id> <标签> <超时秒> <期望状态>...
  local id=$1 tag=$2 timeout=$3
  shift 3
  local want=" $* " deadline=$((SECONDS + timeout)) state=""
  while :; do
    sample_once "$tag" "$id"
    state=$(jget "$SMOKE_DIR/job-$tag.json" path state)
    case "$want" in
      *" $state "*)
        # 补一次采样：daemon 每秒轮询 rvc，终态瞬间的缓存可能早于 StopStream
        # 的最终计数，短任务会因此漏掉 inference_ms。等一拍再采一次。
        sleep 2
        sample_once "$tag" "$id"
        printf '%s' "$state"; return 0 ;;
    esac
    case "$state" in failed|cancelled) printf '%s' "$state"; return 2 ;; esac
    if [ "$SECONDS" -ge "$deadline" ]; then printf 'timeout:%s' "$state"; return 1; fi
    sleep 1
  done
}

wait_ready() { # <超时秒>：pre 有状态且 rvc 有状态（同 smoke 判据）
  local deadline=$((SECONDS + $1)) pre="" rvc=""
  while :; do
    ctl status --json > "$SMOKE_DIR/st-poll.json" 2>/dev/null
    pre=$(jget "$SMOKE_DIR/st-poll.json" pre)
    rvc=$(jget "$SMOKE_DIR/st-poll.json" rvcstate)
    if [ "$pre" = "yes" ] && [ -n "$rvc" ]; then
      cp "$SMOKE_DIR/st-poll.json" "$SMOKE_DIR/st-ready.json"
      printf 'pre=%s rvc.state=%s' "$pre" "$rvc"; return 0
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      printf 'pre=%s rvc.state=%s' "${pre:-none}" "${rvc:-none}"; return 1
    fi
    sleep 1
  done
}

wait_route() { # <期望 active> <超时秒>
  local deadline=$((SECONDS + $2)) active=""
  while :; do
    ctl status --json > "$SMOKE_DIR/st-route.json" 2>/dev/null
    active=$(jget "$SMOKE_DIR/st-route.json" path route.active)
    if [ "$active" = "$1" ]; then printf '%s' "$active"; return 0; fi
    if [ "$SECONDS" -ge "$deadline" ]; then printf 'timeout:%s' "$active"; return 1; fi
    sleep 1
  done
}

# 等控制面可应答（ListModels 成功）。取消后 daemon 按 SPEC-04 把 StopStream
# 超时记降级、立即落 cancelled 并释放 GPU，而 rvc 单线程控制循环要等整段推理
# 的下一个取消检查点才退出，这期间 ListModels/GetStatus 都不可达。后续场景
# 必须等恢复再提交，否则量到的是这段窗口而不是被测行为。
wait_ctl_ready() { # <超时秒>
  local deadline=$((SECONDS + $1))
  while :; do
    if ctl model list --json > "$SMOKE_DIR/st-ctl-ready.json" 2>/dev/null; then return 0; fi
    if [ "$SECONDS" -ge "$deadline" ]; then return 1; fi
    sleep 1
  done
}

submit() { # <音频路径> <标签>：成功回 job_id，失败回空并留 stderr
  ctl file submit "$1" --model "$MODEL_ID" --json > "$SMOKE_DIR/submit-$2.json" 2> "$SMOKE_DIR/submit-$2.err"
  if [ $? -ne 0 ]; then printf ''; return 1; fi
  jget "$SMOKE_DIR/submit-$2.json" path job_id
}

# 受管目录内该任务的临时/源文件清单（取消与收尾清理的判据）。
job_files() { ls -1 "$FILES" 2>/dev/null | grep "^$1" || true; }

# 队列快照里按 job_id 取状态：队列含本次测试的历史任务，不能按下标取。
queue_state() { python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
print(next((j["state"] for j in d.get("jobs", []) if j["job_id"] == sys.argv[2]), "missing"))
' "$1" "$2" 2>/dev/null; }

cat > "$CFG" <<EOF
[daemon]
run_dir = "$RUN"
rpc_timeout_ms = 2000
start_timeout_ms = 30000

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
models_dir = "$MODELS_DIR"
device = "cuda"
half = false
cache_limit = 2
hubert_path = "$MODELS_DIR/hubert_base_dynamic.onnx"
rmvpe_path = "$MODELS_DIR/rmvpe_dynamic.onnx"
realtime_hubert_path = "$MODELS_DIR/hubert-realtime.onnx"
realtime_rmvpe_path = "$MODELS_DIR/rmvpe-realtime.onnx"

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
{
  echo "date: $(date -Is)"
  echo "host: $(uname -srm)"
  echo "bin_dir: $BIN_DIR"
  echo "models_dir: $MODELS_DIR"
  echo "model_id: $MODEL_ID"
  echo "sample_m4a: $SAMPLE_M4A"
  echo "durations_s: short=$SHORT_S mid=$MID_S long=$LONG_S excerpt=$EXCERPT_S"
  echo "job_timeout_s: $JOB_TIMEOUT"
  echo "cap_sub: $CAP_SUB pb_sub: $PB_SUB"
  ffmpeg -version 2>/dev/null | head -1
  python3 -c 'import numpy; print("numpy:", numpy.__version__)'
} > "$SMOKE_DIR/env.txt"

echo "=== up ==="
ctl up || { echo "FILE TEST FAILED: ctl up 失败" >&2; exit 1; }
echo "=== wait pre/rvc ready ==="
READY=$(wait_ready 90) || { echo "FILE TEST FAILED: pre/rvc 未就绪（$READY）" >&2; exit 1; }
echo "ready: $READY"

# ---- a 模型清单 ----
scenario "a-model-list"
ctl model list --json > "$SMOKE_DIR/models.json" 2> "$SMOKE_DIR/models.err"
MODEL_EXISTS=$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
print(next((str(m["exists"]).lower() for m in d["models"] if m["id"] == sys.argv[2]), "missing"))
' "$SMOKE_DIR/models.json" "$MODEL_ID" 2>/dev/null)
assert_eq "$MODEL_ID exists" "$MODEL_EXISTS" "true"
ctl model list > "$SMOKE_DIR/models.txt" 2>&1
end_scenario "models.json"

# ---- b 短 WAV：submit → completed → fetch → 客观校验 ----
scenario "b-short-wav"
audio_stats "$SMOKE_DIR/short.wav" "$SMOKE_DIR/stats-in-b.json" > /dev/null
JOB_B=$(submit "$SMOKE_DIR/short.wav" b)
if [ -z "$JOB_B" ]; then
  bad "submit 失败：$(cat "$SMOKE_DIR/submit-b.err")"
else
  ok "job_id=$JOB_B"
  B_T0=$SECONDS
  STATE=$(wait_job "$JOB_B" b "$JOB_TIMEOUT" completed); RC=$?
  B_WALL=$((SECONDS - B_T0))
  assert_eq "终态" "$STATE" "completed"
  if [ "$RC" -eq 0 ]; then
    ctl file fetch "$JOB_B" -o "$SMOKE_DIR/out-b.wav" --json > "$SMOKE_DIR/fetch-b.json" 2>&1
    assert_eq "fetch 退出码" "$?" "0"
    audio_stats "$SMOKE_DIR/out-b.wav" "$SMOKE_DIR/stats-b.json" > /dev/null
    assert_eq "容器格式" "$(jget "$SMOKE_DIR/stats-b.json" path format)" "wav"
    assert_eq "采样率" "$(jget "$SMOKE_DIR/stats-b.json" path sample_rate)" "48000"
    assert_eq "声道" "$(jget "$SMOKE_DIR/stats-b.json" path channels)" "1"
    IN_D=$(jget "$SMOKE_DIR/stats-in-b.json" path decoded_duration_s)
    OUT_D=$(jget "$SMOKE_DIR/stats-b.json" path decoded_duration_s)
    if python3 -c 'import sys; sys.exit(0 if abs(float(sys.argv[1])-float(sys.argv[2]))<=0.1 else 1)' \
         "$IN_D" "$OUT_D"; then ok "时长一致：入 $IN_D s / 出 $OUT_D s"; else bad "时长偏差：入 $IN_D s / 出 $OUT_D s"; fi
    assert_gt "RMS" "$(jget "$SMOKE_DIR/stats-b.json" path rms)" "0.0001"
    assert_gt "峰值" "$(jget "$SMOKE_DIR/stats-b.json" path peak)" "0.0001"
    ok "submit→completed 墙钟 ${B_WALL} s（1 s 采样粒度）"
  fi
fi
end_scenario "out-b.wav stats-b.json progress-b.jsonl"

# ---- c 压缩文件（真实录音截取）：验 ffmpeg 解码非 WAV ----
scenario "c-compressed-m4a"
audio_stats "$SMOKE_DIR/excerpt.m4a" "$SMOKE_DIR/stats-in-c.json" > /dev/null
JOB_C=$(submit "$SMOKE_DIR/excerpt.m4a" c)
if [ -z "$JOB_C" ]; then
  bad "submit 失败：$(cat "$SMOKE_DIR/submit-c.err")"
else
  ok "job_id=$JOB_C"
  C_T0=$SECONDS
  STATE=$(wait_job "$JOB_C" c "$JOB_TIMEOUT" completed); RC=$?
  C_WALL=$((SECONDS - C_T0))
  assert_eq "终态" "$STATE" "completed"
  if [ "$RC" -eq 0 ]; then
    ctl file fetch "$JOB_C" -o "$SMOKE_DIR/out-c.wav" --json > "$SMOKE_DIR/fetch-c.json" 2>&1
    assert_eq "fetch 退出码" "$?" "0"
    audio_stats "$SMOKE_DIR/out-c.wav" "$SMOKE_DIR/stats-c.json" > /dev/null
    assert_eq "容器格式" "$(jget "$SMOKE_DIR/stats-c.json" path format)" "wav"
    assert_eq "采样率" "$(jget "$SMOKE_DIR/stats-c.json" path sample_rate)" "48000"
    assert_eq "声道" "$(jget "$SMOKE_DIR/stats-c.json" path channels)" "1"
    IN_D=$(jget "$SMOKE_DIR/stats-in-c.json" path container_duration_s)
    OUT_D=$(jget "$SMOKE_DIR/stats-c.json" path decoded_duration_s)
    if python3 -c 'import sys; sys.exit(0 if abs(float(sys.argv[1])-float(sys.argv[2]))<=0.1 else 1)' \
         "$IN_D" "$OUT_D"; then ok "时长一致：入 $IN_D s / 出 $OUT_D s"; else bad "时长偏差：入 $IN_D s / 出 $OUT_D s"; fi
    assert_gt "RMS" "$(jget "$SMOKE_DIR/stats-c.json" path rms)" "0.0001"
    ok "submit→completed 墙钟 ${C_WALL} s（1 s 采样粒度）"
  fi
fi
end_scenario "out-c.wav stats-c.json progress-c.jsonl"

# ---- e GPU 互斥 + 进度细化 + 内存峰值（中等时长任务，留出观察窗）----
scenario "e-gpu-mutex-progress"
JOB_E=$(submit "$SMOKE_DIR/mid.wav" e)
if [ -z "$JOB_E" ]; then
  bad "submit 失败：$(cat "$SMOKE_DIR/submit-e.err")"
else
  ok "job_id=$JOB_E"
  STATE=$(wait_job "$JOB_E" e "$JOB_TIMEOUT" processing); RC=$?
  assert_eq "进入 processing" "$STATE" "processing"
  if [ "$RC" -eq 0 ]; then
    ctl route set rvc > "$SMOKE_DIR/route-e.txt" 2>&1
    assert_eq "route set rvc 退出码" "$?" "0"
    ctl status --json > "$SMOKE_DIR/st-e-defer.json" 2>/dev/null
    assert_eq "desired" "$(jget "$SMOKE_DIR/st-e-defer.json" path route.desired)" "rvc"
    assert_eq "active（应挂等待槽）" "$(jget "$SMOKE_DIR/st-e-defer.json" path route.active)" "direct"
    assert_eq "GPU 归属" "$(jget "$SMOKE_DIR/st-e-defer.json" path arbiter.gpu.owner)" "file"
    E_T0=$SECONDS
    STATE=$(wait_job "$JOB_E" e "$JOB_TIMEOUT" completed); RC=$?
    E_WALL=$((SECONDS - E_T0))
    assert_eq "终态" "$STATE" "completed"
    if [ "$RC" -eq 0 ]; then
      ACTIVE=$(wait_route rvc 90)
      assert_eq "任务结束后 active" "$ACTIVE" "rvc"
      ctl status --json > "$SMOKE_DIR/st-e-applied.json" 2>/dev/null
      assert_eq "GPU 归属（已切实时）" "$(jget "$SMOKE_DIR/st-e-applied.json" path arbiter.gpu.owner)" "realtime"
      ctl file fetch "$JOB_E" -o "$SMOKE_DIR/out-e.wav" --json > "$SMOKE_DIR/fetch-e.json" 2>&1
      audio_stats "$SMOKE_DIR/out-e.wav" "$SMOKE_DIR/stats-e.json" > /dev/null
      assert_gt "RMS" "$(jget "$SMOKE_DIR/stats-e.json" path rms)" "0.0001"
      ok "processing→completed 墙钟 ${E_WALL} s（1 s 采样粒度）"
      ctl route set direct > "$SMOKE_DIR/route-e-restore.txt" 2>&1
      ACTIVE=$(wait_route direct 90)
      assert_eq "恢复 direct" "$ACTIVE" "direct"
    fi
  fi
fi
end_scenario "st-e-defer.json st-e-applied.json progress-e.jsonl out-e.wav"

# ---- d 取消：长任务执行中取消，验终态与临时/源副本清理 ----
scenario "d-cancel"
JOB_D=$(submit "$SMOKE_DIR/long.wav" d)
if [ -z "$JOB_D" ]; then
  bad "submit 失败：$(cat "$SMOKE_DIR/submit-d.err")"
else
  ok "job_id=$JOB_D"
  STATE=$(wait_job "$JOB_D" d "$JOB_TIMEOUT" processing); RC=$?
  assert_eq "进入 processing" "$STATE" "processing"
  if [ "$RC" -eq 0 ]; then
    cp "$SMOKE_DIR/job-d.json" "$SMOKE_DIR/job-d-before-cancel.json"
    ok "取消前阶段=$(jget "$SMOKE_DIR/job-d.json" path progress.stage) percent=$(jget "$SMOKE_DIR/job-d.json" path progress.percent)"
    D_T0=$SECONDS
    ctl file cancel "$JOB_D" --json > "$SMOKE_DIR/cancel-d.json" 2>&1
    assert_eq "cancel 退出码" "$?" "0"
    STATE=$(wait_job "$JOB_D" d-cancel 180 cancelled); RC=$?
    D_WALL=$((SECONDS - D_T0))
    assert_eq "终态" "$STATE" "cancelled"
    ok "cancel→cancelled 墙钟 ${D_WALL} s（1 s 采样粒度）"
    LEFT=$(job_files "$JOB_D")
    if [ -z "$LEFT" ]; then ok "受管目录无该任务残留（源副本/decoded/output/.part 全清）"
    else bad "受管目录残留：$LEFT"; fi
    ctl status --json > "$SMOKE_DIR/st-d-after.json" 2>/dev/null
    assert_eq "GPU 已释放" "$(jget "$SMOKE_DIR/st-d-after.json" path arbiter.gpu.owner)" ""
    ls -1 "$FILES" > "$SMOKE_DIR/storage-after-cancel.txt" 2>&1
    # 取消后控制面恢复窗口：rvc 停在整段推理检查点才退出，期间命令不可达。
    D_REC_T0=$SECONDS
    if wait_ctl_ready 180; then
      ok "控制面恢复耗时 $((SECONDS - D_REC_T0)) s（1 s 粒度，cancel 后起算）"
    else
      bad "控制面 180 s 未恢复"
    fi
    ctl status --json > "$SMOKE_DIR/st-d-recovered.json" 2>/dev/null
  fi
fi
end_scenario "cancel-d.json job-d-before-cancel.json storage-after-cancel.txt"

# ---- f 重启续转：执行中 down/up，验落盘任务从头重跑并完成 ----
scenario "f-restart-resume"
JOB_F=$(submit "$SMOKE_DIR/mid.wav" f)
if [ -z "$JOB_F" ]; then
  bad "submit 失败：$(cat "$SMOKE_DIR/submit-f.err")"
else
  ok "job_id=$JOB_F"
  STATE=$(wait_job "$JOB_F" f "$JOB_TIMEOUT" processing); RC=$?
  assert_eq "进入 processing" "$STATE" "processing"
  if [ "$RC" -eq 0 ]; then
    cp "$FILES/queue.json" "$SMOKE_DIR/queue-f-before.json"
    cp "$SMOKE_DIR/job-f.json" "$SMOKE_DIR/job-f-before.json"
    F_T0=$SECONDS
    ctl down > "$SMOKE_DIR/down-f.txt" 2>&1
    assert_eq "down 退出码" "$?" "0"
    # down 应答先于进程退出：Shutdown 立即回 ok，随后才 join 监管线程/poller/
    # 文件 worker 并删 socket（daemon/src/server.rs）。故按有界等待判退出，
    # 并量这段退出耗时；瞬时 pgrep 会把设计内的收尾窗口误判成残留。
    F_DOWN_T0=$(date +%s%N)
    F_EXITED=0
    for _ in $(seq 1 150); do
      if ! pgrep -f mozart-daemon >/dev/null; then F_EXITED=1; break; fi
      sleep 0.2
    done
    F_EXIT_MS=$(( ($(date +%s%N) - F_DOWN_T0) / 1000000 ))
    if [ "$F_EXITED" = 1 ]; then ok "daemon 退出耗时 ${F_EXIT_MS} ms（down 应答后起算，0.2 s 轮询）"
    else bad "down 后 30 s 仍有 mozart-daemon 进程"; fi
    cp "$FILES/queue.json" "$SMOKE_DIR/queue-f-down.json"
    assert_eq "落盘状态（关停不改终态）" "$(queue_state "$SMOKE_DIR/queue-f-down.json" "$JOB_F")" "processing"
    ctl up > "$SMOKE_DIR/up-f.txt" 2>&1
    assert_eq "up 退出码" "$?" "0"
    # 启动即读回快照：queue load 把 processing 复位为 queued 并落盘，
    # 随后仲裁才重新启动，故此处能直接看到续转复位。
    cp "$FILES/queue.json" "$SMOKE_DIR/queue-f-up.json"
    ok "重启瞬间落盘状态=$(queue_state "$SMOKE_DIR/queue-f-up.json" "$JOB_F")"
    READY=$(wait_ready 90)
    if [ $? -ne 0 ]; then bad "重启后 pre/rvc 未就绪（$READY）"; else ok "重启后就绪：$READY"; fi
    if wait_ctl_ready 90; then ok "重启后控制面可应答"; else bad "重启后控制面 90 s 不可应答"; fi
    ctl file status "$JOB_F" --json > "$SMOKE_DIR/job-f-reloaded.json" 2>&1
    RELOADED=$(jget "$SMOKE_DIR/job-f-reloaded.json" path state)
    case "$RELOADED" in
      queued|processing) ok "重启读回状态=$RELOADED（processing 复位为 queued 续转）" ;;
      *) bad "重启读回状态=$RELOADED，期望 queued/processing" ;;
    esac
    STATE=$(wait_job "$JOB_F" f-restart "$JOB_TIMEOUT" completed); RC=$?
    F_WALL=$((SECONDS - F_T0))
    assert_eq "终态" "$STATE" "completed"
    if [ "$RC" -eq 0 ]; then
      ctl file fetch "$JOB_F" -o "$SMOKE_DIR/out-f.wav" --json > "$SMOKE_DIR/fetch-f.json" 2>&1
      audio_stats "$SMOKE_DIR/out-f.wav" "$SMOKE_DIR/stats-f.json" > /dev/null
      assert_eq "采样率" "$(jget "$SMOKE_DIR/stats-f.json" path sample_rate)" "48000"
      assert_gt "RMS" "$(jget "$SMOKE_DIR/stats-f.json" path rms)" "0.0001"
      ok "down→up→completed 墙钟 ${F_WALL} s（含重启与就绪等待）"
    fi
  fi
fi
end_scenario "queue-f-before.json queue-f-down.json job-f-reloaded.json out-f.wav"

# ---- g 入口拒绝：模型不存在、扩展名不在白名单 ----
scenario "g-reject-paths"
ctl file submit "$SMOKE_DIR/short.wav" --model no-such-model --json \
  > "$SMOKE_DIR/reject-model.json" 2> "$SMOKE_DIR/reject-model.err"
RC=$?
assert_eq "不存在模型退出码（4＝被拒）" "$RC" "4"
if grep -qi "model not found" "$SMOKE_DIR/reject-model.err"; then ok "拒绝原因含 model not found"
else bad "拒绝原因缺失：$(cat "$SMOKE_DIR/reject-model.err")"; fi
ctl file submit "$SMOKE_DIR/bad.txt" --model "$MODEL_ID" --json \
  > "$SMOKE_DIR/reject-ext.json" 2> "$SMOKE_DIR/reject-ext.err"
RC=$?
assert_eq "坏扩展名退出码（4＝被拒）" "$RC" "4"
if grep -qi "unsupported file extension" "$SMOKE_DIR/reject-ext.err"; then ok "拒绝原因含 unsupported file extension"
else bad "拒绝原因缺失：$(cat "$SMOKE_DIR/reject-ext.err")"; fi
LEFT=$(ls -1 "$FILES" | grep -c "input.txt" || true)
assert_eq "坏扩展名未留副本" "$LEFT" "0"
end_scenario "reject-model.err reject-ext.err"

# ---- h 时长上限：真实整段录音超 300 s，解码后拒绝 ----
scenario "h-duration-cap"
FULL_D=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$SAMPLE_M4A")
ok "整段样本时长 $FULL_D s（上限 300 s）"
JOB_H=$(submit "$SAMPLE_M4A" h)
if [ -z "$JOB_H" ]; then
  bad "submit 失败：$(cat "$SMOKE_DIR/submit-h.err")"
else
  STATE=$(wait_job "$JOB_H" h "$JOB_TIMEOUT" failed); RC=$?
  assert_eq "终态" "$STATE" "failed"
  ERR=$(jget "$SMOKE_DIR/job-h.json" path error)
  if printf '%s' "$ERR" | grep -qi "duration limit"; then ok "失败原因含 duration limit：$ERR"
  else bad "失败原因不符：$ERR"; fi
  ctl status --json > "$SMOKE_DIR/st-h-after.json" 2>/dev/null
  assert_eq "GPU 已释放" "$(jget "$SMOKE_DIR/st-h-after.json" path arbiter.gpu.owner)" ""
fi
end_scenario "job-h.json progress-h.jsonl"

# ---- i 近时长上限长文件（可选）：300 s 兜底的内存与整段耗时证据 ----
if [ "$LONG_BUDGET_S" -gt 0 ]; then
  scenario "i-long-budget"
  gen_wav "$SMOKE_DIR/budget.wav" "$LONG_BUDGET_S"
  audio_stats "$SMOKE_DIR/budget.wav" "$SMOKE_DIR/stats-in-i.json" > /dev/null
  ok "样本时长 $(jget "$SMOKE_DIR/stats-in-i.json" path decoded_duration_s) s（上限 300 s）"
  JOB_I=$(submit "$SMOKE_DIR/budget.wav" i)
  if [ -z "$JOB_I" ]; then
    bad "submit 失败：$(cat "$SMOKE_DIR/submit-i.err")"
  else
    ok "job_id=$JOB_I"
    I_T0=$SECONDS
    STATE=$(wait_job "$JOB_I" i "$JOB_TIMEOUT" completed); RC=$?
    I_WALL=$((SECONDS - I_T0))
    assert_eq "终态" "$STATE" "completed"
    if [ "$RC" -eq 0 ]; then
      ctl file fetch "$JOB_I" -o "$SMOKE_DIR/out-i.wav" --json > "$SMOKE_DIR/fetch-i.json" 2>&1
      audio_stats "$SMOKE_DIR/out-i.wav" "$SMOKE_DIR/stats-i.json" > /dev/null
      assert_eq "采样率" "$(jget "$SMOKE_DIR/stats-i.json" path sample_rate)" "48000"
      assert_eq "声道" "$(jget "$SMOKE_DIR/stats-i.json" path channels)" "1"
      IN_D=$(jget "$SMOKE_DIR/stats-in-i.json" path decoded_duration_s)
      OUT_D=$(jget "$SMOKE_DIR/stats-i.json" path decoded_duration_s)
      if python3 -c 'import sys; sys.exit(0 if abs(float(sys.argv[1])-float(sys.argv[2]))<=0.1 else 1)' \
           "$IN_D" "$OUT_D"; then ok "时长一致：入 $IN_D s / 出 $OUT_D s"; else bad "时长偏差：入 $IN_D s / 出 $OUT_D s"; fi
      assert_gt "RMS" "$(jget "$SMOKE_DIR/stats-i.json" path rms)" "0.0001"
      ok "submit→completed 墙钟 ${I_WALL} s（1 s 采样粒度）"
      ctl status --json > "$SMOKE_DIR/st-i-after.json" 2>/dev/null
      assert_eq "GPU 已释放" "$(jget "$SMOKE_DIR/st-i-after.json" path arbiter.gpu.owner)" ""
      # pre 接管证据（SPEC-03 状态门控交接）：处理期间 pre.file_mode=true
      # （rvc AI 发送暂停、输出 socket 释放），收尾后恢复 false。
      PAUSED=$(python3 -c '
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
print("yes" if any(r.get("pre_file_mode") is True for r in rows) else "no")
' "$SMOKE_DIR/progress-i.jsonl")
      assert_eq "处理期间 pre 已暂停（file_mode）" "$PAUSED" "yes"
      assert_eq "收尾后 pre 已恢复" "$(jget "$SMOKE_DIR/st-i-after.json" path pre.file_mode)" "false"
    fi
  fi
  end_scenario "out-i.wav stats-i.json progress-i.jsonl"
fi

# ---- 汇总测量：从逐秒 JSONL 与 stats 提取阶段时长、blocks、推理耗时、内存峰值 ----
python3 - "$SMOKE_DIR" <<'PY'
import glob, json, os, sys
d = sys.argv[1]
out = {}
for path in sorted(glob.glob(os.path.join(d, "progress-*.jsonl"))):
    tag = os.path.basename(path)[len("progress-"):-len(".jsonl")]
    rows = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                try: rows.append(json.loads(line))
                except ValueError: pass
    if not rows: continue
    stages, first_t = {}, rows[0]["t"]
    for r in rows:
        s = r.get("stage")
        if s and s not in stages: stages[s] = r["t"] - first_t
    def peak(key, sub):
        vals = [(r.get(key) or {}).get(sub) for r in rows]
        vals = [v for v in vals if isinstance(v, (int, float))]
        return max(vals) if vals else None
    def maxnum(key):
        vals = [r.get(key) for r in rows]
        vals = [v for v in vals if isinstance(v, (int, float))]
        return max(vals) if vals else None
    # rvc 派生指标只取属于本任务的采样：GetStatus 在无流时仍报上一条流的
    # 最终计数，混入会把上一任务的 blocks/推理耗时算到本任务头上（实测：
    # decode 阶段就失败的任务全程 stream_kind=null，却带着上一任务的值）。
    # 取 stream_kind=file（流在跑）或 stage 已到 infer/publish（推理已起，
    # 含 StopStream 后 stream_kind 清空、最终计数才带 inference_ms 的那一拍）。
    file_rows = [r for r in rows
                 if r.get("stream_kind") == "file" or r.get("stage") in ("infer", "publish")]
    def maxnum_file(key):
        vals = [r.get(key) for r in file_rows]
        vals = [v for v in vals if isinstance(v, (int, float))]
        return max(vals) if vals else None
    infer_start = stages.get("infer")
    publish_start = stages.get("publish")
    out[tag] = {
        "samples": len(rows),
        "file_stream_samples": len(file_rows),
        "window_s": round(rows[-1]["t"] - first_t, 3),
        "stage_offset_s": {k: round(v, 3) for k, v in stages.items()},
        "infer_wall_s": round(publish_start - infer_start, 3)
                        if infer_start is not None and publish_start is not None else None,
        "blocks_max": maxnum_file("blocks"),
        "blocks_done_max": maxnum_file("blocks_done"),
        "inference_ms_max": maxnum_file("inference_ms_last"),
        "frames_in_max": maxnum_file("frames_in"),
        "frames_out_max": maxnum_file("frames_out"),
        "frames_dropped_max": maxnum_file("frames_dropped"),
        "daemon_vmhwm_bytes": peak("daemon_mem", "VmHWM"),
        "daemon_vmrss_peak_bytes": peak("daemon_mem", "VmRSS"),
        "rvc_vmhwm_bytes": peak("rvc_mem", "VmHWM"),
        "rvc_vmrss_peak_bytes": peak("rvc_mem", "VmRSS"),
        "hw_used_peak_bytes": maxnum("hw_used_bytes"),
        "degraded": sorted({x for r in rows for x in (r.get("degraded") or [])}),
        "final_state": rows[-1].get("state"),
    }
for path in sorted(glob.glob(os.path.join(d, "stats-*.json"))):
    tag = os.path.basename(path)[len("stats-"):-len(".json")]
    with open(path) as f: out.setdefault("audio", {})[tag] = json.load(f)
with open(os.path.join(d, "measure.json"), "w") as f:
    json.dump(out, f, ensure_ascii=False, indent=2)
for tag, m in out.items():
    if tag == "audio": continue
    print("%-10s state=%-9s infer_wall=%s s blocks=%s inference_ms=%s daemon_VmHWM=%s MiB rvc_VmHWM=%s MiB"
          % (tag, m["final_state"], m["infer_wall_s"], m["blocks_max"], m["inference_ms_max"],
             round(m["daemon_vmhwm_bytes"] / 1048576, 1) if m["daemon_vmhwm_bytes"] else None,
             round(m["rvc_vmhwm_bytes"] / 1048576, 1) if m["rvc_vmhwm_bytes"] else None))
PY

echo
echo "=== down ==="
ctl down > "$SMOKE_DIR/down-final.txt" 2>&1
scenario "z-residue"
if pgrep -f "mozart-(daemon|pre|rvc)" > /dev/null; then
  pgrep -af "mozart-(daemon|pre|rvc)"
  bad "残留 mozart 进程"
else
  ok "无 mozart 进程残留"
fi
for f in daemon.sock pre.sock rvc.sock rvc-in.sock rvc-out.sock; do
  if [ -S "$RUN/$f" ]; then
    LIVE=$(python3 - "$RUN/$f" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
try:
    s.connect(sys.argv[1]); print("live")
except OSError:
    print("dead")
PY
)
    echo "$RUN/$f: $LIVE"
    if [ "$LIVE" = "live" ]; then bad "$f 仍接受连接"; else ok "$f 已失效"; fi
  else
    ok "$f 不存在"
  fi
done
end_scenario "进程与控制 socket 残留"
echo "=== rvc.log tail ==="
tail -20 "$RUN/logs/rvc.log" 2>/dev/null
echo "=== daemon.log tail ==="
tail -20 "$RUN/logs/daemon.log" 2>/dev/null

echo
echo "=== FILE TEST SUMMARY ==="
cat "$SMOKE_DIR/summary.tsv"
printf 'PASS=%d FAIL=%d\n' "$PASS_N" "$FAIL_N"
echo "证据目录：$SMOKE_DIR"
if [ "$FAIL_N" -ne 0 ]; then
  echo "FILE TEST FAILED" >&2
  exit 1
fi
echo "FILE TEST PASSED"
exit 0