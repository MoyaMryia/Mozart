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
# 阶段计时：只打印耗时，不改变部署行为。
STAGE_START=$SECONDS
stage() { echo "-- 耗时 $1：$((SECONDS - STAGE_START)) s"; STAGE_START=$SECONDS; }

echo '== 1/5 构建网页（主机）=='
# 按需 npm ci：依赖未变时跳过安装，只跑构建（SPEC-09）。
#  stamp 记录上次安装所用的 lock 摘要；--full 或 node_modules 缺失时强制重装。
lock_hash=$(sha256sum "$REPO/web/package-lock.json" | cut -d' ' -f1)
stamp="$REPO/web/node_modules/.mozart-npm-ci.sha256"
if [ "$FULL" = 1 ] || [ ! -d "$REPO/web/node_modules" ] ||
  [ ! -f "$stamp" ] || [ "$(cat "$stamp")" != "$lock_hash" ]; then
  npm --prefix "$REPO/web" ci
  printf '%s\n' "$lock_hash" > "$stamp"
else
  echo '依赖未变，跳过 npm ci'
fi
npm --prefix "$REPO/web" run build
stage "1/5 网页"

# 在改动前建立事务记录。回滚脚本不依赖可能已被替换的源码。
# coproc 的管道 fd 不传给部署子命令。本机 shell 退出即关闭写端。
remote_lock='"$HOME/.mozart-deploy.lock"'
holder=$(cat <<'HOLDER'
set -eu
lock=$1
# 交接前排除上个持锁会话断开时仍在执行的远端阶段。
exec 8>>"$lock.stage"
take_lock() {
  flock -n "$1" || {
    rc=$?
    if [ "$rc" = 1 ]; then echo "$2" >&2; else echo "flock fd $1 失败（退出码 $rc）" >&2; fi
    exit "$rc"
  }
}
take_lock 8 '仍有部署阶段在执行'
if [ -d "$lock" ]; then
  [ ! -e "$lock/owner" ] || { echo "历史部署锁含 owner，请先检查 $lock" >&2; exit 1; }
  rmdir "$lock" || exit 1
fi
exec 9>>"$lock"
take_lock 9 "另一次部署占用 $lock"
transaction=$(mktemp -d /tmp/mozart-deploy.XXXXXXXX)
printf '%s\n' "$lock" "$$" > "$transaction/lock-owner"
printf '%s\n' "$transaction" > "$lock"
trap ': > "$lock"' EXIT
cat > "$transaction/run" <<'RUN'
set -eu
transaction=$(dirname "$0")
read -r lock < "$transaction/lock-owner"
[ "$transaction" = "$(<"$lock")" ] || { echo '部署事务已更换' >&2; exit 1; }
if flock -n "$lock" true; then
  echo '部署会话锁已丢失，拒绝执行下一阶段' >&2; exit 1
else
  rc=$?
  [ "$rc" = 1 ] || { echo "部署锁检查失败（退出码 $rc）" >&2; exit "$rc"; }
fi
exec "$@"
RUN
exec 8>&-
printf 'LOCKED %s\n' "$transaction"
if ! read -r action || [ "$action" = rollback ]; then
  exec 8>>"$lock.stage"
  flock 8
  if [ -e "$transaction/completed" ]; then
    if [ "${action:-}" = rollback ]; then echo "新栈已发布，不回滚；请检查事务清理：$transaction" >&2; fi
  elif [ -f "$transaction/initialized" ]; then
    bash "$transaction/rollback.sh" "$transaction" 8>&- >&2
  fi
fi
HOLDER
)
coproc LOCK { exec ssh -T -o ControlMaster=no -o ControlPath=none -o ServerAliveInterval=10 -o ServerAliveCountMax=3 -o ConnectTimeout=10 "$TARGET" "bash -c $(printf %q "$holder") holder $remote_lock"; }
lock_pid=$LOCK_PID
read -r reply TRANSACTION <&"${LOCK[0]}"
[ "$reply" = LOCKED ]
lock_in=${LOCK[1]}
access_host=$(ssh -G "$TARGET" | awk '$1 == "hostname" {print $2}')
guard="flock -o ${remote_lock}.stage bash $(printf %q "$TRANSACTION/run")"
"${SSH[@]}" "$ENVIRONMENT transaction=$(printf %q "$TRANSACTION") $guard bash -s" <<'INIT'
set -euo pipefail
root=${MOZART_RUNTIME_DIR:-$HOME/mozart-runtime}
[ -f "$root/mozart.toml" ] || { echo "缺少 $root/mozart.toml" >&2; exit 1; }
for tool in rsync jq cargo cmake ninja curl; do
  if [ "$tool" = cargo ]; then . "$HOME/.cargo/env"; fi
  command -v "$tool" >/dev/null || { echo "缺少 $tool" >&2; exit 1; }
