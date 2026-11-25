#!/usr/bin/env bash
# 安装、更新或卸载 Jetson 开机定频与 Mozart 自启 systemd 服务。
# 支持本机远程推送执行或直接在 Jetson 本地执行（--local）。
# 用法：
#   tools/install-jetson-systemd.sh [选项] <SSH目标>
#   tools/install-jetson-systemd.sh --local [选项]
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${MOZART_JETSON:-}
LOCAL=0
RESTART="on-failure"
FAN_FLAG=""
WITH_CLOCKS=1
ENABLE_NOW=1
UNINSTALL=0
RUNTIME_DIR_ARG=${MOZART_RUNTIME_DIR:-}

usage() {
  cat <<'HELP'
用法：
  tools/install-jetson-systemd.sh [选项] <SSH目标>
  tools/install-jetson-systemd.sh --local [选项]

选项：
  --local                       在当前 Jetson 设备本机直接执行（不走 SSH）
  --restart <on-failure|no>     进程异常退出时是否由 systemd 自动重启（默认 on-failure）
  --fan                         额外传入 jetson_clocks --fan 强制满载风扇（默认保留自动调速风扇）
  --no-fan                      保留自动调速风扇（默认行为）
  --no-clocks                   不安装 mozart-clocks.service，仅装 mozart.service（已装则移除）
  --no-enable                   只写入服务文件，暂不启用与启动
  --runtime-dir <路径>          运行时目录（默认 $HOME/mozart-runtime）
  --uninstall                   停用并删除 systemd 单元与部署免密规则
  -h, --help                    显示帮助
HELP
}

POSITIONAL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --local) LOCAL=1 ;;
    --restart)
      shift
      [ $# -gt 0 ] || { echo '--restart 需要参数：on-failure 或 no' >&2; exit 2; }
      case "$1" in
        on-failure|no) RESTART=$1 ;;
        *) echo "--restart 只接受 on-failure 或 no，收到：$1" >&2; exit 2 ;;
      esac
      ;;
    --restart=*)
      val=${1#*=}
      case "$val" in
        on-failure|no) RESTART=$val ;;
        *) echo "--restart 只接受 on-failure 或 no，收到：$val" >&2; exit 2 ;;
      esac
      ;;
    --fan) FAN_FLAG="--fan" ;;
    --no-fan) FAN_FLAG="" ;;
    --no-clocks) WITH_CLOCKS=0 ;;
    --no-enable) ENABLE_NOW=0 ;;
    --runtime-dir)
      shift
      [ $# -gt 0 ] || { echo '--runtime-dir 需要路径参数' >&2; exit 2; }
      RUNTIME_DIR_ARG=$1
      ;;
    --runtime-dir=*) RUNTIME_DIR_ARG=${1#*=} ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "未知参数：$1" >&2; exit 2 ;;
    *)
      [ "$POSITIONAL" -eq 0 ] || { echo '只能指定一个 SSH 目标' >&2; exit 2; }
      TARGET=$1
      POSITIONAL=1
      ;;
  esac
  shift
done

CLOCKS_TPL="$REPO/deploy/systemd/mozart-clocks.service.in"
MOZART_TPL="$REPO/deploy/systemd/mozart.service.in"
if [ "$UNINSTALL" -eq 0 ]; then
  [ -f "$MOZART_TPL" ] || { echo "缺少模板：$MOZART_TPL" >&2; exit 1; }
  if [ "$WITH_CLOCKS" -eq 1 ]; then
    [ -f "$CLOCKS_TPL" ] || { echo "缺少模板：$CLOCKS_TPL" >&2; exit 1; }
  fi
fi

run_remote_or_local() {
  local script=$1
  if [ "$LOCAL" -eq 1 ]; then
    bash -s <<< "$script"
    return
  fi
  [ -n "$TARGET" ] || { echo '请提供 SSH 目标、设置 MOZART_JETSON，或传入 --local' >&2; exit 2; }
  # 先落远端临时文件，再按需分配 PTY 执行，避免远端 sudo 因 stdin 被脚本占用而无法读密码。
  local remote_tmp
  remote_tmp=$(ssh -o ConnectTimeout=10 "$TARGET" 'mktemp /tmp/mozart-systemd-run.XXXXXXXX')
  ssh -o ConnectTimeout=10 "$TARGET" "cat > $(printf %q "$remote_tmp")" <<< "$script"
  local ssh_opts=(-o ConnectTimeout=10)
  if [ -t 0 ] && [ -t 1 ]; then
    ssh_opts+=(-t)
  fi
  ssh "${ssh_opts[@]}" "$TARGET" "bash $(printf %q "$remote_tmp"); rc=\$?; rm -f $(printf %q "$remote_tmp"); exit \$rc"
}

