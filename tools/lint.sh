#!/usr/bin/env bash
# 文档与提交内容的轻量检查。不依赖 Python。
#   tools/lint.sh                    检查全部跟踪的 Markdown
#   tools/lint.sh --staged           检查暂存区中的 Markdown
#   tools/lint.sh --base-ref <sha>   检查提交范围内的 Markdown
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# 行尾空白逐行检查，代码围栏内的正文同样不允许。
# “适用范围”必须在围栏之外，用二级标题写出，正文提到这四个字不算。
check_stream() {
    local path=$1 scope=$2
    awk -v name="$path" -v require_scope="$scope" '
        BEGIN { infence = 0; has_scope = 0; problems = 0 }
        {
            line = $0
            if (line ~ /^[[:space:]]*(```|~~~)/) {
                infence = !infence
                next
            }
            if (line ~ /[[:space:]]+$/) {
                printf "%s:%d: 行尾空白\n", name, NR
                problems = 1
            }
            if (!infence && line ~ /^## 适用范围[[:space:]]*$/) {
                has_scope = 1
            }
        }
        END {
            if (require_scope && !has_scope) {
                printf "%s: 缺少“## 适用范围”一节\n", name
                problems = 1
            }
            if (problems) exit 1
        }
    '
}

needs_scope() {
    case "$1" in
        reports/*/RESULTS.md | reports/*/AUDIT.md | reports/*/REVIEW.md) return 0 ;;
        *) return 1 ;;
    esac
}

mode=all
base=
case "${1:-}" in
    --staged) mode=diff; base=--cached ;;
    --base-ref) mode=diff; base=$2 ;;
    "") ;;
    *)
        echo "未知参数：$1" >&2
        exit 2
        ;;
esac

fail=0
scope=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    if needs_scope "$f"; then scope=1; else scope=0; fi
    case "$mode" in
        all) check_stream "$f" "$scope" <"$f" || fail=1 ;;
        *)
            if [ "$base" = --cached ]; then
                git show ":$f" | check_stream "$f" "$scope" || fail=1
            else
                git show "HEAD:$f" | check_stream "$f" "$scope" || fail=1
            fi
            ;;
    esac
done < <(
    if [ "$mode" = all ]; then
        git ls-files '*.md'
    else
        spec=$base
        [ "$base" = --cached ] || spec="$base..HEAD"
        git diff "$spec" --name-only --diff-filter=AMR -- '*.md'
    fi
)

if [ "$fail" -ne 0 ]; then
    echo "检查未通过。" >&2
    exit 1
fi
echo "文档检查：通过" >&2
