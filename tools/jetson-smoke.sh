#!/bin/bash
# Jetson 三进程冒烟：daemon + 真 pre + 真 rvc，在目标设备上执行。
# 前置：本仓库已 rsync 到设备并完成构建（cargo build --release +
#   cmake --build --preset jetson-release，rvc 产物已复制到 target/release）。
# 拓扑（snd-aloop，SUBDEV 0 可能被 root 遗留进程占用，默认用空闲的 2/3）：
#   aplay 正弦 → Loopback DEV=0,SUBDEV=$CAP_SUB → pre 采集 DEV=1,SUBDEV=$CAP_SUB
#   pre 播放 DEV=0,SUBDEV=$PB_SUB → arecord DEV=1,SUBDEV=$PB_SUB 回录
# 结果文件留在 $SMOKE_DIR：rec.raw（回录）、st-*.json/txt（各阶段 status）。
#   MODELS_DIR  模型资产目录（默认 $HOME/Mozart-Old/rvc-golden/qiqi-zh-run）
#   BIN_DIR     部署二进制目录（默认 $HOME/Mozart/target/release）
#   SMOKE_DIR   产物目录（默认 /tmp/mozart-smoke）
set -u

MODELS_DIR=${MODELS_DIR:-$HOME/Mozart-Old/rvc-golden/qiqi-zh-run}
BIN_DIR=${BIN_DIR:-$HOME/Mozart/target/release}
SMOKE_DIR=${SMOKE_DIR:-/tmp/mozart-smoke}
CAP_SUB=${CAP_SUB:-2}
PB_SUB=${PB_SUB:-3}
RUN=$SMOKE_DIR/run
CFG=$SMOKE_DIR/smoke.toml

pkill -f mozart-daemon 2>/dev/null
pkill -f "mozart-pre --socket" 2>/dev/null
pkill -f "mozart-rvc --socket" 2>/dev/null
sleep 0.3
mkdir -p "$SMOKE_DIR"
rm -rf "$RUN" "$SMOKE_DIR"/rec.raw "$SMOKE_DIR"/signal.raw "$SMOKE_DIR"/st-*.json "$SMOKE_DIR"/st-*.txt

ts() { printf "[%4ss]" "$SECONDS"; }
ctl() { "$BIN_DIR/mozart-ctl" --config "$CFG" "$@"; }
# $3 仅 rvcproc 用；其余调用没有第三参，set -u 下用空默认。
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
' "$1" "$2" "${3:-}" 2>/dev/null; }

# 正弦测试信号：220 Hz、48 kHz、双声道、16 s。每次重新生成，
# 不复用产物目录中的同名旧文件。
python3 - "$SMOKE_DIR/signal.raw" <<'EOF'
import math, struct, sys
sr, dur, freq = 48000, 16.0, 220.0
with open(sys.argv[1], 'wb') as f:
    for i in range(int(sr * dur)):
        v = int(24000 * math.sin(2 * math.pi * freq * i / sr))
        f.write(struct.pack('<hh', v, v))
EOF

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

echo "=== up ==="
ctl up

echo "=== wait pre/rvc ready ==="
RVC_STATE=""
for i in $(seq 1 60); do
  ctl status --json > "$SMOKE_DIR/st-poll.json" 2>/dev/null
  PRE_OK=$(jget "$SMOKE_DIR/st-poll.json" pre)
  RVC_STATE=$(jget "$SMOKE_DIR/st-poll.json" rvcstate)
  if [ "$PRE_OK" = "yes" ] && [ -n "$RVC_STATE" ]; then break; fi
  sleep 1
done
cp "$SMOKE_DIR/st-poll.json" "$SMOKE_DIR/st-ready.json"
echo "ready after ${i}s: pre=$PRE_OK rvc.state=$RVC_STATE"

AREC="arecord -D plughw:CARD=Loopback,DEV=1,SUBDEV=$PB_SUB -f S16_LE -c 2 -r 48000 --buffer-size=4096 --period-size=1024 -t raw"
APLAY="aplay -D plughw:CARD=Loopback,DEV=0,SUBDEV=$CAP_SUB -f S16_LE -c 2 -r 48000 --buffer-size=4096 --period-size=1024"

$AREC "$SMOKE_DIR/rec.raw" &
REC_PID=$!
$APLAY "$SMOKE_DIR/signal.raw" &
APLAY_PID=$!

sleep 2                            # A：direct 直出段
echo "$(ts) route set rvc"
ctl route set rvc
sleep 5                            # B：rvc 变声段（LoadModel+StartStream+启动缓冲）
echo "$(ts) status @ rvc route"
ctl status > "$SMOKE_DIR/st-rvc.txt" 2>&1
ctl status --json > "$SMOKE_DIR/st-rvc.json"
RVC_PID=$(jget "$SMOKE_DIR/st-rvc.json" rvcproc pid)
echo "$(ts) kill -9 rvc pid=$RVC_PID"
kill -9 "$RVC_PID"
sleep 4                            # 崩溃重启 + 重放 LoadModel/StartStream
echo "$(ts) status @ healed"
ctl status > "$SMOKE_DIR/st-heal.txt" 2>&1
ctl status --json > "$SMOKE_DIR/st-heal.json"
echo "$(ts) route set direct"
ctl route set direct
sleep 3                            # C：direct 恢复段
echo "$(ts) final status"
ctl status > "$SMOKE_DIR/st-final.txt" 2>&1
ctl status --json > "$SMOKE_DIR/st-final.json"

kill "$APLAY_PID" "$REC_PID" 2>/dev/null
wait 2>/dev/null

echo "=== down ==="
ctl down
echo "=== residue check ==="
pgrep -af "mozart-(daemon|pre|rvc)" && echo "RESIDUE PROCESSES FOUND" || echo "no mozart processes"
for f in daemon.sock pre.sock rvc.sock rvc-in.sock rvc-out.sock; do
  if [ -S "$RUN/$f" ]; then
    python3 - "$RUN/$f" <<'EOF'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
try:
    s.connect(sys.argv[1]); print(sys.argv[1], "LIVE")
except OSError as e:
    print(sys.argv[1], "dead:", e.strerror)
EOF
  else
    echo "$RUN/$f: absent"
  fi
done

echo "=== rvc.log tail ==="
tail -20 "$RUN/logs/rvc.log"
