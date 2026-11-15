#!/usr/bin/env bash
# Jetson 一键部署：本机执行，构建在目标设备上完成，产物不回传本机。
# 部署不跑测试；设备端单测与功能测试都在部署后由 tools/jetson-e2e.sh 执行。
#   tools/deploy-jetson.sh <SSH目标>
#   MOZART_JETSON=<SSH目标> tools/deploy-jetson.sh
#
# SSH 目标必须显式提供，脚本不保存设备地址。地址变化由参数或 ~/.ssh/config
# 别名解决；脚本不做连接重试，连不上立即报错并以非零退出。
#
# 流程（1 的 rsync 在本机，其余动作在远端 ssh 内，输出原样透传）：
#   1 部署前快照：~/.mozart-prev 存在时，把 ~/Mozart 源码（不含 target/build）
#     拷入 ~/.mozart-prev，作为本次部署的回滚锚点；首次运行跳过。
#   2 源码同步：本机仓库 → 远端 ~/Mozart（rsync -a --delete，排除 target/build/.git）；
#     随后归一增量构建的新鲜度：-a 保留本机旧 mtime，远端 target/、build/ 里
#     mtime 更新的旧产物会让构建器误判"源码未变"跳过重编，部署出落后于源码的
#     陈旧二进制并带病进入运行位（实测暴露）。ninja 按 mtime 判定，把同步树全部
#     文件 mtime 前拨即强制重编；cargo 的指纹判定在旧 mtime 场景下实测不可依赖，
#     删除工作区各包的指纹逼其重编，注册表第三方依赖保持缓存。指纹名单按
#     workspace 成员推导，新增 crate 自动纳入。每次部署为全量重编。
#   3 构建：cargo build --workspace --bins --examples（debug）；
#     cmake --preset jetson-release；rvc 产物对齐 target/debug。失败即回滚。
#   4 安装与重启：停运行时并确认无残留进程；~/mozart-runtime/bin 备份到
#     bin.prev；安装 7 个程序与 examples/rvc_stub；用 ~/mozart-runtime/mozart.toml
#     重启并等待 pre/rvc 健康；写 deploy.json。安装或重启失败时从 bin.prev
#     恢复上一代二进制并重启旧栈。
#   5 摘要。
#
# 任一步失败（含 Ctrl-C 中断）：恢复部署前源码快照，清掉 target/debug 部署产物；
# 安装阶段失败时先按 4 的恢复逻辑还二进制并重启旧栈。回滚不改动
# ~/mozart-assets、~/mozart-runtime 的配置与状态。脚本幂等可重复。
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${MOZART_JETSON:-}
SSH_OPTS=(-o ConnectTimeout=10)

usage() {
  echo "用法：tools/deploy-jetson.sh <SSH目标>" >&2
  echo "      MOZART_JETSON=<SSH目标> tools/deploy-jetson.sh" >&2
  echo "  SSH目标必填；脚本不保存设备地址。" >&2
}

TARGET_ARG=0
for arg in "$@"; do
  case "$arg" in
    -h|--help) usage; exit 0 ;;
    -*) echo "未知参数：$arg" >&2; usage; exit 2 ;;
    *)
      if [ "$TARGET_ARG" -eq 1 ]; then
        echo "只能指定一个 SSH 目标。" >&2
        usage
        exit 2
      fi
      TARGET=$arg
      TARGET_ARG=1
      ;;
  esac
done
[ -n "$TARGET" ] || { usage; exit 2; }

command -v ssh >/dev/null || { echo "缺 ssh" >&2; exit 1; }
command -v rsync >/dev/null || { echo "缺 rsync" >&2; exit 1; }

COMMIT=$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)
DIRTY=0
[ -z "$(git -C "$REPO" status --porcelain 2>/dev/null)" ] || DIRTY=1
RUNTIME_ENV="MOZART_RUNTIME_DIR=$(printf '%q' "${MOZART_RUNTIME_DIR:-}")"

STEP=参数解析
ROLLED_BACK=0

