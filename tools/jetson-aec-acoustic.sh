#!/usr/bin/env bash
# Jetson 物理声卡实时声学录测：用真实扬声器外放 + 真实麦克风采集，对比 AEC 关/开效果，
# 并用 --aec-probe / align.py 测定真实双硬件时钟漂移、物理声学延时与实测 ERLE。
#
# 用法：
#   tools/jetson-aec-acoustic.sh [<SSH目标>] [选项]
#   MOZART_JETSON=<SSH目标> tools/jetson-aec-acoustic.sh [选项]
#
# 选项：
#   --duration <秒>       单轮播录时长（默认 12）
#   --capture-dev <设备>  采集 ALSA 设备（默认读取板端 mozart.toml）
#   --playback-dev <设备> 播放 ALSA 设备（默认读取板端 mozart.toml）
#   --wav <板端路径>      远端参考语音 WAV（默认使用板端 TTS 测试语音）
#   --out <本地目录>      保存输出 WAV 与 report.json 的目录
#   --with-denoise        开启 WOLA 降噪一并测试（默认关闭，仅测纯 AEC3）
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${MOZART_JETSON:-}
DURATION=12
CAPTURE_DEV=""
PLAYBACK_DEV=""
REMOTE_WAV=""
OUT_DIR=""
WITH_DENOISE=0

usage() {
  sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//' >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --duration) DURATION=${2:?缺少 --duration 参数}; shift 2 ;;
    --capture-dev) CAPTURE_DEV=${2:?缺少 --capture-dev 参数}; shift 2 ;;
    --playback-dev) PLAYBACK_DEV=${2:?缺少 --playback-dev 参数}; shift 2 ;;
    --wav) REMOTE_WAV=${2:?缺少 --wav 参数}; shift 2 ;;
    --out) OUT_DIR=${2:?缺少 --out 参数}; shift 2 ;;
    --with-denoise) WITH_DENOISE=1; shift ;;
    -*) echo "未知参数：$1" >&2; usage; exit 2 ;;
    *)
      [ -z "$TARGET" ] || [ "$TARGET" = "${MOZART_JETSON:-}" ] || { echo "只能指定一个 SSH 目标" >&2; exit 2; }
      TARGET=$1; shift ;;
  esac
done

[ -n "$TARGET" ] || { echo "请提供 SSH 目标或设置 MOZART_JETSON" >&2; usage; exit 2; }
for tool in ssh scp python3; do
  command -v "$tool" >/dev/null || { echo "缺少本机工具：$tool" >&2; exit 1; }
done

TS=$(date +%Y%m%d-%H%M%S)
if [ -z "$OUT_DIR" ]; then
  if [ -d "$REPO/../assets" ]; then
    OUT_DIR="$REPO/../assets/out/${TS}-aec-acoustic"
  else
    OUT_DIR="/tmp/mozart-${TS}-aec-acoustic"
  fi
fi
mkdir -p "$OUT_DIR"

REMOTE_OUT="/tmp/mozart-aec-acoustic-${TS}"
SSH_OPTS=(-o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=3)

echo "== Jetson 声学 AEC 实测：$TARGET（每轮 ${DURATION}s，输出 -> $OUT_DIR）=="
ssh "${SSH_OPTS[@]}" "$TARGET" \
  "DURATION=$(printf %q "$DURATION") CAPTURE_DEV=$(printf %q "$CAPTURE_DEV") PLAYBACK_DEV=$(printf %q "$PLAYBACK_DEV") REMOTE_WAV=$(printf %q "$REMOTE_WAV") REMOTE_OUT=$(printf %q "$REMOTE_OUT") WITH_DENOISE=$(printf %q "$WITH_DENOISE") bash -s" <<'REMOTE'
set -euo pipefail

RUNTIME_DIR=${MOZART_RUNTIME_DIR:-$HOME/mozart-runtime}
CTL="$RUNTIME_DIR/bin/mozart-ctl"
PRE_BIN="$RUNTIME_DIR/bin/mozart-pre"
CFG="$RUNTIME_DIR/mozart.toml"
ASSETS_DIR=${MOZART_ASSETS_DIR:-$HOME/mozart-assets}
REPO_DIR=${REPO_DIR:-$HOME/Mozart}
ALIGN_PY="$REPO_DIR/pre/aec-probe/align.py"
DEFAULT_WAV="$ASSETS_DIR/models/tts/sherpa-onnx-pocket-tts-int8-2026-01-26/test_wavs/sample_fr_hibiki_crepes.wav"
WAV_PATH=${REMOTE_WAV:-$DEFAULT_WAV}

