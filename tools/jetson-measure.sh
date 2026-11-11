#!/bin/bash
# Jetson 端到端测量运行器：配合 tools/analyze_measure.py 使用（Task 7 实测）。
# 前置与 tools/jetson-smoke.sh 相同：仓库已 rsync 到设备并完成构建，
#   mozart-daemon/mozart-pre/mozart-ctl 在 $BIN_DIR（cargo release），
#   mozart-rvc 已复制到 $BIN_DIR。
# 场景（evidence 留在 $M_ROOT/<scenario>/，供 analyze_measure.py 汇总）：
#   floor <writer_period> <reader_period> <sub>  回录地板校准：aplay→loopback→arecord，
#       无 pre。参数为 aplay/arecord 周期帧数与 SUBDEV 号。同参数跑两次看抖动。
#   latency  direct/rvc 两路脉冲串回录延迟 + 路由切换时间线。
#   fade     连续音下 direct↔rvc 往返切换，淡变无爆音证据。
#   longrun <seconds>  rvc 稳态长跑：计数稳定性 + GPU 负载 + 推理耗时。
#   voice    espeak-ng 中文真语音 direct/rvc 两段回录。
#   heal <pre|rvc>     kill -9 自愈：pre 与 rvc 各一次，status 高频轮询 + 回录。
#       heal pre 在 pre 重启后重启测试信号源：snd-aloop 的采集端随 pre 死亡断缆，
#       旧信号缆位失同步（重开的 pre 只能采到静默），受控信号下才能验证恢复链。
# 环境变量：
#   MODELS_DIR  模型资产目录（默认 $HOME/Mozart-Old/rvc-golden/qiqi-zh-run）
#   BIN_DIR     部署二进制目录（默认 $HOME/Mozart/target/release）
#   M_ROOT      产物根目录（默认 $HOME/mozart-t7）
#   CAP_SUB/PB_SUB  loopback 子设备号（默认 2/3，避开被占用的 0/1）
set -u

MODELS_DIR=${MODELS_DIR:-$HOME/Mozart-Old/rvc-golden/qiqi-zh-run}
BIN_DIR=${BIN_DIR:-$HOME/Mozart/target/release}
M_ROOT=${M_ROOT:-$HOME/mozart-t7}
CAP_SUB=${CAP_SUB:-2}
PB_SUB=${PB_SUB:-3}
SR=48000
AREC_ARGS_COMMON=(-f S16_LE -c 2 -r "$SR" --buffer-size=4096 --period-size=1024 -t raw)
APLAY_ARGS_COMMON=(-f S16_LE -c 2 -r "$SR" --buffer-size=4096 --period-size=1024)