# ---- 回滚：源码与二进制各归其位，结果如实输出。不依赖 set -e。----
rollback() {
  ROLLED_BACK=1
  trap - INT TERM EXIT
  echo >&2
  echo "== 回滚（失败步骤：$STEP）==" >&2
  if ssh "${SSH_OPTS[@]}" "$TARGET" "$RUNTIME_ENV bash -s" <<'ROLLBACK'
set -u
RUNTIME_DIR=${MOZART_RUNTIME_DIR:-$HOME/mozart-runtime}
CTL="$RUNTIME_DIR/bin/mozart-ctl"
CFG="$RUNTIME_DIR/mozart.toml"
rc=0

if [ -f "$RUNTIME_DIR/.installing" ]; then
  echo '-- 恢复上一代运行时二进制 --'
  if [ -x "$CTL" ] && [ -f "$CFG" ]; then
    "$CTL" --config "$CFG" down >/dev/null 2>&1 || true
  fi
  if [ -d "$RUNTIME_DIR/bin.prev" ]; then
    if rsync -a --delete "$RUNTIME_DIR/bin.prev/" "$RUNTIME_DIR/bin/"; then
      rm -f "$RUNTIME_DIR/.installing"
      if [ -x "$CTL" ] && [ -f "$CFG" ]; then
        "$CTL" --config "$CFG" up >/dev/null 2>&1 || rc=1
      fi
      rm -f "$RUNTIME_DIR/.stopped"
      echo '运行时二进制已恢复为 bin.prev，并尝试重启旧栈'
    else
      echo '回滚失败：bin.prev 恢复 rsync 出错' >&2
      rc=1
    fi
  else
    rm -rf "$RUNTIME_DIR/bin"
    rm -f "$RUNTIME_DIR/.installing"
    echo '本次部署前无二进制备份，已移除安装结果并保持停机'
  fi
elif [ -f "$RUNTIME_DIR/.stopped" ]; then
  echo '-- 运行时已停机但未进入安装，尝试重启当前栈 --'
  if [ -x "$CTL" ] && [ -f "$CFG" ]; then
    "$CTL" --config "$CFG" up >/dev/null 2>&1 || rc=1
  fi
  rm -f "$RUNTIME_DIR/.stopped"
else
  echo '未进入安装阶段，运行时保持原状'
fi

echo '-- 源码恢复 --'
if [ -d "$HOME/.mozart-prev" ]; then
  if rsync -a --delete --exclude target --exclude build \
       "$HOME/.mozart-prev/" "$HOME/Mozart/"; then
    echo '~/Mozart 源码已恢复为部署前快照（~/.mozart-prev）'
  else
    echo '回滚失败：快照恢复 rsync 出错，~/Mozart 保持当前状态' >&2
    rc=1
  fi
else
  echo '本次部署前未做快照（首次运行），源码保持同步后的状态'
fi

echo '-- 清理部署位二进制 --'
rm -f "$HOME/Mozart/target/debug/mozart-daemon" \
      "$HOME/Mozart/target/debug/mozart-pre" \
      "$HOME/Mozart/target/debug/mozart-ctl" \
      "$HOME/Mozart/target/debug/mozart-rvc" \
      "$HOME/Mozart/target/debug/mozart-asr" \
      "$HOME/Mozart/target/debug/mozart-tts" \
      "$HOME/Mozart/target/debug/mozart-bridge" \
      "$HOME/Mozart/target/debug/examples/rvc_stub" 2>/dev/null \
  && echo 'target/debug 部署产物已清' \
  || { echo 'target/debug 产物清理失败（目录可能不存在），注意甄别' >&2; rc=1; }
exit "$rc"
ROLLBACK
  then
    :
  else
    echo '回滚未能完成：远端回滚命令返回非零，见上方输出' >&2
  fi
}

on_exit() {
  local rc=$?
  trap - INT TERM
  if [ "$rc" -eq 0 ]; then
    return 0
  fi
  if [ "$ROLLED_BACK" -eq 0 ]; then
    rollback
  fi
  exit 1
}
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "== Jetson 部署：$TARGET =="

# 1 部署前快照。
STEP='1/5 部署前快照'
echo "== $STEP =="
ssh "${SSH_OPTS[@]}" "$TARGET" bash -s <<'REMOTE'
set -euo pipefail
if [ -d "$HOME/.mozart-prev" ]; then
  rsync -a --delete --exclude target --exclude build "$HOME/Mozart/" "$HOME/.mozart-prev/"
  echo '部署前快照完成：~/.mozart-prev 已更新为部署前源码'
else
  echo '首次运行：~/.mozart-prev 不存在，跳过部署前快照（没有可回滚的上一代）'
fi
REMOTE

# 2 源码同步与预检。
STEP='2/5 源码同步与预检'
echo "== $STEP =="
rsync -a --delete -e "ssh -o ConnectTimeout=10" \
  --exclude target --exclude build --exclude .git \
  "$REPO/" "$TARGET":~/Mozart/
ssh "${SSH_OPTS[@]}" "$TARGET" bash -s <<'REMOTE'
set -euo pipefail
. "$HOME/.cargo/env" 2>/dev/null || true
for tool in cargo cmake ninja; do
  command -v "$tool" >/dev/null || { echo "缺 $tool" >&2; exit 1; }