[ -x "$PRE_BIN" ] || { echo "缺少 $PRE_BIN" >&2; exit 1; }
[ -f "$CFG" ] || { echo "缺少 $CFG" >&2; exit 1; }
[ -f "$WAV_PATH" ] || { echo "缺少测试语音 $WAV_PATH" >&2; exit 1; }

STOPPED=0
SYSTEMD_MANAGED=0
sctl=$(command -v systemctl || true)
if [ -n "$sctl" ]; then
  wd=$("$sctl" show -p WorkingDirectory --value mozart.service 2>/dev/null || true)
  if [ -n "$wd" ] && [ "$(realpath -m "$wd")" = "$(realpath -m "$RUNTIME_DIR")" ]; then
    svc_state=$("$sctl" is-active mozart.service 2>/dev/null || true)
    case "$svc_state" in
      active|activating|reloading|deactivating)
        SYSTEMD_MANAGED=1
        ;;
    esac
  fi
fi

restore() {
  rc=$?
  set +e
  pkill -f "$REMOTE_OUT/run/pre.sock" 2>/dev/null || true
  if [ "$STOPPED" -eq 1 ]; then
    sleep 0.3
    if [ "$SYSTEMD_MANAGED" -eq 1 ]; then
      sudo -n "$sctl" start mozart.service >/dev/null 2>&1 || echo "警告：恢复 mozart.service 失败" >&2
    elif [ -x "$CTL" ]; then
      "$CTL" --config "$CFG" up >/dev/null 2>&1 || echo "警告：恢复 mozart-ctl up 失败" >&2
    fi
  fi
  exit "$rc"
}
trap restore EXIT

echo "== 1/4 暂停在线运行时以独占物理声卡 =="
STOPPED=1
if [ "$SYSTEMD_MANAGED" -eq 1 ]; then
  sudo -n "$sctl" stop mozart.service
else
  "$CTL" --config "$CFG" down >/dev/null 2>&1 || true
fi
for _ in $(seq 1 30); do
  pgrep -f 'mozart-(daemon|pre|rvc|asr|tts|bridge)' >/dev/null || break
  sleep 0.2
done

mkdir -p "$REMOTE_OUT/run"

python3 - "$CFG" "$WAV_PATH" "$REMOTE_OUT" "$DURATION" "$CAPTURE_DEV" "$PLAYBACK_DEV" "$WITH_DENOISE" "$PRE_BIN" "$ASSETS_DIR/lib" "$ALIGN_PY" <<'PY'
import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time
import tomllib
import wave
import numpy as np

cfg_file, wav_path, out_dir, dur_s, cap_override, pb_override, denoise_flag, pre_bin, lib_dir, align_py = sys.argv[1:]
duration = float(dur_s)
with_denoise = denoise_flag == "1"

with open(cfg_file, "rb") as f:
    toml_cfg = tomllib.load(f)
capture_dev = cap_override or toml_cfg["pre"]["capture_device"]
playback_dev = pb_override or toml_cfg["pre"]["playback_device"]

CTL_MAGIC = 0x4C54434D
CTL_VERSION = 1
PRE_HELLO = 0x0201
PRE_GET_STATUS = 0x0204
PRE_SET_PLAYOUT = 0x0209
PRE_SET_AEC = 0x020A
SPEECH_MAGIC = 0x50535A4D
INPUT_MAGIC = 0x4D5A5254


def ctl_call(sock_path, cmd_type, body, req_id=1):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    s.settimeout(5.0)
    s.connect(sock_path)
    try:
        payload = json.dumps(body).encode()
        s.send(struct.pack("<IHHII", CTL_MAGIC, CTL_VERSION, cmd_type, req_id, len(payload)) + payload)
        resp = s.recv(65536)
        magic, _, rtype, rreq, rlen = struct.unpack("<IHHII", resp[:16])
        assert magic == CTL_MAGIC and rtype == cmd_type and rreq == req_id
        env = json.loads(resp[16:16 + rlen])
        assert env["status"] == "ok", env
        return env["data"]
    finally:
        s.close()