done
printf '%s\n' "$root" > "$transaction/runtime"
if [ -x "$root/bin/mozart-ctl" ]; then cp -a "$root/bin/mozart-ctl" "$transaction/ctl-before"; fi
if [ -f "$root/deploy.json" ]; then cp -a "$root/deploy.json" "$transaction/deploy-before.json"; fi
cat > "$transaction/rollback.sh" <<'ROLLBACK'
#!/usr/bin/env bash
set -euo pipefail
transaction=$1
{ read -r lock; read -r holder_pid; } < "$transaction/lock-owner"
read -r root < "$transaction/runtime"
ctl="$root/bin/mozart-ctl"
web="$root/bin/mozart-web-service"
cfg="$root/mozart.toml"
# fd 9 由持锁会话继承；独立调用不能绕过互斥。
[ "$PPID" = "$holder_pid" ] && [ /proc/self/fd/9 -ef "$lock" ] && grep -Eq '^lock:.*FLOCK[[:space:]]+ADVISORY[[:space:]]+WRITE ' /proc/self/fdinfo/9 &&
  [ "$(<"$lock")" = "$transaction" ] &&
  [ -f "$transaction/initialized" ] && [ ! -e "$transaction/completed" ] || {
    echo '锁或事务记录不匹配，拒绝回滚' >&2; exit 1;
  }
if [ -f "$transaction/source-existed" ]; then
  read -r snapshot < "$transaction/source-backup-path"
  [[ $snapshot == "$HOME"/.mozart-source-backup.???????? && -d $snapshot && ! -L $snapshot ]] || {
    echo '源码快照缺失或路径无效，拒绝回滚' >&2; exit 1;
  }
elif [ ! -s "$transaction/source-created-id" ]; then
  echo '缺少源码事务记录，拒绝回滚' >&2; exit 1
fi
if [ -f "$transaction/installing" ]; then
  [ -d "$root/bin.prev" ] && [ -d "$root/web.prev" ] || { echo '缺少安装备份，拒绝回滚' >&2; exit 1; }
fi
# 父会话持锁直至本脚本退出；不把锁 fd 传给恢复的服务。
exec 9>&-
systemd_matches_root() {
  local sctl wd
  sctl=$(command -v systemctl || true)
  [ -n "$sctl" ] || return 1
  wd=$("$sctl" show -p WorkingDirectory --value mozart.service 2>/dev/null || true)
  [ -n "$wd" ] && [ "$(realpath -m "$wd")" = "$(realpath -m "$root")" ]
}
systemd_state() {
  local sctl
  sctl=$(command -v systemctl || true)
  [ -n "$sctl" ] || return 0
  "$sctl" is-active mozart.service 2>/dev/null || true
}
if [ -f "$transaction/installing" ]; then
  sctl=$(command -v systemctl || true)
  svc_state=$(systemd_state)
  if [ -n "$sctl" ] && systemd_matches_root && {
    case "$svc_state" in active|activating|reloading|deactivating) true ;; *) [ -f "$transaction/systemd-managed" ] ;; esac
  }; then
    sudo -n "$sctl" stop mozart.service || { echo '无法经 systemd 停止新栈，保留回滚记录' >&2; exit 1; }
  else
    stop_ctl=$ctl
    if [ -x "$transaction/ctl-before" ]; then stop_ctl="$transaction/ctl-before"; fi
    if [ -x "$stop_ctl" ]; then
      if ! "$stop_ctl" --config "$cfg" down; then
        if pgrep -x mozart-daemon; then echo '无法停止新栈，保留回滚记录' >&2; exit 1; fi
      fi
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
  if [ -f "$transaction/source-synced" ]; then
    rsync -a --checksum --no-times --delete --exclude target --exclude build --exclude .git --exclude node_modules "$snapshot/" "$HOME/Mozart/"
  fi
