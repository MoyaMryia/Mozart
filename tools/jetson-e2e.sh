#!/usr/bin/env bash
# Jetson 部署后测试：本机执行，SSH 到目标设备，按套件执行并汇总。
# 功能套件开始前停止运行时，结束后重启；只跑单元测试时不碰运行时。
#   tools/jetson-e2e.sh <SSH目标> [套件...]
#   MOZART_JETSON=<SSH目标> tools/jetson-e2e.sh [套件...]
#
# 套件（不指定时默认 unit + smoke + file + language）：
#   --unit       设备端单元测试（cargo test + ctest）
#   --smoke      三进程冒烟
#   --file       文件转换链路
#   --file-models 多模型文件转换（MODELS_DIR 下全部模型）
#   --language   字幕/翻译/播报链路
#   --measure <场景> [参数...]  性能测量（需要 release 构建）
#   --all        全部功能套件（unit + smoke + file + file-models + language）
#
# 前置：已运行 tools/deploy-jetson.sh；~/Mozart 构建树、~/mozart-runtime/bin
# 与 mozart.toml 存在。证据目录：/tmp/mozart-smoke、/tmp/mozart-file-test、
# /tmp/mozart-multimodel、/tmp/mozart-language-test；measure 证据在 ~/mozart-t7。
set -euo pipefail

TARGET=${MOZART_JETSON:-}
SSH_OPTS=(-o ConnectTimeout=10)
RUN_UNIT=0
RUN_SMOKE=0
RUN_FILE=0
RUN_FILE_MODELS=0
RUN_LANGUAGE=0
RUN_MEASURE=0
SELECTED=0
MEASURE_ARGS=()

usage() {
  echo "用法：tools/jetson-e2e.sh <SSH目标> [--unit] [--smoke] [--file] [--file-models] [--language] [--all] [--measure <场景> [参数...]]" >&2
  echo "      MOZART_JETSON=<SSH目标> tools/jetson-e2e.sh" >&2
  echo "  不指定套件时默认 unit + smoke + file + language；--measure 需要 release 构建。" >&2
}

TARGET_ARG=0
while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --all)
      RUN_UNIT=1; RUN_SMOKE=1; RUN_FILE=1; RUN_FILE_MODELS=1; RUN_LANGUAGE=1; SELECTED=1; shift ;;
    --unit)
      RUN_UNIT=1; SELECTED=1; shift ;;
    --smoke)
      RUN_SMOKE=1; SELECTED=1; shift ;;
    --file)
      RUN_FILE=1; SELECTED=1; shift ;;
    --file-models)
      RUN_FILE_MODELS=1; SELECTED=1; shift ;;
    --language)
      RUN_LANGUAGE=1; SELECTED=1; shift ;;
    --measure)
      RUN_MEASURE=1; SELECTED=1; shift
      if [ $# -eq 0 ]; then
        echo "--measure 需要场景名，见 tools/jetson-measure.sh" >&2
        usage
        exit 2
      fi
      MEASURE_ARGS=("$@")
      break
      ;;
    -*)
      echo "未知参数：$1" >&2
      usage
      exit 2
      ;;
    *)
      if [ "$TARGET_ARG" -eq 1 ]; then
        echo "只能指定一个 SSH 目标。" >&2
        usage
        exit 2
      fi
      TARGET=$1
      TARGET_ARG=1
      shift
      ;;
  esac
done
[ -n "$TARGET" ] || { usage; exit 2; }
command -v ssh >/dev/null || { echo "缺 ssh" >&2; exit 1; }

if [ "$SELECTED" -eq 0 ]; then
  RUN_UNIT=1
  RUN_SMOKE=1
  RUN_FILE=1
  RUN_LANGUAGE=1
fi

remote_env="MOZART_RUNTIME_DIR=$(printf '%q' "${MOZART_RUNTIME_DIR:-}")"
remote_env+=" RUN_UNIT=$RUN_UNIT RUN_SMOKE=$RUN_SMOKE RUN_FILE=$RUN_FILE RUN_FILE_MODELS=$RUN_FILE_MODELS RUN_LANGUAGE=$RUN_LANGUAGE RUN_MEASURE=$RUN_MEASURE"
for v in MODELS_DIR CAP_SUB PB_SUB ASSETS_DIR SAMPLE_M4A M_ROOT; do
  if [ -n "${!v:-}" ]; then
    remote_env+=" $v=$(printf '%q' "${!v}")"
  fi
done

remote_cmd="$remote_env bash -s"
if [ "$RUN_MEASURE" -eq 1 ]; then
  remote_cmd+=" --"
  for a in "${MEASURE_ARGS[@]}"; do
    remote_cmd+=" $(printf '%q' "$a")"
  done
fi

suites=""
if [ "$RUN_UNIT" -eq 1 ]; then suites+=" unit"; fi
if [ "$RUN_SMOKE" -eq 1 ]; then suites+=" smoke"; fi
if [ "$RUN_FILE" -eq 1 ]; then suites+=" file"; fi
if [ "$RUN_FILE_MODELS" -eq 1 ]; then suites+=" file-models"; fi
if [ "$RUN_LANGUAGE" -eq 1 ]; then suites+=" language"; fi
if [ "$RUN_MEASURE" -eq 1 ]; then suites+=" measure"; fi
echo "== Jetson 部署后测试：$TARGET（${suites# }）=="

