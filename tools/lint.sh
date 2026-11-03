#!/usr/bin/env bash
# 文档与提交内容的轻量检查。不依赖 Python。
#   tools/lint.sh                    检查全部跟踪的 Markdown
#   tools/lint.sh --staged           检查暂存的新增行
#   tools/lint.sh --base-ref <sha>   检查提交范围内的新增行
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

mode=all
base=
case "${1:-}" in
    --staged) mode=diff; base="--cached" ;;
    --base-ref) mode=diff; base="$2" ;;
    "") ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
esac

fail=0

report() {
    echo "$1" >&2
    fail=1
}

# 行尾空白和制表符：Markdown 中除代码块外不出现。
check_lines() {
    local file=$1 line=$2 text=$3
    if printf '%s' "$text" | grep -qE '[[:space:]]+$'; then
        report "行尾空白：$file:$line"
    fi
}

if [ "$mode" = all ]; then
    files=$(git ls-files '*.md')
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        while IFS= read -r entry; do
            n=${entry%%:*}
            check_lines "$f" "$n" "${entry#*:}"
        done < <(grep -n ' $' "$f" || true)
    done <<<"$files"
else
    diff_args=(--unified=0 --no-color)
    if [ "$base" = "--cached" ]; then
        diff_args+=(--cached)
    else
        diff_args+=("$base"..HEAD)
    fi
    while IFS= read -r entry; do
        [ -n "$entry" ] || continue
        file=${entry#+++ b/}
        check_lines "$file" "新增行" "$(git diff "${diff_args[@]}" -- "$file" | grep -E '^\+[^+]' | sed 's/^+//' | head -1)"
    done < <(git diff "${diff_args[@]}" --name-only --diff-filter=AM | sed 's/^/+++ b\//')
fi

# 实测报告必须包含“适用范围”一节。
if [ "$mode" = all ]; then
    scope_files=$(git ls-files 'reports/*/RESULTS.md' 'reports/*/AUDIT.md' 'reports/*/REVIEW.md')
else
    scope_files=$(git diff "${diff_args[@]}" --name-only --diff-filter=AM | grep -E '^reports/[^/]+/(RESULTS|AUDIT|REVIEW)\.md$' || true)
fi
while IFS= read -r f; do
    [ -n "$f" ] || continue
    grep -q '^## 适用范围' "$f" || report "缺少“## 适用范围”一节：$f"
done <<<"$scope_files"

if [ "$fail" -ne 0 ]; then
    echo "检查未通过。" >&2
    exit 1
fi
echo "文档检查：通过" >&2