now_ms() { date +%s%3N; }
die() { echo "measure: $*" >&2; exit 1; }
ctl() { "$BIN_DIR/mozart-ctl" --config "$M_DIR/smoke.toml" "$@"; }
jget() { python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
key = sys.argv[2]
if key == "pre":
    print("yes" if d.get("pre") else "no")
elif key == "rvcstate":
    print((d.get("rvc") or {}).get("state") or "")
elif key == "rvcproc":
    print(d["processes"]["rvc"][sys.argv[3]])
elif key == "preproc":
    print(d["processes"]["pre"][sys.argv[3]])
elif key == "preroute":
    print((d.get("pre") or {}).get("route") or "")
' "$1" "$2" "${3:-}" 2>/dev/null; }

kill_mozart() {
  pkill -f mozart-daemon 2>/dev/null
  pkill -f "mozart-pre --socket" 2>/dev/null
  pkill -f "mozart-rvc --socket" 2>/dev/null
  sleep 0.3
}

# 生成部署 TOML（键值与 tools/jetson-smoke.sh 一致；run_dir 指向本场景目录）。
gen_config() {
  cat > "$M_DIR/smoke.toml" <<EOF
[daemon]
run_dir = "$M_DIR/run"
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
model_id = "qiqi-zh"
f0_method = "rmvpe"
models_dir = "$MODELS_DIR"
device = "cuda"
half = false
cache_limit = 2
hubert_path = "$MODELS_DIR/hubert_base_dynamic.onnx"
rmvpe_path = "$MODELS_DIR/rmvpe_dynamic.onnx"
realtime_hubert_path = "$MODELS_DIR/hubert-realtime.onnx"
realtime_rmvpe_path = "$MODELS_DIR/rmvpe-realtime.onnx"
EOF
}

daemon_up() {
  gen_config
  ctl up >/dev/null 2>&1 || die "ctl up failed"
  for _ in $(seq 1 60); do
    ctl status --json > "$M_DIR/st-poll.json" 2>/dev/null || { sleep 1; continue; }
    [ "$(jget "$M_DIR/st-poll.json" pre)" = "yes" ] \
      && [ -n "$(jget "$M_DIR/st-poll.json" rvcstate)" ] && break
    sleep 1
  done
  cp "$M_DIR/st-poll.json" "$M_DIR/st-ready.json"
  echo "ready: pre=$(jget "$M_DIR/st-ready.json" pre) rvc.state=$(jget "$M_DIR/st-ready.json" rvcstate)"
}

daemon_down() {
  ctl down >/dev/null 2>&1
  sleep 0.3
  if pgrep -f "mozart-(daemon|pre|rvc)" > /dev/null; then
    echo "RESIDUE PROCESSES FOUND" >&2
    pgrep -af "mozart-(daemon|pre|rvc)" >&2
    return 1
  fi
}

# 以 epoch 毫秒为绝对时刻调度（供路由切换与 kill 对齐回录时间线）。
sleep_until() {
  local target=$1 now
  now=$(now_ms)
  [ "$now" -lt "$target" ] && sleep "$(python3 -c "print(($target - $now) / 1000)")"
}

# 后台轮询 status 到 JSONL；参数：输出文件、轮询间隔秒、退出文件（存在即停）。
poll_status() {
  local out=$1 interval=$2 stopflag=$3
  : > "$out"
  while [ ! -e "$stopflag" ]; do
    local line
    line="{\"epoch_ms\":$(now_ms)}"
    if s=$(ctl status --json 2>/dev/null); then
      line="$line,\"status\":$s"
    else
      line="$line,\"status\":null"
    fi
    printf '%s\n' "$line" >> "$out"
    sleep "$interval"
  done
}

SCENARIO=${1:-}
[ -n "$SCENARIO" ] || die "usage: jetson-measure.sh <floor|latency|fade|longrun|voice|heal> [args]"
M_DIR="$M_ROOT/$SCENARIO-$(now_ms)"
mkdir -p "$M_DIR"
echo "evidence dir: $M_DIR"

case "$SCENARIO" in
floor)
  WP=${2:?writer period}
  RP=${3:?reader period}
  SUB=${4:?subdevice}
  SIG="$M_ROOT/sig-clicks.raw"
  [ -f "$SIG" ] || python3 - "$SIG" <<'EOF'
import math, struct, sys
sr = 48000
click_n = int(sr * 0.001)
env = [0.5 - 0.5 * math.cos(math.pi * i / click_n) for i in range(click_n)]
data = [0.0] * int(sr * 2.6)
for t in [0.5, 0.8, 1.1, 1.4, 1.7, 2.0]:
    off = int(sr * t)
    for i in range(click_n):
        data[off + i] = 0.7 * env[i] * math.sin(2 * math.pi * 2000 * i / sr)
with open(sys.argv[1], 'wb') as f:
    for v in data:
        s = max(-32767, min(32767, int(v * 32767)))
        f.write(struct.pack('<hh', s, s))
EOF
  # 占位；aplay 启动时刻在拿到 APLAY_EPOCH 后回写完整 meta（单对象）。
  arecord -D "plughw:CARD=Loopback,DEV=1,SUBDEV=$SUB" -f S16_LE -c 2 -r "$SR" \
    --buffer-size=$(( RP * 4 )) --period-size="$RP" -t raw "$M_DIR/rec.raw" &
  REC_PID=$!
  sleep 0.5
  APLAY_EPOCH=$(now_ms)
  aplay -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$SUB" -f S16_LE -c 2 -r "$SR" \
    --buffer-size=$(( WP * 4 )) --period-size="$WP" "$SIG" &
  APLAY_PID=$!
  wait "$APLAY_PID"
  sleep 0.3
  kill "$REC_PID" 2>/dev/null
  wait "$REC_PID" 2>/dev/null
  cat > "$M_DIR/meta.json" <<EOF
{"writer_period":$WP,"reader_period":$RP,"sub":$SUB,
 "clicks_ms":[500,800,1100,1400,1700,2000],"aplay_epoch_ms":$APLAY_EPOCH}
EOF
  echo "floor done: $M_DIR/rec.raw"
  ;;

latency)
  kill_mozart
  daemon_up
  SIG="$M_DIR/signal.raw"
  python3 - "$SIG" > "$M_DIR/bursts.json" <<'EOF'