done
avail=$(df -Pk "$HOME" | awk 'NR==2 {print $4}')
if [ -n "$avail" ] && [ "$avail" -lt 2097152 ]; then
  echo "警告：$HOME 剩余空间不足 2 GiB（${avail} KiB），构建可能失败" >&2
fi
cd ~/Mozart
find . \( -path ./target -o -path ./build \) -prune \
  -o -type f -exec touch {} +
for name in $(awk '/^members *=/ {sub(/^members *= *\[/, ""); sub(/\]$/, ""); gsub(/[" ,]/, " "); print}' Cargo.toml); do
  rm -rf "target/debug/.fingerprint/${name}-*"
done
REMOTE
echo '新鲜度归一完成（mtime 前拨 + cargo 指纹重置）'

# 3 构建。
STEP='3/5 构建'
echo "== $STEP（$TARGET）=="
ssh "${SSH_OPTS[@]}" "$TARGET" bash -s <<'REMOTE'
set -euo pipefail
cd "$HOME/Mozart"
. "$HOME/.cargo/env"

ASSETS_DIR=${MOZART_ASSETS_DIR:-$HOME/mozart-assets}
if [ -e "$ASSETS_DIR/lib/libsherpa-onnx-c-api.so" ]; then
  export SHERPA_ONNX_LIB_DIR="$ASSETS_DIR/lib"
fi
export LD_LIBRARY_PATH="$ASSETS_DIR/lib:$ASSETS_DIR/llama${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

echo '== 3/5 cargo build --workspace --bins --examples（debug）=='
cargo build --workspace --bins --examples

echo '== 3/5 cmake jetson-release =='
cmake --preset jetson-release -DFETCHCONTENT_UPDATES_DISCONNECTED=ON
cmake --build --preset jetson-release --parallel 2
cp -f build/jetson-release/rvc/mozart-rvc target/debug/
REMOTE

# 4 安装与重启。
STEP='4/5 安装与重启'
echo "== $STEP =="
ssh "${SSH_OPTS[@]}" "$TARGET" \
  "$RUNTIME_ENV COMMIT=$(printf '%q' "$COMMIT") DIRTY=$DIRTY bash -s" <<'REMOTE'
set -euo pipefail
cd "$HOME/Mozart"
RUNTIME_DIR=${MOZART_RUNTIME_DIR:-$HOME/mozart-runtime}
CTL="$RUNTIME_DIR/bin/mozart-ctl"
CFG="$RUNTIME_DIR/mozart.toml"
STATUS_JSON=/tmp/mozart-deploy-status.json

[ -d "$RUNTIME_DIR" ] || { echo "运行时目录不存在：$RUNTIME_DIR" >&2; exit 1; }
[ -f "$CFG" ] || { echo "缺配置文件：$CFG" >&2; exit 1; }

recover() {
  set +e
  echo '安装或重启失败，恢复上一代二进制' >&2
  "$CTL" --config "$CFG" down >/dev/null 2>&1 || true
  if [ -f "$RUNTIME_DIR/.installing" ]; then
    if [ -d "$RUNTIME_DIR/bin.prev" ]; then
      rsync -a --delete "$RUNTIME_DIR/bin.prev/" "$RUNTIME_DIR/bin/" || true
      rm -f "$RUNTIME_DIR/.installing"
      if [ -x "$CTL" ] && [ -f "$CFG" ]; then
        "$CTL" --config "$CFG" up >/dev/null 2>&1 || true
      fi
      echo '已从 bin.prev 恢复上一代二进制并尝试重启' >&2
    else
      rm -rf "$RUNTIME_DIR/bin"
      rm -f "$RUNTIME_DIR/.installing"
      echo '无 bin.prev，已移除本次安装的二进制并保持停机' >&2
    fi
  else
    echo '安装尚未开始，保持当前二进制' >&2
    if [ -x "$CTL" ] && [ -f "$CFG" ]; then
      "$CTL" --config "$CFG" up >/dev/null 2>&1 || true
    fi
  fi
  rm -f "$RUNTIME_DIR/.stopped"
  exit 1
}
fail_install() { echo "$*" >&2; recover; }
trap 'recover' ERR

echo '-- 停止运行时 --'
touch "$RUNTIME_DIR/.stopped"
if [ -x "$CTL" ]; then
  "$CTL" --config "$CFG" down >/dev/null 2>&1 || true
else
  echo '当前无 mozart-ctl，跳过停机（首次部署）'