else
  read -r source_id < "$transaction/source-created-id"
  if [ -e "$HOME/Mozart" ] || [ -L "$HOME/Mozart" ]; then
    [ ! -L "$HOME/Mozart" ] && [ -d "$HOME/Mozart" ] &&
      [ "$(stat -c '%d:%i' "$HOME/Mozart")" = "$source_id" ] &&
      [ ! -e "$transaction/failed-source" ] && [ ! -L "$transaction/failed-source" ] || {
        echo '源码目录已被替换或备份目标不安全，拒绝回滚' >&2; exit 1;
      }
    mv -T "$HOME/Mozart" "$transaction/failed-source"
  fi
fi
if [ -f "$transaction/was-running" ] && { [ -f "$transaction/stopped" ] || [ -f "$transaction/stopping" ]; }; then
  if [ -f "$transaction/systemd-managed" ]; then
    sctl=$(command -v systemctl)
    if [ "$(systemd_state)" = deactivating ]; then
      sudo -n "$sctl" stop mozart.service
    fi
    sudo -n "$sctl" start mozart.service || {
      echo '回滚未能启动 mozart.service；若遇 systemd 启动限流，请执行：sudo systemctl reset-failed mozart.service && sudo systemctl start mozart.service' >&2
      exit 1
    }
  else
    "$ctl" --config "$cfg" up
  fi
fi
if [ -x "$web" ] && [ -f "$transaction/web-stopped" ] && [ -f "$transaction/web-was-running" ]; then
  "$web" "$root" start
fi
if [ -f "$transaction/deploy-before.json" ]; then cp -a "$transaction/deploy-before.json" "$root/deploy.json";
elif [ -f "$transaction/installing" ]; then rm -f "$root/deploy.json"; fi
printf '部署前源码与运行状态已恢复；事务记录：%s\n' "$transaction"
ROLLBACK
if [ -L "$HOME/Mozart" ] || { [ -e "$HOME/Mozart" ] && [ ! -d "$HOME/Mozart" ]; }; then
  echo '源码目标不是安全的目录' >&2; exit 1