import json, math, struct, sys
sr = 48000
dur = 18.0
data = [0.0] * int(sr * dur)
def burst(t0, length=0.3, ramp=0.01, amp=24000 / 32768):
    off = int(sr * t0)
    n = int(sr * length)
    rn = int(sr * ramp)
    for i in range(n):
        v = math.sin(2 * math.pi * 220 * (i / sr))
        if i < rn:
            v *= 0.5 - 0.5 * math.cos(math.pi * i / rn)
        if i >= n - rn:
            v *= 0.5 - 0.5 * math.cos(math.pi * (n - i) / rn)
        data[off + i] = amp * v
bursts = []
t = 1.0
while t <= 4.5:            # direct 段：0.5 s 间距 8 个
    burst(t); bursts.append(round(t, 3)); t += 0.5
for t in [7.5, 8.23, 8.96, 9.69, 10.42]:   # rvc 冷稳态段
    burst(t); bursts.append(round(t, 3))
for t in [11.5, 12.0]:      # 切回 direct 后
    burst(t); bursts.append(round(t, 3))
for t in [14.5, 15.23, 15.96]:   # rvc 热稳态段
    burst(t); bursts.append(round(t, 3))
with open(sys.argv[1], 'wb') as f:
    for v in data:
        s = max(-32767, min(32767, int(v * 32767)))
        f.write(struct.pack('<hh', s, s))
print(json.dumps({"burst_start_s": bursts}))
EOF
  arecord -D "plughw:CARD=Loopback,DEV=1,SUBDEV=$PB_SUB" "${AREC_ARGS_COMMON[@]}" "$M_DIR/rec.raw" &
  REC_PID=$!
  sleep 0.5
  APLAY_EPOCH=$(now_ms)
  aplay -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "${APLAY_ARGS_COMMON[@]}" "$SIG" &
  APLAY_PID=$!
  # 时间线（相对 aplay 启动，ms）：5.0 切 rvc（冷）、11.0 切 direct、12.2 切 rvc（热）
  : > "$M_DIR/switches.jsonl"
  for spec in "5000 rvc-cold" "11000 direct" "12200 rvc-hot"; do
    set -- $spec
    sleep_until "$(( APLAY_EPOCH + $1 ))"
    case $2 in
      rvc-cold)  T=$(now_ms); ctl route set rvc;;
      direct)    T=$(now_ms); ctl route set direct;;
      rvc-hot)   T=$(now_ms); ctl route set rvc;;
    esac
    echo "{\"switch\":\"$2\",\"at_epoch_ms\":$(now_ms),\"cmd_wall_ms\":$(( $(now_ms) - T ))}" >> "$M_DIR/switches.jsonl"
  done
  wait "$APLAY_PID"
  sleep 0.5
  ctl status --json > "$M_DIR/st-final.json" 2>/dev/null
  kill "$REC_PID" 2>/dev/null
  wait "$REC_PID" 2>/dev/null
  cat > "$M_DIR/meta.json" <<EOF
{"scenario":"latency","sr":$SR,
 "aplay_epoch_ms":$APLAY_EPOCH,
 "direct_bursts_s":[1.0,1.5,2.0,2.5,3.0,3.5,4.0,4.5,11.5,12.0],
 "rvc_cold_bursts_s":[7.5,8.23,8.96,9.69,10.42],
 "rvc_hot_bursts_s":[14.5,15.23,15.96]}
EOF
  daemon_down || true
  echo "latency done"
  ;;

fade)
  kill_mozart
  daemon_up
  SIG="$M_DIR/signal.raw"
  python3 - "$SIG" <<'EOF'
import math, struct, sys
sr = 48000
dur = 21.0
with open(sys.argv[1], 'wb') as f:
    for i in range(int(sr * dur)):
        v = 24000 * math.sin(2 * math.pi * 220 * i / sr)
        f.write(struct.pack('<hh', int(v), int(v)))
EOF
  arecord -D "plughw:CARD=Loopback,DEV=1,SUBDEV=$PB_SUB" "${AREC_ARGS_COMMON[@]}" "$M_DIR/rec.raw" &
  REC_PID=$!
  sleep 0.5
  APLAY_EPOCH=$(now_ms)
  aplay -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "${APLAY_ARGS_COMMON[@]}" "$SIG" &
  APLAY_PID=$!
  : > "$M_DIR/switches.jsonl"
  for spec in "5000 rvc-cold" "12000 direct" "16000 rvc-hot"; do
    set -- $spec
    sleep_until "$(( APLAY_EPOCH + $1 ))"
    case $2 in
      rvc-cold)  T=$(now_ms); ctl route set rvc;;
      direct)    T=$(now_ms); ctl route set direct;;
      rvc-hot)   T=$(now_ms); ctl route set rvc;;
    esac
    echo "{\"switch\":\"$2\",\"at_epoch_ms\":$(now_ms),\"cmd_wall_ms\":$(( $(now_ms) - T ))}" >> "$M_DIR/switches.jsonl"
  done
  wait "$APLAY_PID"
  sleep 0.5
  kill "$REC_PID" 2>/dev/null
  wait "$REC_PID" 2>/dev/null
  echo "{\"scenario\":\"fade\",\"sr\":$SR,\"aplay_epoch_ms\":$APLAY_EPOCH}" > "$M_DIR/meta.json"
  daemon_down || true
  echo "fade done"
  ;;