fi
for _ in $(seq 1 30); do
  pgrep -f 'mozart-(daemon|pre|rvc|asr|tts|bridge)' >/dev/null || break
  sleep 0.3
done
if pgrep -f 'mozart-(daemon|pre|rvc|asr|tts|bridge)' >/dev/null; then
  rm -f "$RUNTIME_DIR/.stopped"
  echo '仍有 mozart 进程残留，停止部署：' >&2
  pgrep -af 'mozart-(daemon|pre|rvc|asr|tts|bridge)' >&2 || true
  exit 1
fi

echo '-- 备份当前二进制 --'
rm -rf "$RUNTIME_DIR/bin.prev.new"
mkdir -p "$RUNTIME_DIR/bin.prev.new"
if [ -d "$RUNTIME_DIR/bin" ]; then
  if ! cp -a "$RUNTIME_DIR/bin/." "$RUNTIME_DIR/bin.prev.new/"; then
    rm -rf "$RUNTIME_DIR/bin.prev.new"
    rm -f "$RUNTIME_DIR/.stopped"
    "$CTL" --config "$CFG" up >/dev/null 2>&1 || true
    echo '备份当前二进制失败，已尝试重启旧栈' >&2
    exit 1
  fi
fi
rm -rf "$RUNTIME_DIR/bin.prev"
if ! mv "$RUNTIME_DIR/bin.prev.new" "$RUNTIME_DIR/bin.prev"; then
  rm -f "$RUNTIME_DIR/.stopped"
  "$CTL" --config "$CFG" up >/dev/null 2>&1 || true
  echo '替换 bin.prev 失败，已尝试重启旧栈' >&2
  exit 1
fi
touch "$RUNTIME_DIR/.installing"

echo '-- 安装二进制 --'
mkdir -p "$RUNTIME_DIR/bin/examples"

INSTALLED=""
for b in ctl daemon pre rvc asr tts bridge; do
  src="target/debug/mozart-$b"
  [ -x "$src" ] || fail_install "缺构建产物：$src"
  install -m 755 "$src" "$RUNTIME_DIR/bin/mozart-$b" || fail_install "安装失败：mozart-$b"
  INSTALLED="$INSTALLED mozart-$b"
done
[ -x target/debug/examples/rvc_stub ] || fail_install '缺构建产物：target/debug/examples/rvc_stub'
install -m 755 target/debug/examples/rvc_stub "$RUNTIME_DIR/bin/examples/rvc_stub" \
  || fail_install '安装失败：examples/rvc_stub'
INSTALLED="$INSTALLED examples/rvc_stub"

echo '-- 重启运行时 --'
"$CTL" --config "$CFG" up || fail_install '运行时启动失败'

health=0
for i in $(seq 1 90); do
  if "$CTL" --config "$CFG" status --json > "$STATUS_JSON" 2>/dev/null; then
    if python3 - "$STATUS_JSON" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
rvc = d.get("rvc")
ok = bool(d.get("pre")) and (rvc is None or bool(rvc.get("state")))
sys.exit(0 if ok else 1)
PY
    then
      health=1
      echo "健康检查通过（${i} s）"
      break
    fi
  fi
  sleep 1
done
if [ "$health" -ne 1 ]; then
  "$CTL" --config "$CFG" status || true
  fail_install '健康检查超时或未通过'
fi

if [ "$DIRTY" = "1" ]; then dirty_json=true; else dirty_json=false; fi
printf '{"commit":"%s","dirty":%s,"time":"%s","preset":"jetson-release","bins":"%s"}\n' \
  "$COMMIT" "$dirty_json" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${INSTALLED# }" \
  > "$RUNTIME_DIR/deploy.json" || fail_install '写 deploy.json 失败'
rm -f "$RUNTIME_DIR/.installing"
rm -f "$RUNTIME_DIR/.stopped"
trap - ERR
echo '安装与重启完成'
REMOTE

# 5 摘要。
STEP='5/5 摘要'
echo "== $STEP =="
if [ -n "${MOZART_RUNTIME_DIR:-}" ]; then
  RUNTIME_LABEL=$MOZART_RUNTIME_DIR
else
  RUNTIME_LABEL='~/mozart-runtime'
fi
echo "部署成功（耗时 ${SECONDS} s）。"
echo "源码：~/Mozart（commit $COMMIT，dirty=$DIRTY）"
echo "运行位：$RUNTIME_LABEL/bin（上一代：bin.prev）"
echo "记录：$RUNTIME_LABEL/deploy.json"
echo "功能测试：tools/jetson-e2e.sh $TARGET"
