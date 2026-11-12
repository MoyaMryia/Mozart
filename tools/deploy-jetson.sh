#!/usr/bin/env bash
# Jetson 一键部署 + 测试 + 失败回滚。本机执行，构建与测试在目标设备上完成，
# 构建产物不回传本机。
#   tools/deploy-jetson.sh [SSH目标] [--smoke]
#
# SSH 目标缺省 moyamryia@10.0.0.4，环境变量 MOZART_JETSON 覆盖缺省。地址
# 变化由本参数或 ~/.ssh/config 别名解决；脚本不做地址管理，不做连接重试，
# 连不上立即报错并以非零退出。
#
# 流程（2 的 rsync 在本机，其余动作在远端 ssh 内，输出原样透传）：
#   1 部署前快照（远端本地盘拷贝）：~/.mozart-prev 存在时，把 ~/Mozart 源码
#     （不含 target/build）拷入 ~/.mozart-prev，作为本次部署的回滚锚点；
#     首次运行 ~/.mozart-prev 不存在则跳过快照——没有可回滚的上一代。
#   2 源码同步：本机仓库 → 远端 ~/Mozart（rsync -a --delete，排除
#     target/build/.git）；随后归一增量构建的新鲜度：-a 保留本机旧 mtime，
#     远端 target/、build/ 里 mtime 更新的旧产物会让构建器误判"源码未变"
#     跳过重编，部署出落后于源码的陈旧二进制并带病通过测试门（实测暴露）。
#     ninja 按 mtime 判定，把同步树全部文件 mtime 前拨即强制重编；cargo
#     的指纹判定在旧 mtime 场景下实测不可依赖，删除工作区各包的指纹逼其
#     重编，注册表第三方依赖保持缓存。指纹名单按 workspace 成员推导，
#     新增 crate 自动纳入。每次部署为全量重编。
#   3 cargo build --workspace --bins --examples（debug）。--bins 显式产出部署
#     二进制：单 --examples 只编示例不编 bin；且 pre 无集成测试，其 bin 不在
#     cargo test 的默认构建集里，靠测试门顺带产出不可靠（实测陈旧 pre 带病
#     通过全部单测）。
#   4 cmake --preset jetson-release 构建，随后把 build/jetson-release/rvc/
#     mozart-rvc 对齐复制到 target/debug——daemon 按自身可执行文件同目录
#     解析子进程（SPEC-04 部署布局）。对齐先于测试门执行：
#     jetson-smoke.sh 的前置要求 BIN_DIR 内已有 mozart-rvc。
#   5 测试门：cargo test --workspace；再跑 ctest --test-dir
#     build/jetson-release（CMakePresets.json 中 jetson-release 的产物目录），
#     无注册用例则跳过并在输出注明。
#   6 [--smoke] 三进程冒烟 tools/jetson-smoke.sh。BIN_DIR 由本脚本固定为
#     ~/Mozart/target/debug，不透传本机同名变量；本机设置了
#     MODELS_DIR/CAP_SUB/PB_SUB 时透传，其余用脚本内置缺省。冒烟自带
#     进程残留清理。
#   7 成功：~/.mozart-prev 保留为上一代回滚锚点（首次成功部署时建立），
#     输出部署摘要。
#
# 任一步失败（含 Ctrl-C 中断）：远端清理 mozart 进程与 /tmp/mozart-smoke
# 测试残留，恢复部署前快照源码并清掉部署位二进制（回滚只还源码不还产物，
# 旧源码配新二进制是错配态，留着会让手工拉起跑上未验收产物），明确输出
# 回滚结果与失败步骤，退出码 1。回滚自身失败时如实输出。脚本幂等可重复。
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${MOZART_JETSON:-moyamryia@10.0.0.4}
SMOKE=0
SSH_OPTS=(-o ConnectTimeout=10)
SMOKE_DIR=/tmp/mozart-smoke

usage() {
  echo "用法：tools/deploy-jetson.sh [SSH目标] [--smoke]" >&2
  echo "  SSH目标 缺省 moyamryia@10.0.0.4，环境变量 MOZART_JETSON 覆盖缺省。" >&2
  echo "  --smoke 测试门通过后追加三进程冒烟（tools/jetson-smoke.sh）。" >&2
}