fi
if [ -d "$HOME/Mozart" ]; then
  snapshot=$(mktemp -d "$HOME/.mozart-source-backup.XXXXXXXX")
  rsync -a --checksum --exclude target --exclude build --exclude .git --exclude node_modules "$HOME/Mozart/" "$snapshot/"
  touch "$snapshot"
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
touch "$transaction/initialized"
INIT
rollback() {
  local result=$?
  trap - EXIT INT TERM
  if [ "$result" -ne 0 ]; then
    echo "部署命令失败，请持锁会话核对事务状态：$TRANSACTION" >&2
    if ! printf 'rollback\n' >&"$lock_in" || ! wait "$lock_pid"; then
      echo "回滚失败，请保留并检查事务记录：$TRANSACTION" >&2
    fi
  fi
  exit "$result"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo '== 2/5 同步源码并停止运行时 =='
"${SSH[@]}" "$guard touch $(printf %q "$TRANSACTION/source-synced")"
rsync -a --checksum --no-times --delete --rsync-path="$guard rsync" -e 'ssh -o ConnectTimeout=10' \
  --exclude target --exclude build --exclude .git --exclude node_modules "$REPO/" "$TARGET":~/Mozart/
"${SSH[@]}" "TRANSACTION=$(printf %q "$TRANSACTION") $guard bash -s" <<'STOP'
set -euo pipefail
read -r root < "$TRANSACTION/runtime"
ctl="$root/bin/mozart-ctl"
web="$root/bin/mozart-web-service"
if [ -x "$web" ] && "$web" "$root" status >/dev/null; then
  touch "$TRANSACTION/web-was-running" "$TRANSACTION/web-stopped"
  "$web" "$root" stop
fi
: > "$TRANSACTION/status-before.json"
sctl=$(command -v systemctl || true)
svc_wd=""
svc_state=""
if [ -n "$sctl" ]; then
  svc_wd=$("$sctl" show -p WorkingDirectory --value mozart.service 2>/dev/null || true)
  if [ -n "$svc_wd" ] && [ "$(realpath -m "$svc_wd")" = "$(realpath -m "$root")" ]; then
    svc_state=$("$sctl" is-active mozart.service 2>/dev/null || true)
  fi
fi
save_baseline() {
  jq -se 'length == 1 and (.[0].processes | type == "object" and length > 0 and all(.[]; .state | type == "string" and length > 0))' "$TRANSACTION/status-before.tmp" >/dev/null || { echo '停栈前状态缺少进程基线' >&2; exit 1; }
  mv "$TRANSACTION/status-before.tmp" "$TRANSACTION/status-before.json"
}
case "$svc_state" in
  active|activating|reloading|deactivating)
    "$ctl" --config "$root/mozart.toml" status --json > "$TRANSACTION/status-before.tmp"
    save_baseline
    touch "$TRANSACTION/was-running" "$TRANSACTION/systemd-managed"
    touch "$TRANSACTION/stopping"
    sudo -n "$sctl" stop mozart.service || { echo '无法经 systemd 停止 mozart.service（请检查 /etc/sudoers.d/mozart-deploy）' >&2; exit 1; }
    touch "$TRANSACTION/stopped"
    ;;
  *)
    if [ -x "$ctl" ] && "$ctl" --config "$root/mozart.toml" status --json > "$TRANSACTION/status-before.tmp"; then
      save_baseline
      touch "$TRANSACTION/was-running"
      touch "$TRANSACTION/stopping"
      "$ctl" --config "$root/mozart.toml" down
      touch "$TRANSACTION/stopped"
    fi
    ;;
esac
# 停栈后留短暂收割缓冲（最多 3 s），避免 systemd/子进程退出竞态误报残留；
# 只检查实际进程名，不匹配包含命令文本的 ssh/bash 进程；Linux comm 限 15 字节，mozart-web-adapter 截断为 mozart-web-adap。
process_names=(mozart-daemon mozart-pre mozart-enhance mozart-rvc mozart-asr mozart-tts mozart-bridge mozart-web-adap llama-server)
for _wait in 1 2 3 4 5 6; do
  lingering=""
  for name in "${process_names[@]}"; do
    if pgrep -x "$name" >/dev/null; then lingering=$name; break; fi
  done
  [ -n "$lingering" ] || break
  sleep 0.5
done
for name in "${process_names[@]}"; do
  if pgrep -x "$name"; then echo "仍有活跃进程，停止构建：$name" >&2; exit 1; fi
done
STOP
stage "2/5 同步与停栈"