def wait_sock(path, timeout=8.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(path):
            try:
                ctl_call(path, PRE_HELLO, {})
                return
            except OSError:
                pass
        time.sleep(0.05)
    raise RuntimeError(f"等待 {path} 超时")


def load_speech_48k(path, seconds):
    with wave.open(path, "rb") as w:
        sr = w.getframerate()
        ch = w.getnchannels()
        pcm = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(np.float32) / 32768.0
    if ch > 1:
        pcm = pcm.reshape(-1, ch)[:, 0]
    n_target = int(seconds * 48000)
    idx = (np.arange(n_target, dtype=np.float64) * (sr / 48000.0)) % max(1, len(pcm) - 1)
    res = np.interp(idx, np.arange(len(pcm), dtype=np.float64), pcm).astype(np.float32)
    peak = float(np.max(np.abs(res)))
    if peak > 0:
        res *= (0.45 / peak)
    return res


def save_wav_16k(path, samples):
    clipped = np.clip(samples, -1.0, 1.0)
    pcm16 = (clipped * 32767.0).astype(np.int16)
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(pcm16.tobytes())


run_dir = os.path.join(out_dir, "run")
probe_dir = os.path.join(out_dir, "probe")
align_dir = os.path.join(out_dir, "aligned")
test_cfg = os.path.join(out_dir, "mozart.toml")
with open(test_cfg, "w") as f:
    f.write(f"""
[daemon]
run_dir = "{run_dir}"

[pre]
capture_device = "{capture_dev}"
playback_device = "{playback_dev}"
capture_gain_db = 0.0

[pre.dsp]
denoise = {"true" if with_denoise else "false"}
gate = false
agc = false
limiter = false

[pre.aec]
enabled = false
""")

asr_sock_path = os.path.join(run_dir, "asr-in.sock")
asr_srv = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
asr_srv.bind(asr_sock_path)
asr_srv.settimeout(0.2)

rec_buf = []
rec_lock = threading.Lock()
stop_rx = threading.Event()


def rx_loop():
    while not stop_rx.is_set():
        try:
            pkt = asr_srv.recv(4096)
            if len(pkt) == 1300 and struct.unpack("<I", pkt[:4])[0] == INPUT_MAGIC:
                frame = np.frombuffer(pkt[20:1300], dtype=np.float32).copy()
                with rec_lock:
                    rec_buf.append(frame)
        except socket.timeout:
            continue
        except OSError:
            break


t_rx = threading.Thread(target=rx_loop, daemon=True)
t_rx.start()

env = os.environ.copy()
env["LD_LIBRARY_PATH"] = f"{lib_dir}:{env.get('LD_LIBRARY_PATH', '')}".rstrip(":")
pre_sock = os.path.join(run_dir, "pre.sock")
proc = subprocess.Popen(
    [pre_bin, "--socket", pre_sock, "--config", test_cfg, "--aec-probe", probe_dir],
    env=env,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
)

try:
    wait_sock(pre_sock)
    ctl_call(pre_sock, PRE_SET_PLAYOUT, {"policy": "broadcast_only", "route": "direct", "mute": False})
    tts_sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    tts_sock.connect(os.path.join(run_dir, "tts-out.sock"))
    speech = load_speech_48k(wav_path, duration)
    n_frames = len(speech) // 960

    def run_round(aec_enabled, label):
        print(f"== {label} (aec={aec_enabled}, {duration:.0f}s) ==")
        ctl_call(pre_sock, PRE_SET_AEC, {"enabled": aec_enabled}, req_id=100)
        time.sleep(0.3)
        with rec_lock:
            rec_buf.clear()
        snaps = []
        t0 = time.monotonic()
        t0_ns = time.monotonic_ns()
        for idx in range(n_frames):
            chunk = speech[idx * 960:(idx + 1) * 960]
            pkt = (
                struct.pack("<IQ", SPEECH_MAGIC, t0_ns)
                + struct.pack("<QIBBBB", t0_ns + idx * 20_000_000, idx, 1, 0, 0, 0)
                + struct.pack("<II", 48000, 960)
                + chunk.tobytes()
            )
            tts_sock.send(pkt)
            if idx % 50 == 49:
                st = ctl_call(pre_sock, PRE_GET_STATUS, {}, req_id=200 + idx)
                snaps.append({"t_s": round((idx + 1) * 0.02, 2), "aec": st["aec"], "pb_xrun": st["playback_underruns"]})
            rem = t0 + (idx + 1) * 0.020 - time.monotonic()
            if rem > 0:
                time.sleep(rem)
        time.sleep(0.3)
        end_st = ctl_call(pre_sock, PRE_GET_STATUS, {}, req_id=999)
        with rec_lock:
            audio = np.concatenate(rec_buf) if rec_buf else np.zeros(1, dtype=np.float32)
        return audio, snaps, end_st

    audio_off, snaps_off, st_off = run_round(False, "2/4 录制 AEC 关闭基线")
    time.sleep(0.6)
    audio_on, snaps_on, st_on = run_round(True, "3/4 录制 AEC 开启消回声")
    tts_sock.close()

    proc.terminate()
    _, stderr = proc.communicate(timeout=5)
    stop_rx.set()
    t_rx.join(timeout=2)
    asr_srv.close()

    save_wav_16k(os.path.join(out_dir, "out_aec_off.wav"), audio_off)
    save_wav_16k(os.path.join(out_dir, "out_aec_on.wav"), audio_on)

    # 跳过前 4 s 收敛期，对比 4s..end 稳态回声残留能量
    start_16k = min(4 * 16000, len(audio_off) // 2, len(audio_on) // 2)
    rms_off = float(np.sqrt(np.mean(audio_off[start_16k:] ** 2)))
    rms_on = float(np.sqrt(np.mean(audio_on[start_16k:] ** 2)))
    acoustic_erle_db = 20.0 * np.log10((rms_off + 1e-12) / (rms_on + 1e-12))

    # 跑 align.py 估算双卡漂移与对齐覆盖率，并用互相关找物理声学延时
    align_info = {}
    if os.path.exists(align_py):
        res = subprocess.run(["python3", align_py, probe_dir, align_dir], capture_output=True, text=True)
        if res.returncode == 0:
            align_info = json.loads(res.stdout)
            with wave.open(os.path.join(align_dir, "mic.wav"), "rb") as wm, wave.open(os.path.join(align_dir, "reference.wav"), "rb") as wr:
                mic_a = np.frombuffer(wm.readframes(wm.getnframes()), dtype=np.int16).astype(np.float32)
                ref_a = np.frombuffer(wr.readframes(wr.getnframes()), dtype=np.int16).astype(np.float32)
            max_lag = int(0.20 * 48000)
            seg_len = min(len(mic_a), len(ref_a), 8 * 48000)
            if seg_len > max_lag * 3:
                m_seg = mic_a[:seg_len]
                r_seg = ref_a[:seg_len]
                # FFT 快速互相关，寻找 mic 相对 ref 的物理声学滞后（>= 0）
                n_fft = 1 << int(np.ceil(np.log2(seg_len * 2)))
                cc = np.fft.irfft(np.fft.rfft(m_seg, n_fft) * np.conj(np.fft.rfft(r_seg, n_fft)))
                pos_cc = np.abs(cc[:max_lag])
                lag_samples = int(np.argmax(pos_cc))
                align_info["estimated_acoustic_delay_ms"] = round(lag_samples * 1000.0 / 48000.0, 2)
        else:
            align_info["error"] = res.stderr.strip()

    report = {
        "capture_device": capture_dev,
        "playback_device": playback_dev,
        "duration_s": duration,
        "with_denoise": with_denoise,
        "rms_off_4s_end": round(rms_off, 6),
        "rms_on_4s_end": round(rms_on, 6),
        "measured_acoustic_erle_db": round(acoustic_erle_db, 2),
        "aec_on_final": st_on["aec"],
        "aec_on_timeline": snaps_on,
        "playback_underruns_final": st_on["playback_underruns"],
        "frames_captured": st_on["frames_captured"],
        "frames_played": st_on["frames_played"],
        "align": align_info,
        "pre_stderr": stderr.strip().splitlines(),
    }
    with open(os.path.join(out_dir, "report.json"), "w") as f:
        json.dump(report, f, indent=2, ensure_ascii=False)
    print("== 4/4 实测汇总 ==")
    print(json.dumps({k: v for k, v in report.items() if k not in ("aec_on_timeline", "pre_stderr")}, indent=2, ensure_ascii=False))
finally:
    stop_rx.set()
    if proc.poll() is None:
        proc.kill()
        proc.wait()
    try:
        asr_srv.close()
    except OSError:
        pass
PY
REMOTE

scp "${SSH_OPTS[@]}" "$TARGET:$REMOTE_OUT/out_aec_off.wav" "$TARGET:$REMOTE_OUT/out_aec_on.wav" "$TARGET:$REMOTE_OUT/report.json" "$OUT_DIR/"
ssh "${SSH_OPTS[@]}" "$TARGET" "rm -rf $(printf %q "$REMOTE_OUT")"
echo "== 完成：结果保存在 $OUT_DIR（含 out_aec_off.wav / out_aec_on.wav / report.json）=="