longrun)
  SECONDS_RUN=${2:-60}
  kill_mozart
  daemon_up
  SIG="$M_DIR/signal.raw"
  python3 - "$SIG" <<EOF
import math, struct, sys
sr = $SR
dur = $SECONDS_RUN + 2
with open(sys.argv[1], 'wb') as f:
    for i in range(int(sr * dur)):
        v = 24000 * math.sin(2 * math.pi * 220 * i / sr)
        f.write(struct.pack('<hh', int(v), int(v)))
EOF
  T=$(now_ms); ctl route set rvc > "$M_DIR/route-rvc.out" 2>&1
  echo "{\"switch\":\"rvc\",\"at_epoch_ms\":$(now_ms),\"cmd_wall_ms\":$(( $(now_ms) - T ))}" > "$M_DIR/switches.jsonl"
  arecord -D "plughw:CARD=Loopback,DEV=1,SUBDEV=$PB_SUB" "${AREC_ARGS_COMMON[@]}" "$M_DIR/rec.raw" &
  REC_PID=$!
  sleep 0.5
  APLAY_EPOCH=$(now_ms)
  aplay -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "${APLAY_ARGS_COMMON[@]}" "$SIG" &
  APLAY_PID=$!
  STOP="$M_DIR/poll.stop"
  poll_status "$M_DIR/status.jsonl" 2 "$STOP" &
  POLL_PID=$!
  wait "$APLAY_PID"
  sleep 1
  touch "$STOP"; wait "$POLL_PID" 2>/dev/null
  kill "$REC_PID" 2>/dev/null
  wait "$REC_PID" 2>/dev/null
  ctl status --json > "$M_DIR/st-final.json" 2>/dev/null
  echo "{\"scenario\":\"longrun\",\"sr\":$SR,\"duration_s\":$SECONDS_RUN,\"aplay_epoch_ms\":$APLAY_EPOCH}" > "$M_DIR/meta.json"
  daemon_down || true
  echo "longrun done"
  ;;

voice)
  kill_mozart
  daemon_up
  espeak-ng -v zh -s 150 "今天天气很好，我们一起去公园散步，欣赏美丽的风景。" -w "$M_DIR/voice.wav" 2>/dev/null
  sox "$M_DIR/voice.wav" -r "$SR" -c 2 -t raw -e signed -b 16 "$M_DIR/signal.raw" 2>/dev/null
  LEN=$(python3 -c "import os;print(os.path.getsize('$M_DIR/signal.raw')//$SR//4)")
  echo "voice signal: ${LEN}s"
  arecord -D "plughw:CARD=Loopback,DEV=1,SUBDEV=$PB_SUB" "${AREC_ARGS_COMMON[@]}" "$M_DIR/rec.raw" &
  REC_PID=$!
  sleep 0.5
  APLAY_EPOCH=$(now_ms)
  aplay -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "${APLAY_ARGS_COMMON[@]}" "$M_DIR/signal.raw" &
  APLAY_PID=$!
  wait "$APLAY_PID"
  # direct 段已录；整段重放走 rvc
  sleep 0.5
  ctl route set rvc > "$M_DIR/route-rvc.out" 2>&1
  echo "{\"switch\":\"rvc-cold\",\"at_epoch_ms\":$(now_ms)}" > "$M_DIR/switches.jsonl"
  sleep 3   # 冷激活 + 启动缓冲
  APLAY_EPOCH2=$(now_ms)
  aplay -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "${APLAY_ARGS_COMMON[@]}" "$M_DIR/signal.raw" &
  APLAY_PID=$!
  wait "$APLAY_PID"
  sleep 1
  ctl status --json > "$M_DIR/st-final.json" 2>/dev/null
  kill "$REC_PID" 2>/dev/null
  wait "$REC_PID" 2>/dev/null
  echo "{\"scenario\":\"voice\",\"sr\":$SR,\"aplay_epoch_ms\":$APLAY_EPOCH,\"aplay2_epoch_ms\":$APLAY_EPOCH2,\"signal_s\":$LEN}" > "$M_DIR/meta.json"
  daemon_down || true
  echo "voice done"
  ;;