echo '== 3/5 构建设备程序 =='
"${SSH[@]}" "FULL=$FULL $guard bash -s" <<'BUILD'
set -euo pipefail
. "$HOME/.cargo/env"
cd "$HOME/Mozart"
assets=${MOZART_ASSETS_DIR:-$HOME/mozart-assets}
if [ -e "$assets/lib/libsherpa-onnx-c-api.so" ]; then export SHERPA_ONNX_LIB_DIR="$assets/lib"; fi
if [ -e "$assets/lib/libmozart_aec3.so" ]; then export MOZART_AEC3_LIB_DIR="$assets/lib"; fi
export LD_LIBRARY_PATH="$assets/lib:$assets/llama${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
if [ "$FULL" = 1 ]; then cargo clean; fi
t0=$SECONDS
# 部署只安装 rvc_stub 一个桩，不编其余 examples（SPEC-09 部署提速）。
cargo build --workspace --bins --example rvc_stub --jobs 2 --locked
echo "-- 耗时 cargo：$((SECONDS - t0)) s"
t1=$SECONDS
cmake --preset jetson-release -DFETCHCONTENT_UPDATES_DISCONNECTED=ON
# 部署不跑测试：只编运行时两个可执行目标，不随部署编测试程序。
if [ "$FULL" = 1 ]; then
  cmake --build --preset jetson-release --parallel 2 --clean-first --target mozart-rvc mozart-enhance
else
  cmake --build --preset jetson-release --parallel 2 --target mozart-rvc mozart-enhance
fi
echo "-- 耗时 cmake：$((SECONDS - t1)) s"
cp build/jetson-release/rvc/mozart-rvc target/debug/mozart-rvc
cp build/jetson-release/enhance/mozart-enhance target/debug/mozart-enhance
BUILD
stage "3/5 设备构建"

echo '== 4/5 安装并启动 =='
"${SSH[@]}" "TRANSACTION=$(printf %q "$TRANSACTION") COMMIT=$(printf %q "$COMMIT") DIRTY=$DIRTY FULL=$FULL access_host=$(printf %q "$access_host") $guard bash -s" <<'INSTALL'
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
validated=$(target/debug/mozart-ctl --config "$root/mozart.toml" config validate --json)
description=$(target/debug/mozart-web-adapter --config "$root/mozart.toml" --print-config)
[ "$(jq -er '.configured' <<< "$description")" = true ] || { echo '部署需要 mozart.toml 配置 [web] 段（由 daemon 监管 web-adapter）' >&2; exit 1; }
allowed=$(jq -er '.allowed_origins[0] // empty' <<< "$description") || { echo '部署需要 [web].allowed_origins 至少包含一个来源，否则 /api/v1/health 会返回 403' >&2; exit 1; }
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
systemd_used=false
sctl=$(command -v systemctl || true)
svc_matches=0
if [ -n "$sctl" ]; then
  svc_wd=$("$sctl" show -p WorkingDirectory --value mozart.service 2>/dev/null || true)
  if [ -n "$svc_wd" ] && [ "$(realpath -m "$svc_wd")" = "$(realpath -m "$root")" ]; then
    svc_matches=1
  fi
fi
if [ -f "$TRANSACTION/systemd-managed" ] || { [ "$svc_matches" -eq 1 ] && "$sctl" is-enabled --quiet mozart.service 2>/dev/null; }; then
  sudo -n "$sctl" start mozart.service || { echo '无法经 systemd 启动 mozart.service（请检查 /etc/sudoers.d/mozart-deploy）' >&2; exit 1; }
  systemd_used=true
else
  "$root/bin/mozart-ctl" --config "$root/mozart.toml" up
