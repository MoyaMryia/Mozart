#!/usr/bin/env bash
# 仅前端部署：主机构建 web，替换设备运行位 web/dist。不编后端、不停栈、不重启。
# 适配器按请求读盘，替换后下一个请求即生效。接口变更仍须走 deploy-jetson.sh。
# 用法：tools/deploy-web.sh <SSH目标>
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
TARGET=${MOZART_JETSON:-}
POSITIONAL=0
for argument in "$@"; do
  case "$argument" in
    --help|-h) echo '用法：tools/deploy-web.sh <SSH目标>'; exit 0 ;;
    -*) echo "未知参数：$argument" >&2; exit 2 ;;
    *) [ "$POSITIONAL" -eq 0 ] || { echo '只能指定一个 SSH 目标' >&2; exit 2; }; TARGET=$argument; POSITIONAL=1 ;;
  esac
done
[ -n "$TARGET" ] || { echo '请提供 SSH 目标或 MOZART_JETSON' >&2; exit 2; }
for tool in ssh rsync npm; do command -v "$tool" >/dev/null || { echo "缺少 $tool" >&2; exit 1; }; done
SSH=(ssh -o ConnectTimeout=10 "$TARGET")
ENVIRONMENT="MOZART_RUNTIME_DIR=$(printf %q "${MOZART_RUNTIME_DIR:-}")"

echo '== 1/3 构建网页（主机）=='
# 按需 npm ci：lock 未变且 node_modules 存在时跳过安装（与 deploy-jetson.sh 同一 stamp）。
lock_hash=$(sha256sum "$REPO/web/package-lock.json" | cut -d' ' -f1)
stamp="$REPO/web/node_modules/.mozart-npm-ci.sha256"
if [ ! -d "$REPO/web/node_modules" ] || [ ! -f "$stamp" ] || [ "$(cat "$stamp")" != "$lock_hash" ]; then
  npm --prefix "$REPO/web" ci
  printf '%s\n' "$lock_hash" > "$stamp"
else
  echo '依赖未变，跳过 npm ci'
fi
npm --prefix "$REPO/web" run build
[ -f "$REPO/web/dist/index.html" ] || { echo '构建缺少 web/dist/index.html' >&2; exit 1; }

echo '== 2/3 备份并替换运行位静态资源 =='
# 备份与 deploy-jetson.sh 的 web.prev 语义一致，便于其回滚复用。
REMOTE_ROOT=$("${SSH[@]}" "$ENVIRONMENT bash -s" <<'PREP'
set -euo pipefail
root=${MOZART_RUNTIME_DIR:-$HOME/mozart-runtime}
mkdir -p "$root/web"
if [ -d "$root/web/dist" ]; then
  web_backup=$(mktemp -d "$root/.web-backup.XXXXXXXX")
  cp -a "$root/web/." "$web_backup/"
  rm -rf "$root/web.prev"
  mv "$web_backup" "$root/web.prev"
fi
mkdir -p "$root/web/dist"
printf '%s\n' "$root"
PREP
)
rsync -a --checksum --delete -e 'ssh -o ConnectTimeout=10' \
  "$REPO/web/dist/" "$TARGET:$REMOTE_ROOT/web/dist/"

echo '== 3/3 校验 =='
"${SSH[@]}" "test -f $(printf %q "$REMOTE_ROOT")/web/dist/index.html" \
  || { echo '运行位缺少 index.html' >&2; exit 1; }
echo "前端部署完成：$REMOTE_ROOT/web/dist（上一代 web.prev）；未重启栈，适配器下个请求即生效。"