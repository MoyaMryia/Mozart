#!/usr/bin/env bash
# 主机构建网页；设备构建运行时。部署不跑测试。
# 默认保留构建缓存，--full 清理构建缓存。构建期间暂停整栈以释放共享内存。
# 用法：tools/deploy-jetson.sh [--full] <SSH目标>
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${MOZART_JETSON:-}
FULL=0
POSITIONAL=0
for argument in "$@"; do
  case "$argument" in
    --full) FULL=1 ;;
    --help|-h) echo '用法：tools/deploy-jetson.sh [--full] <SSH目标>'; exit 0 ;;
    -*) echo "未知参数：$argument" >&2; exit 2 ;;
    *) [ "$POSITIONAL" -eq 0 ] || { echo '只能指定一个 SSH 目标' >&2; exit 2; }; TARGET=$argument; POSITIONAL=1 ;;
  esac
done
[ -n "$TARGET" ] || { echo '请提供 SSH 目标或 MOZART_JETSON' >&2; exit 2; }
for tool in ssh rsync npm; do command -v "$tool" >/dev/null || { echo "缺少 $tool" >&2; exit 1; }; done
COMMIT=$(git -C "$REPO" rev-parse HEAD)
DIRTY=false
[ -z "$(git -C "$REPO" status --porcelain)" ] || DIRTY=true
SSH=(ssh -o ConnectTimeout=10 "$TARGET")
ENVIRONMENT="MOZART_RUNTIME_DIR=$(printf %q "${MOZART_RUNTIME_DIR:-}")"

echo '== 1/5 构建网页（主机）=='
npm --prefix "$REPO/web" ci
npm --prefix "$REPO/web" run build