ssh "${SSH_OPTS[@]}" "$TARGET" "$remote_cmd" <<'REMOTE'
set -uo pipefail
RUNTIME_DIR=${MOZART_RUNTIME_DIR:-$HOME/mozart-runtime}
CTL="$RUNTIME_DIR/bin/mozart-ctl"
CFG="$RUNTIME_DIR/mozart.toml"
BIN="$RUNTIME_DIR/bin"
STATUS_JSON=/tmp/mozart-e2e-status.json
ASSETS_DIR=${ASSETS_DIR:-${MOZART_ASSETS_DIR:-$HOME/mozart-assets}}
STOPPED=0
NEED_STOP=0
if [ "$RUN_SMOKE" = "1" ] || [ "$RUN_FILE" = "1" ] || \
   [ "$RUN_FILE_MODELS" = "1" ] || [ "$RUN_LANGUAGE" = "1" ] || \
   [ "$RUN_MEASURE" = "1" ]; then
  NEED_STOP=1
fi

fail() { echo "E2E FAILED: $*" >&2; exit 1; }

health() {
  local i
  for i in $(seq 1 60); do
    if "$CTL" --config "$CFG" status --json > "$STATUS_JSON" 2>/dev/null; then
      if python3 - "$STATUS_JSON" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
rvc = d.get("rvc")
ok = bool(d.get("pre")) and (rvc is None or bool(rvc.get("state")))
sys.exit(0 if ok else 1)
PY
      then
        return 0
      fi
    fi
    sleep 1
  done
  return 1
}

restore() {
  rc=$?
  set +e
  if [ "$STOPPED" -eq 1 ]; then
    if [ -x "$CTL" ] && [ -f "$CFG" ]; then
      "$CTL" --config "$CFG" down >/dev/null 2>&1
    fi
    pkill -f mozart-daemon 2>/dev/null
    pkill -f 'mozart-pre --socket' 2>/dev/null
    pkill -f 'mozart-rvc --socket' 2>/dev/null
    pkill -f 'mozart-asr --socket' 2>/dev/null
    pkill -f 'mozart-tts --socket' 2>/dev/null
    pkill -f 'mozart-bridge --socket' 2>/dev/null
    sleep 0.3
    if [ -x "$CTL" ] && [ -f "$CFG" ]; then
      if "$CTL" --config "$CFG" up >/dev/null 2>&1 && health; then
        echo '运行时已恢复'
      else
        echo '运行时恢复失败，请手动检查' >&2
        rc=1
      fi
    fi
  fi
  exit "$rc"
}
trap restore EXIT

[ -d "$HOME/Mozart/tools" ] || fail "缺 ~/Mozart/tools，先部署"
[ -x "$CTL" ] || fail "缺 $CTL，先部署"
[ -f "$CFG" ] || fail "缺 $CFG"
[ -x "$BIN/examples/rvc_stub" ] || fail "缺 examples/rvc_stub，先部署"

cd "$HOME/Mozart" || fail '缺 ~/Mozart'
. "$HOME/.cargo/env" 2>/dev/null || true
if [ -e "$ASSETS_DIR/lib/libsherpa-onnx-c-api.so" ]; then
  export SHERPA_ONNX_LIB_DIR="$ASSETS_DIR/lib"
fi
export LD_LIBRARY_PATH="$ASSETS_DIR/lib:$ASSETS_DIR/llama${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

if [ "$RUN_UNIT" = "1" ]; then
  echo '== 套件：设备端单元测试 =='
  tools/jetson-unit-test.sh || fail 'jetson-unit-test.sh'
fi

if [ "$NEED_STOP" -eq 1 ]; then
  echo '== 停止运行时 =='
  STOPPED=1
  "$CTL" --config "$CFG" down >/dev/null 2>&1 || true
  for _ in $(seq 1 30); do
    pgrep -f 'mozart-(daemon|pre|rvc|asr|tts|bridge)' >/dev/null || break
    sleep 0.3
  done
  pgrep -f 'mozart-(daemon|pre|rvc|asr|tts|bridge)' >/dev/null \
    && fail '停止运行时后仍有残留进程'
fi

export BIN_DIR="$BIN"
export ASSETS_DIR

if [ "$RUN_SMOKE" = "1" ]; then
  echo '== 套件：三进程冒烟 =='
  tools/jetson-smoke.sh || fail 'jetson-smoke.sh'
fi
if [ "$RUN_FILE" = "1" ]; then
  echo '== 套件：文件转换链路 =='
  tools/jetson-file-test.sh || fail 'jetson-file-test.sh'
fi
if [ "$RUN_FILE_MODELS" = "1" ]; then
  echo '== 套件：多模型文件转换 =='
  RESTORE_RUNTIME=0 MODELS_DIR="${MODELS_DIR:-$ASSETS_DIR/models/rvc}" \
    tools/jetson-file-multimodel.sh || fail 'jetson-file-multimodel.sh'
fi
if [ "$RUN_LANGUAGE" = "1" ]; then
  echo '== 套件：字幕/翻译/播报链路 =='
  tools/jetson-language-test.sh || fail 'jetson-language-test.sh'
fi
if [ "$RUN_MEASURE" = "1" ]; then
  [ -x "$HOME/Mozart/target/release/mozart-daemon" ] || fail '缺 release 构建，无法测量'
  echo '== 套件：性能测量 =='
  BIN_DIR="$HOME/Mozart/target/release" tools/jetson-measure.sh "$@" \
    || fail 'jetson-measure.sh'
fi

echo '== 全部选中套件通过 =='
echo '证据：/tmp/mozart-smoke /tmp/mozart-file-test /tmp/mozart-multimodel /tmp/mozart-language-test ~/mozart-t7'
REMOTE