heal)
  WHO=${2:?pre|rvc}
  kill_mozart
  daemon_up
  SIG="$M_DIR/signal.raw"
  python3 - "$SIG" <<'EOF'
import math, struct, sys
sr = 48000
dur = 30.0
with open(sys.argv[1], 'wb') as f:
    for i in range(int(sr * dur)):
        v = 24000 * math.sin(2 * math.pi * 220 * i / sr)
        f.write(struct.pack('<hh', int(v), int(v)))
EOF
  T=$(now_ms); ctl route set rvc > "$M_DIR/route-rvc.out" 2>&1
  echo "{\"switch\":\"rvc\",\"at_epoch_ms\":$(now_ms),\"cmd_wall_ms\":$(( $(now_ms) - T ))}" > "$M_DIR/switches.jsonl"
  arecord -D "plughw:CARD=Loopback,DEV=1,SUBDEV=$PB_SUB" "${AREC_ARGS_COMMON[@]}" "$M_DIR/rec.raw" &
  REC_PID=$!
  sleep 0.5
  APLAY_EPOCH=$(now_ms)
  aplay -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "${APLAY_ARGS_COMMON[@]}" "$SIG" &
  APLAY_PID=$!
  STOP="$M_DIR/poll.stop"
  poll_status "$M_DIR/status.jsonl" 0.15 "$STOP" &
  POLL_PID=$!
  sleep 6                            # 稳态后动手
  ctl status --json > "$M_DIR/st-poll.json" 2>/dev/null
  if [ "$WHO" = rvc ]; then
    PID=$(jget "$M_DIR/st-poll.json" rvcproc pid)
  else
    PID=$(jget "$M_DIR/st-poll.json" preproc pid)
  fi
  [ -n "$PID" ] || die "cannot resolve $WHO pid"
  KILL_EPOCH=$(now_ms)
  kill -9 "$PID"
  echo "{\"kill\":\"$WHO\",\"pid\":$PID,\"at_epoch_ms\":$KILL_EPOCH}" > "$M_DIR/kill.json"
  APLAY2_EPOCH=0
  APLAY_SURVIVED=n/a
  if [ "$WHO" = pre ]; then
    # snd-aloop：读者（pre 采集端）死亡后旧缆对象在写端存活期间不释放，
    # 重开的 pre 只能采到静默；且写端先入新缆会被读者按缆时开盘追平，同样
    # 永久静默。因此 kill 后立刻退掉旧写端（两端齐闭释放缆对象），等
    # restarts>=1（daemon 已收尸，此时即翻转）后再固定延 2 s——覆盖 pre-2
    # 设备打开（约 0.6 s）与旧状态逐出——然后才重供信号。写端是否存活记入
    # 证据。真实声卡无此耦合（解除条件见报告适用范围）。
    if pgrep -f "aplay -D plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" > /dev/null; then
      APLAY_SURVIVED=yes
    else
      APLAY_SURVIVED=no
    fi
    kill "$APLAY_PID" 2>/dev/null
    wait "$APLAY_PID" 2>/dev/null
    for _ in $(seq 1 20); do
      ctl status --json > "$M_DIR/st-poll2.json" 2>/dev/null || { sleep 0.5; continue; }
      R=$(jget "$M_DIR/st-poll2.json" preproc restarts)
      [ "${R:-0}" -ge 1 ] 2>/dev/null && break
      sleep 0.5
    done
    sleep 2
    APLAY2_EPOCH=$(now_ms)
    aplay -D "plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB" "${APLAY_ARGS_COMMON[@]}" "$SIG" &
    APLAY_PID=$!
  fi
  sleep 20                           # 观察自愈与计数稳定性
  touch "$STOP"; wait "$POLL_PID" 2>/dev/null
  wait "$APLAY_PID"
  sleep 0.5
  ctl status --json > "$M_DIR/st-final.json" 2>/dev/null
  kill "$REC_PID" 2>/dev/null
  wait "$REC_PID" 2>/dev/null
  cat > "$M_DIR/meta.json" <<EOF
{"scenario":"heal","who":"$WHO","sr":$SR,"aplay_epoch_ms":$APLAY_EPOCH,
 "kill_epoch_ms":$KILL_EPOCH,"aplay2_epoch_ms":$APLAY2_EPOCH,
 "aplay_survived_kill":"$APLAY_SURVIVED"}
EOF
  daemon_down || true
  echo "heal-$WHO done"
  ;;

*)
  die "unknown scenario: $SCENARIO"
  ;;
esac