if [ "$UNINSTALL" -eq 1 ]; then
  UNINSTALL_SCRIPT=$(cat <<'EOS'
set -euo pipefail
for unit in mozart.service mozart-clocks.service; do
  if systemctl list-unit-files "$unit" --no-legend 2>/dev/null | grep -q "^$unit"; then
    sudo systemctl disable --now "$unit" || true
  fi
  sudo rm -f "/etc/systemd/system/$unit"
done
sudo rm -f /etc/sudoers.d/mozart-deploy
sudo systemctl daemon-reload
echo '已卸载 mozart.service、mozart-clocks.service 与 /etc/sudoers.d/mozart-deploy'
EOS
)
  run_remote_or_local "$UNINSTALL_SCRIPT"
  exit 0
fi

CLOCKS_CONTENT=""
if [ "$WITH_CLOCKS" -eq 1 ]; then
  CLOCKS_CONTENT=$(cat "$CLOCKS_TPL")
fi
MOZART_CONTENT=$(cat "$MOZART_TPL")

INSTALL_SCRIPT=$(cat <<EOS
set -euo pipefail
[ "\$(id -u)" -ne 0 ] || { echo '请以普通运行用户执行（脚本内部按需调用 sudo）' >&2; exit 1; }
USER_NAME=\$(id -un)
GROUP_NAME=\$(id -gn)
USER_HOME=\$HOME
RUNTIME_DIR=\${RUNTIME_DIR_OVERRIDE:-\$HOME/mozart-runtime}
case "\$RUNTIME_DIR" in
  "~") RUNTIME_DIR="\$HOME" ;;
  "~/"*) RUNTIME_DIR="\$HOME/\${RUNTIME_DIR#~/}" ;;
  /*) ;;
  *) RUNTIME_DIR="\$HOME/\$RUNTIME_DIR" ;;
esac
if printf '%s\n%s\n' "\$RUNTIME_DIR" "\$USER_HOME" | grep -qE '[[:space:]|&]'; then
  echo "路径包含空白或非法字符（| / &），拒绝渲染 systemd 单元：\$RUNTIME_DIR" >&2
  exit 1
fi
[ -f "\$RUNTIME_DIR/mozart.toml" ] || { echo "缺少运行时配置：\$RUNTIME_DIR/mozart.toml" >&2; exit 1; }
[ -x "\$RUNTIME_DIR/bin/mozart-daemon" ] || { echo "缺少可执行文件：\$RUNTIME_DIR/bin/mozart-daemon" >&2; exit 1; }
[ -x "\$RUNTIME_DIR/bin/mozart-ctl" ] || { echo "缺少可执行文件：\$RUNTIME_DIR/bin/mozart-ctl" >&2; exit 1; }

SYSTEMCTL_BIN=\$(command -v systemctl)
[ -n "\$SYSTEMCTL_BIN" ] || { echo '未找到 systemctl' >&2; exit 1; }

WITH_CLOCKS=$WITH_CLOCKS
RESTART=$(printf %q "$RESTART")
FAN_FLAG=$(printf %q "$FAN_FLAG")
ENABLE_NOW=$ENABLE_NOW
CLOCKS_BACKUP_DIR=/var/lib/mozart
CLOCKS_BACKUP=/var/lib/mozart/l4t_dfs.conf
CLOCKS_UNIT=""
CLOCKS_WANTS_LINE=""
JETSON_CLOCKS_BIN=""
if [ "\$WITH_CLOCKS" -eq 1 ]; then
  JETSON_CLOCKS_BIN=\$(command -v jetson_clocks || true)
  [ -n "\$JETSON_CLOCKS_BIN" ] || { echo '未找到 jetson_clocks，如需跳过请加 --no-clocks' >&2; exit 1; }
  CLOCKS_UNIT="mozart-clocks.service"
  CLOCKS_WANTS_LINE="Wants=mozart-clocks.service"
fi

TMPDIR=\$(mktemp -d /tmp/mozart-systemd.XXXXXXXX)
trap 'rm -rf "\$TMPDIR"' EXIT

render() {
  local src=\$1 dst=\$2
  sed \
    -e "s|@MOZART_USER@|\$USER_NAME|g" \
    -e "s|@MOZART_GROUP@|\$GROUP_NAME|g" \
    -e "s|@MOZART_HOME@|\$USER_HOME|g" \
    -e "s|@MOZART_RUNTIME_DIR@|\$RUNTIME_DIR|g" \
    -e "s|@MOZART_RESTART@|\$RESTART|g" \
    -e "s|@CLOCKS_UNIT@|\$CLOCKS_UNIT|g" \
    -e "s|@CLOCKS_WANTS_LINE@|\$CLOCKS_WANTS_LINE|g" \
    -e "s|@CLOCKS_BACKUP_DIR@|\$CLOCKS_BACKUP_DIR|g" \
    -e "s|@CLOCKS_BACKUP@|\$CLOCKS_BACKUP|g" \
    -e "s|@CLOCKS_FLAGS@|\$FAN_FLAG|g" \
    -e "s|@JETSON_CLOCKS_BIN@|\$JETSON_CLOCKS_BIN|g" \
    -e 's/[[:space:]]*\$//' \
    "\$src" | awk 'NR==1 || !(NF==0 && prev=="") { print } { prev=\$0 }' > "\$dst"
}

if [ "\$WITH_CLOCKS" -eq 1 ]; then
  cat > "\$TMPDIR/clocks.in" <<'TPL_CLOCKS'
$CLOCKS_CONTENT
TPL_CLOCKS
  render "\$TMPDIR/clocks.in" "\$TMPDIR/mozart-clocks.service"
  sudo install -m 0644 "\$TMPDIR/mozart-clocks.service" /etc/systemd/system/mozart-clocks.service
else
  if "\$SYSTEMCTL_BIN" list-unit-files mozart-clocks.service --no-legend 2>/dev/null | grep -q '^mozart-clocks\.service'; then
    sudo "\$SYSTEMCTL_BIN" disable --now mozart-clocks.service || true
  fi
  sudo rm -f /etc/systemd/system/mozart-clocks.service
fi

cat > "\$TMPDIR/mozart.in" <<'TPL_MOZART'
$MOZART_CONTENT
TPL_MOZART
render "\$TMPDIR/mozart.in" "\$TMPDIR/mozart.service"
sudo install -m 0644 "\$TMPDIR/mozart.service" /etc/systemd/system/mozart.service

# 最小权限免密规则：仅放行运行用户启停/重启 mozart.service，供 deploy-jetson.sh 与 jetson-e2e.sh 无交互执行。
cat > "\$TMPDIR/mozart-deploy.sudoers" <<SUDOERS
\$USER_NAME ALL=(root) NOPASSWD: \$SYSTEMCTL_BIN start mozart.service, \$SYSTEMCTL_BIN stop mozart.service, \$SYSTEMCTL_BIN restart mozart.service
SUDOERS
sudo visudo -cf "\$TMPDIR/mozart-deploy.sudoers" >/dev/null
sudo install -m 0440 "\$TMPDIR/mozart-deploy.sudoers" /etc/sudoers.d/mozart-deploy

sudo "\$SYSTEMCTL_BIN" daemon-reload

if [ "\$ENABLE_NOW" -eq 1 ]; then
  if [ "\$WITH_CLOCKS" -eq 1 ]; then
    sudo "\$SYSTEMCTL_BIN" enable mozart-clocks.service
    # restart 确保重装切换 --fan / --no-fan 时先 restore 再按新参数生效（RemainAfterExit=yes 下 enable --now 不会重跑）。
    sudo "\$SYSTEMCTL_BIN" restart mozart-clocks.service
  fi
  # 若当前已有手工 mozart-ctl up 启动的实例，先平滑停下并等进程退出，再交由 systemd 托管，避免 daemon.lock 冲突。
  svc_state=\$("\$SYSTEMCTL_BIN" is-active mozart.service 2>/dev/null || true)
  case "\$svc_state" in
    active|activating|reloading|deactivating) ;;
    *)
      if "\$RUNTIME_DIR/bin/mozart-ctl" --config "\$RUNTIME_DIR/mozart.toml" status --json >/dev/null 2>&1; then
        "\$RUNTIME_DIR/bin/mozart-ctl" --config "\$RUNTIME_DIR/mozart.toml" down
        for _w in 1 2 3 4 5 6 7 8 9 10; do
          pgrep -x mozart-daemon >/dev/null || break
          sleep 0.3
        done
      fi
      ;;
  esac
  sudo "\$SYSTEMCTL_BIN" enable mozart.service
  sudo "\$SYSTEMCTL_BIN" restart mozart.service
  sleep 2
  "\$SYSTEMCTL_BIN" is-active mozart.service
  if [ "\$WITH_CLOCKS" -eq 1 ]; then
    "\$SYSTEMCTL_BIN" is-active mozart-clocks.service
  fi
fi

echo "systemd 安装完成：user=\$USER_NAME runtime=\$RUNTIME_DIR restart=\$RESTART clocks=\$WITH_CLOCKS fan=\${FAN_FLAG:-dynamic}"
EOS
)

if [ -n "$RUNTIME_DIR_ARG" ]; then
  INSTALL_SCRIPT="RUNTIME_DIR_OVERRIDE=$(printf %q "$RUNTIME_DIR_ARG")"$'\n'"$INSTALL_SCRIPT"
fi
run_remote_or_local "$INSTALL_SCRIPT"