# 在改动前建立事务记录。回滚脚本不依赖可能已被替换的源码。
TRANSACTION=$("${SSH[@]}" "$ENVIRONMENT bash -s" <<'INIT'
set -euo pipefail
root=${MOZART_RUNTIME_DIR:-$HOME/mozart-runtime}
[ -f "$root/mozart.toml" ] || { echo "缺少 $root/mozart.toml" >&2; exit 1; }
for tool in rsync jq cargo cmake ninja flock; do
  if [ "$tool" = cargo ]; then . "$HOME/.cargo/env"; fi
  command -v "$tool" >/dev/null || { echo "缺少 $tool" >&2; exit 1; }
done
transaction=$(mktemp -d /tmp/mozart-deploy.XXXXXXXX)
lock="$HOME/.mozart-deploy.lock"
mkdir "$lock" || { echo "另一次部署或未恢复事务占用 $lock，请先检查" >&2; exit 1; }
printf '%s\n' "$transaction" > "$lock/owner"
trap 'rm -f "$lock/owner"; rmdir "$lock"' ERR
printf '%s\n' "$root" > "$transaction/runtime"
printf '%s\n' "$lock" > "$transaction/lock"
if [ -x "$root/bin/mozart-ctl" ]; then cp -a "$root/bin/mozart-ctl" "$transaction/ctl-before"; fi
if [ -f "$root/deploy.json" ]; then cp -a "$root/deploy.json" "$transaction/deploy-before.json"; fi
cat > "$transaction/rollback.sh" <<'ROLLBACK'
#!/usr/bin/env bash
set -euo pipefail
transaction=$1
read -r root < "$transaction/runtime"
ctl="$root/bin/mozart-ctl"
web="$root/bin/mozart-web-service"
cfg="$root/mozart.toml"
read -r lock < "$transaction/lock"
read -r owner < "$lock/owner"
[ "$owner" = "$transaction" ] || { echo '部署锁不属于本事务，拒绝回滚' >&2; exit 1; }
if [ -f "$transaction/installing" ]; then
  stop_ctl=$ctl
  if [ -x "$transaction/ctl-before" ]; then stop_ctl="$transaction/ctl-before"; fi
  if [ -x "$stop_ctl" ]; then
    if ! "$stop_ctl" --config "$cfg" down; then
      if pgrep -x mozart-daemon; then echo '无法停止新栈，保留回滚记录' >&2; exit 1; fi
    fi
  fi
  # 硬链接保留文件 capability；普通用户的复制/rsync 不能保证恢复 security.capability。
  restored=$(mktemp -d "$root/.bin-restore.XXXXXXXX")
  cp -al "$root/bin.prev/." "$restored/"
  failed=$(mktemp -d "$root/.bin-failed.XXXXXXXX")
  rmdir "$failed"
  mv "$root/bin" "$failed"
  if ! mv "$restored" "$root/bin"; then mv "$failed" "$root/bin"; exit 1; fi
  rsync -a --checksum --delete "$root/web.prev/" "$root/web/"
fi
if [ -f "$transaction/source-existed" ]; then
  [ ! -L "$HOME/Mozart" ] && [ -d "$HOME/Mozart" ] || { echo '源码目录不安全，拒绝回滚' >&2; exit 1; }
  read -r snapshot < "$transaction/source-backup-path"
  rsync -a --checksum --delete --exclude target --exclude build --exclude .git --exclude node_modules "$snapshot/" "$HOME/Mozart/"
  # 恢复旧 mtime 后，不能继续信任新源码构建的产物。
  (cd "$HOME/Mozart"; . "$HOME/.cargo/env"; cargo clean)
  if [ -f "$HOME/Mozart/build/jetson-release/build.ninja" ]; then
    ninja -C "$HOME/Mozart/build/jetson-release" -t clean
  fi
elif [ -f "$transaction/source-created-id" ]; then
  read -r source_id < "$transaction/source-created-id"
  if [ -e "$HOME/Mozart" ] || [ -L "$HOME/Mozart" ]; then
    [ ! -L "$HOME/Mozart" ] && [ -d "$HOME/Mozart" ] &&
      [ "$(stat -c '%d:%i' "$HOME/Mozart")" = "$source_id" ] &&
      [ ! -e "$transaction/failed-source" ] && [ ! -L "$transaction/failed-source" ] || {
        echo '源码目录已被替换或备份目标不安全，拒绝回滚' >&2; exit 1;
      }
    mv -T "$HOME/Mozart" "$transaction/failed-source"
  fi
else
  echo '缺少源码事务记录，拒绝回滚' >&2; exit 1
fi
if [ -f "$transaction/stopped" ] && [ -f "$transaction/was-running" ]; then
  "$ctl" --config "$cfg" up
fi
if [ -x "$web" ] && [ -f "$transaction/web-stopped" ] && [ -f "$transaction/web-was-running" ]; then
  "$web" "$root" start
fi
if [ -f "$transaction/deploy-before.json" ]; then cp -a "$transaction/deploy-before.json" "$root/deploy.json";
elif [ -f "$transaction/installing" ]; then rm -f "$root/deploy.json"; fi
rm -f "$lock/owner"
rmdir "$lock"
printf '部署前源码与运行状态已恢复；事务记录：%s\n' "$transaction"
ROLLBACK
if [ -L "$HOME/Mozart" ] || { [ -e "$HOME/Mozart" ] && [ ! -d "$HOME/Mozart" ]; }; then
  echo '源码目标不是安全的目录' >&2; exit 1
fi
if [ -d "$HOME/Mozart" ]; then
  snapshot=$(mktemp -d "$HOME/.mozart-source-backup.XXXXXXXX")
  rsync -a --checksum --exclude target --exclude build --exclude .git --exclude node_modules "$HOME/Mozart/" "$snapshot/"
  printf '%s\n' "$snapshot" > "$transaction/source-backup-path"
  mkdir -p "$HOME/.mozart-prev"
  rsync -a --checksum --delete "$snapshot/" "$HOME/.mozart-prev/"
  touch "$transaction/source-existed"
else
  # Publish only after recording identity; failed initialization leaves the original path absent.
  fresh=$(mktemp -d "$HOME/.mozart-source-new.XXXXXXXX")
  stat -c '%d:%i' "$fresh" > "$transaction/source-created-id"
  mv -T -n "$fresh" "$HOME/Mozart"
  [ ! -e "$fresh" ] || { echo '源码目标已被其他操作创建，拒绝覆盖' >&2; exit 1; }
fi
trap - ERR
printf '%s\n' "$transaction"
INIT
)
rollback() {
  local result=$?
  trap - EXIT INT TERM
  if [ "$result" -ne 0 ]; then
    echo "部署失败，开始回滚：$TRANSACTION" >&2
    if ! "${SSH[@]}" "bash $(printf %q "$TRANSACTION/rollback.sh") $(printf %q "$TRANSACTION")"; then
      echo "回滚失败，请保留并检查事务记录：$TRANSACTION" >&2
    fi
  fi
  exit "$result"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo '== 2/5 同步源码并停止运行时 =='
rsync -a --checksum --no-times --delete -e 'ssh -o ConnectTimeout=10' \
  --exclude target --exclude build --exclude .git --exclude node_modules "$REPO/" "$TARGET":~/Mozart/
"${SSH[@]}" "TRANSACTION=$(printf %q "$TRANSACTION") bash -s" <<'STOP'
set -euo pipefail
read -r root < "$TRANSACTION/runtime"
ctl="$root/bin/mozart-ctl"
web="$root/bin/mozart-web-service"
if [ -x "$web" ] && "$web" "$root" status >/dev/null; then
  touch "$TRANSACTION/web-was-running" "$TRANSACTION/web-stopped"
  "$web" "$root" stop
fi
: > "$TRANSACTION/status-before.json"
if [ -x "$ctl" ] && "$ctl" --config "$root/mozart.toml" status --json > "$TRANSACTION/status-before.tmp"; then
  mv "$TRANSACTION/status-before.tmp" "$TRANSACTION/status-before.json"
  touch "$TRANSACTION/was-running" "$TRANSACTION/stopped"
  "$ctl" --config "$root/mozart.toml" down
fi
# 只检查实际进程名，不匹配包含命令文本的 ssh/bash 进程；Linux comm 限 15 字节，mozart-web-adapter 截断为 mozart-web-adap。
for name in mozart-daemon mozart-pre mozart-enhance mozart-rvc mozart-asr mozart-tts mozart-bridge mozart-web-adap llama-server; do
  if pgrep -x "$name"; then echo "仍有活跃进程，停止构建：$name" >&2; exit 1; fi
done
STOP

echo '== 3/5 构建设备程序 =='
"${SSH[@]}" "FULL=$FULL bash -s" <<'BUILD'
set -euo pipefail
. "$HOME/.cargo/env"
cd "$HOME/Mozart"
assets=${MOZART_ASSETS_DIR:-$HOME/mozart-assets}
if [ -e "$assets/lib/libsherpa-onnx-c-api.so" ]; then export SHERPA_ONNX_LIB_DIR="$assets/lib"; fi
export LD_LIBRARY_PATH="$assets/lib:$assets/llama${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
if [ "$FULL" = 1 ]; then cargo clean; fi
cargo build --workspace --bins --examples --jobs 2 --locked
cmake --preset jetson-release -DFETCHCONTENT_UPDATES_DISCONNECTED=ON
if [ "$FULL" = 1 ]; then
  cmake --build --preset jetson-release --parallel 2 --clean-first
else
  cmake --build --preset jetson-release --parallel 2
fi
cp build/jetson-release/rvc/mozart-rvc target/debug/mozart-rvc
cp build/jetson-release/enhance/mozart-enhance target/debug/mozart-enhance
BUILD

echo '== 4/5 安装并启动 =='
"${SSH[@]}" "TRANSACTION=$(printf %q "$TRANSACTION") COMMIT=$(printf %q "$COMMIT") DIRTY=$DIRTY FULL=$FULL bash -s" <<'INSTALL'
set -euo pipefail
read -r root < "$TRANSACTION/runtime"
cd "$HOME/Mozart"
for name in ctl daemon pre enhance rvc asr tts bridge web-adapter; do
  [ -x "target/debug/mozart-$name" ] || { echo "缺少 mozart-$name" >&2; exit 1; }
done
[ -x target/debug/examples/rvc_stub ]
[ -f web/dist/index.html ]
[ "$(id -u)" -ne 0 ] || { echo '请以运行目录所有者而非 root 运行部署' >&2; exit 1; }
# 安装只管理 SETUP 定义的资源目录，不覆盖任意配置路径。
description=$(target/debug/mozart-web-adapter --config "$root/mozart.toml" --print-config)
[ "$(jq -er '.configured' <<< "$description")" = true ] || { echo '部署需要 mozart.toml 配置 [web] 段（由 daemon 监管 web-adapter）' >&2; exit 1; }
port=$(jq -er '.port' <<< "$description")
static=$(jq -er '.static_dir' <<< "$description")
[ "$(realpath -m "$static")" = "$(realpath -m "$root/web/dist")" ] || { echo '部署仅管理 runtime/web/dist；请设置相应 web.static_dir' >&2; exit 1; }
mkdir -p "$root/bin" "$root/web"
# 先完整备份，再标记安装开始。备份失败时当前运行位仍未修改。
bin_backup=$(mktemp -d "$root/.bin-backup.XXXXXXXX")
web_backup=$(mktemp -d "$root/.web-backup.XXXXXXXX")
cp -al "$root/bin/." "$bin_backup/"
cp -a "$root/web/." "$web_backup/"
rm -rf "$root/bin.prev" "$root/web.prev"
mv "$bin_backup" "$root/bin.prev"
mv "$web_backup" "$root/web.prev"
touch "$TRANSACTION/installing"
install_binary() {
  install -m 755 "$1" "$2.new"
  mv -f "$2.new" "$2"
}
for name in ctl daemon pre enhance rvc asr tts bridge web-adapter; do
  install_binary "target/debug/mozart-$name" "$root/bin/mozart-$name"
done
mkdir -p "$root/bin/examples" "$root/web/dist"
install_binary target/debug/examples/rvc_stub "$root/bin/examples/rvc_stub"
rm -f "$root/bin/mozart-web-service"
rsync -a --checksum --delete web/dist/ "$root/web/dist/"
if [ "$port" -gt 0 ] && [ "$port" -lt 1024 ]; then
  if ! getcap "$root/bin/mozart-web-adapter" | grep -q 'cap_net_bind_service=ep'; then
    command -v sudo >/dev/null || { echo 'low HTTP port requires cap_net_bind_service or a configured high port' >&2; exit 1; }
    sudo -n setcap cap_net_bind_service=ep "$root/bin/mozart-web-adapter" || { echo 'grant cap_net_bind_service, or configure a high web.listen port' >&2; exit 1; }
  fi
fi
"$root/bin/mozart-ctl" --config "$root/mozart.toml" up
# 包括原本正常的可选服务；不把部署前已降级的服务冒充本次部署失败。
healthy=0
for attempt in $(seq 1 90); do
  if "$root/bin/mozart-ctl" --config "$root/mozart.toml" status --json > "$TRANSACTION/status-after.json"; then
    if jq -e --slurpfile previous "$TRANSACTION/status-before.json" '
      (.processes.web.state == "running") and
      (.pre != null) and (.rvc == null or (.rvc.state | length > 0)) and
      (if .pre.denoise_backend == "ulunas" then
        .enhance.backend == "ulunas" and
        ( .pre.enhancement.enabled == false or
          (.pre.enhancement.connected == true and .pre.enhancement.last_error == null))
       else true end) and
      (if ($previous | length) == 0 then true else
        . as $current | all($previous[0].processes | to_entries[]; .value.state != "running" or $current.processes[.key].state == "running")
      end)' "$TRANSACTION/status-after.json" >/dev/null; then healthy=1; break; fi
  fi
  sleep 1
done
[ "$healthy" = 1 ] || { echo '运行时健康检查失败' >&2; exit 1; }
mode=full
jq -n --arg commit "$COMMIT" --argjson dirty "$DIRTY" --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg mode "$mode" \
  '{commit:$commit,dirty:$dirty,time:$time,preset:"jetson-release",mode:$mode,restarted:"all"}' > "$root/deploy.json.new"
mv "$root/deploy.json.new" "$root/deploy.json"
touch "$TRANSACTION/completed"
read -r lock < "$TRANSACTION/lock"
rm -f "$lock/owner"
rmdir "$lock"
INSTALL
trap - EXIT INT TERM
echo "== 5/5 部署完成（${SECONDS} s）=="
echo "源码：~/Mozart（$COMMIT，dirty=$DIRTY）；构建期间已暂停整栈。"
echo '运行位：mozart-runtime/bin 和 web/dist；上一代：bin.prev 和 web.prev。'
echo "事务记录：$TRANSACTION；功能验收：tools/jetson-e2e.sh $TARGET"