fi
# 包括原本正常的可选服务；不把部署前已降级的服务冒充本次部署失败。
probe_host=$(jq -r '.listen | sub(":[0-9]+$"; "")' <<< "$description")
case "$probe_host" in 0.0.0.0) probe_host=127.0.0.1 ;; ::|'[::]') probe_host='[::1]' ;; esac
healthy=0
for attempt in $(seq 1 90); do
  if "$root/bin/mozart-ctl" --config "$root/mozart.toml" status --json > "$TRANSACTION/status-after.json"; then
    if jq -e --argjson validated "$validated" --slurpfile previous "$TRANSACTION/status-before.json" '
      (.processes.web.state == "running") and
      (.pre != null) and
      (($validated.output_mode_managed == false and .output_mode == null) or .output_mode.phase == "applied") and
      (.rvc != null) and
      (.rvc.state == "loaded" or .rvc.state == "streaming" or
       ($validated.startup_rvc_expected == false and .rvc.state == "idle")) and
      (if .pre.denoise_backend == "ulunas" then
        .enhance.backend == "ulunas" and
        ( .pre.enhancement.enabled == false or
          (.pre.enhancement.connected == true and .pre.enhancement.last_error == null))
       else true end) and
      (if ($previous | length) == 0 then true else
        . as $current | all($previous[0].processes | to_entries[]; .value.state != "running" or $current.processes[.key].state == "running")
      end)' "$TRANSACTION/status-after.json" >/dev/null &&
      [ "$(curl -fsS --max-time 3 -H "Host: ${allowed#http://}" --noproxy '*' -o "$TRANSACTION/health.json" -w '%{http_code}' "http://${probe_host}:${port}/api/v1/health")" = 200 ] &&
      jq -e '.status == "ok"' "$TRANSACTION/health.json" >/dev/null; then
      echo '运行时健康检查通过：/api/v1/health HTTP 200，status=ok'
      healthy=1; break
    fi
  fi
  sleep 1
done
[ "$healthy" = 1 ] || { echo '运行时健康检查失败' >&2; exit 1; }
access_host=${access_host#[}; access_host=${access_host%]}
[[ $access_host != *:* ]] || access_host="[$access_host]"
origin="http://$access_host"
if [ "$port" != 80 ]; then origin+=":$port"; fi
origin_present=$(jq --arg origin "$origin" --arg explicit "http://$access_host:$port" '
    .allowed_origins | map(ascii_downcase) | index($origin | ascii_downcase) != null or index($explicit | ascii_downcase) != null' <<< "$description")
if [ "$origin_present" = false ]; then
  echo "告警：[web].allowed_origins 缺少访问源 $origin；未修改 mozart.toml" >&2
fi
mode=incremental
if [ "$FULL" = 1 ]; then mode=full; fi
jq -n --arg commit "$COMMIT" --argjson dirty "$DIRTY" --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg mode "$mode" --argjson systemd "$systemd_used" \
  '{commit:$commit,dirty:$dirty,time:$time,preset:"jetson-release",mode:$mode,restarted:"all",systemd:$systemd}' > "$root/deploy.json.new"
mv "$root/deploy.json.new" "$root/deploy.json"
touch "$TRANSACTION/completed"
prune() {
  local parent=$1 pattern=$2 pinned=$3 entry path kept=0
  [ -z "$pinned" ] || kept=1
  find "$parent" -mindepth 1 -maxdepth 1 -type d -uid "$(id -u)" -name "$pattern" -printf '%T@ %p\0' | sort -zrn > "$TRANSACTION/prune-list"
  while IFS= read -r -d '' entry; do
    path=${entry#* }
    [ "$path" != "$pinned" ] || continue
    if (( kept < 3 )); then kept=$((kept + 1)); else rm -rf -- "$path"; fi
  done < "$TRANSACTION/prune-list"
}
prune "$HOME" '.mozart-source-backup.*' ''
prune /tmp 'mozart-deploy.*' "$TRANSACTION"
INSTALL
stage "4/5 安装与启动"
trap - EXIT INT TERM
exec {lock_in}>&-
wait "$lock_pid"
echo "== 5/5 部署完成（${SECONDS} s）=="
echo "源码：~/Mozart（$COMMIT，dirty=$DIRTY）；构建期间已暂停整栈。"
echo '运行位：mozart-runtime/bin 和 web/dist；上一代：bin.prev 和 web.prev。'
echo "事务记录：$TRANSACTION；功能验收：tools/jetson-e2e.sh $TARGET"