for arg in "$@"; do
  case "$arg" in
    --smoke) SMOKE=1 ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "未知参数：$arg" >&2; usage; exit 2 ;;
    *) TARGET=$arg ;;
  esac
done

STEP=参数解析
ROLLED_BACK=0

# ---- 回滚：远端清理运行残留 + 恢复快照源码，结果如实输出。不依赖 set -e。----
rollback() {
  ROLLED_BACK=1
  trap - INT TERM EXIT
  echo >&2
  echo "== 回滚（失败步骤：$STEP）==" >&2
  if ssh "${SSH_OPTS[@]}" "$TARGET" bash -s <<'ROLLBACK'
rc=0
echo '-- 运行时清理 --'
# 冒烟拉起的栈先尝试优雅关停（daemon socket 还在时），再按 jetson-smoke.sh
# 的清理段强制收尾。路径与 jetson-smoke.sh 的 SMOKE_DIR 缺省一致。
if [ -S /tmp/mozart-smoke/run/daemon.sock ] && [ -x "$HOME/Mozart/target/debug/mozart-ctl" ]; then
  "$HOME/Mozart/target/debug/mozart-ctl" --config /tmp/mozart-smoke/smoke.toml down 2>&1 \
    || { echo 'mozart-ctl down 失败，继续强制清理'; rc=1; }
fi
pkill -f mozart-daemon 2>/dev/null || true
pkill -f 'mozart-pre --socket' 2>/dev/null || true
pkill -f 'mozart-rvc --socket' 2>/dev/null || true
sleep 0.3
if pgrep -f 'mozart-(daemon|pre|rvc)' >/dev/null; then
  echo '警告：仍有 mozart 进程残留：' >&2
  pgrep -af 'mozart-(daemon|pre|rvc)' >&2 || true
  rc=1
else
  echo '无 mozart 进程残留'
fi
rm -rf /tmp/mozart-smoke 2>/dev/null || true
echo '-- 源码恢复 --'
if [ -d "$HOME/.mozart-prev" ]; then
  if rsync -a --delete --exclude target --exclude build \
       "$HOME/.mozart-prev/" "$HOME/Mozart/"; then
    echo '部署已回滚：~/Mozart 源码已恢复为部署前快照（~/.mozart-prev）'
  else
    echo '回滚失败：快照恢复 rsync 出错，~/Mozart 保持当前状态' >&2
    rc=1
  fi
else
  echo '本次部署前未做快照（首次运行 ~/.mozart-prev 不存在），源码保持同步后的状态'
fi
echo '-- 部署位二进制清理 --'
# 快照只含源码：恢复后 target/ 里留下的是本次失败构建的产物，旧源码配
# 新二进制是错配态，清掉部署位防止手工拉起跑上未验收二进制。
rm -f "$HOME/Mozart/target/debug/mozart-daemon" \
      "$HOME/Mozart/target/debug/mozart-pre" \
      "$HOME/Mozart/target/debug/mozart-ctl" \
      "$HOME/Mozart/target/debug/mozart-rvc" 2>/dev/null \
  && echo '部署位二进制已清（target/debug 下四个 mozart-*）' \
  || { echo '部署位二进制清理失败（目录可能不存在），产物残留注意甄别' >&2; rc=1; }
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

echo "== Jetson 部署：$TARGET（冒烟 $([ "$SMOKE" = 1 ] && echo 开 || echo 关)）=="

# 1 部署前快照。
STEP='1/7 部署前快照'
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

# 2 源码同步。
STEP='2/7 源码同步（本机 → 远端）'
echo "== $STEP =="
rsync -a --delete -e "ssh -o ConnectTimeout=10" \
  --exclude target --exclude build --exclude .git \
  "$REPO/" "$TARGET":~/Mozart/
# rsync -a 保留本机文件的旧 mtime。远端 target/、build/ 里 mtime 更新的旧
# 产物会让增量构建器误判“源码未变”跳过重编，部署出落后于源码的陈旧二进制
# 并带病通过测试门（实测：pre 旧二进制缺 config.rs 的 [rvc] 新契约字段，
# 单测全绿，冒烟才暴露）。ninja 按 mtime 判定，前拨全部源文件 mtime 即可
# 强制重编；cargo 的指纹判定在旧 mtime 场景下实测不可依赖（touch 后仍判
# fresh），直接删除工作区各包指纹逼其重编，注册表第三方依赖保持缓存。
# 指纹名单按 workspace 成员推导——硬编码名单会在新增 crate 时静默失效。
ssh "${SSH_OPTS[@]}" "$TARGET" bash -s <<'REMOTE'
set -euo pipefail
cd ~/Mozart
find . \( -path ./target -o -path ./build \) -prune \
  -o -type f -exec touch {} +
for name in $(awk '/^members *=/ {sub(/^members *= *\[/, ""); sub(/\]$/, ""); gsub(/[" ,]/, " "); print}' Cargo.toml); do
  rm -rf "target/debug/.fingerprint/${name}-*"
done
REMOTE
echo '新鲜度归一完成（mtime 前拨 + cargo 指纹重置）'

# 3–7 远端构建、测试门、对齐与摘要。
STEP='3/7–7/7 远端构建与测试'
remote_env="SMOKE=$SMOKE"
if [ -n "${MODELS_DIR:-}" ]; then
  remote_env="$remote_env MODELS_DIR=$(printf '%q' "$MODELS_DIR")"
fi
if [ -n "${CAP_SUB:-}" ]; then
  remote_env="$remote_env CAP_SUB=$(printf '%q' "$CAP_SUB")"
fi
if [ -n "${PB_SUB:-}" ]; then
  remote_env="$remote_env PB_SUB=$(printf '%q' "$PB_SUB")"
fi
echo "== $STEP（$TARGET）=="
ssh "${SSH_OPTS[@]}" "$TARGET" "$remote_env bash -s" <<'REMOTE'
set -euo pipefail
cd "$HOME/Mozart"
. "$HOME/.cargo/env"

echo '== 3/7 cargo build --workspace --bins --examples（debug）=='
cargo build --workspace --bins --examples

echo '== 4/7 cmake jetson-release =='
cmake --preset jetson-release
cmake --build --preset jetson-release --parallel 2

echo '== 4/7 rvc 产物对齐 target/debug（SPEC-04 部署布局）=='
cp -f build/jetson-release/rvc/mozart-rvc target/debug/

echo '== 5/7 测试门：cargo test --workspace =='
cargo test --workspace
# ctest 用例数清点：查询失败与无用例是两种状态，混为一个跳过分支会让工具
# 故障被改写成"无用例"。
if ctest_list=$(ctest --test-dir build/jetson-release -N 2>&1); then
  if grep -q 'Test #' <<<"$ctest_list"; then
    echo '== 5/7 测试门：ctest（jetson-release）=='
    ctest --test-dir build/jetson-release
  else
    echo 'jetson-release 无注册 ctest 用例，跳过 ctest'
  fi
else
  echo 'ctest 清点失败（jetson-release 未配置或工具故障），视为测试门失败' >&2
  exit 1
fi

if [ "${SMOKE:-0}" = "1" ]; then
  echo '== 6/7 三进程冒烟 jetson-smoke.sh =='
  BIN_DIR="$HOME/Mozart/target/debug" tools/jetson-smoke.sh
fi

echo '== 7/7 部署完成对齐 =='
if [ ! -d "$HOME/.mozart-prev" ]; then
  rsync -a --delete --exclude target --exclude build "$HOME/Mozart/" "$HOME/.mozart-prev/"
  echo '已建立回滚锚点 ~/.mozart-prev（首次成功部署）'
else
  echo '~/.mozart-prev 保留为上一代回滚锚点'
fi
echo '部署完成。target/debug 产物：'
ls -l target/debug/mozart-daemon target/debug/mozart-pre \
      target/debug/mozart-ctl target/debug/mozart-rvc
exit 0
REMOTE

echo "部署成功（耗时 ${SECONDS} s）。"
