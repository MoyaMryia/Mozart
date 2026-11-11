#!/usr/bin/env bash
# 本地与 CI 共用的检查命令。依赖安装留在 workflow 中。
#   tools/ci-local.sh <repo|lint|native|web>          运行一个阶段
#   tools/ci-local.sh --staged                        提交前检查，pre-commit hook 调用
#   tools/ci-local.sh all                             依次运行全部阶段
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

MAX_TRACKED_BYTES=$((5 * 1024 * 1024))

# 阶段名与触发路径（扩展正则，匹配暂存文件路径）。
NATIVE_PATHS='^(core|pre|rvc|daemon|api|tests)/|^CMakeLists\.txt$|^CMakePresets\.json$|^cmake/'
RUST_PATHS='^(core|pre|daemon)/|^Cargo\.toml$|^Cargo\.lock$'
WEB_PATHS='^web/'

skip() { echo "跳过 $1：$2" >&2; }

stage_repo() {
    local fails=0

    # 索引必须与忽略规则一致：被忽略却仍在跟踪的文件说明规则或索引有一方错了。
    local drift
    drift=$(git ls-files -i -c --exclude-standard || true)
    if [ -n "$drift" ]; then
        echo "以下文件被 .gitignore 忽略但仍在索引中：" >&2
        echo "$drift" >&2
        echo "取消跟踪（git rm --cached）或修正规则。" >&2
        fails=1
    fi

    # 单个跟踪文件不超过上限：资产出库，避免再次积累无法清理的历史对象。
    local big
    big=$(git ls-files -z | xargs -0 -r stat -c '%s %n' 2>/dev/null \
        | awk -v lim="$MAX_TRACKED_BYTES" '$1 > lim {printf "  %.1f MiB  %s\n", $1/1048576, $2}')
    if [ -n "$big" ]; then
        echo "以下跟踪文件超过 5 MiB，请移到仓外并加入 .gitignore：" >&2
        echo "$big" >&2
        fails=1
    fi

    [ "$fails" -eq 0 ] || exit 1
    echo "仓库卫生：通过（$(git ls-files | wc -l) 个跟踪文件）" >&2
}

stage_lint() {
    bash tools/lint.sh "$@"
}

stage_native() {
    if [ ! -f CMakeLists.txt ]; then
        skip native "没有 CMakeLists.txt"
        return 0
    fi
    cmake --preset host-debug
    cmake --build --preset host-debug --parallel 2
    if ctest --preset host-debug -N 2>/dev/null | grep -q 'Test #'; then
        ctest --preset host-debug
    else
        skip native "没有注册的 ctest 用例"
    fi
}

stage_rust() {
    if [ ! -f Cargo.toml ]; then
        skip rust "没有 Cargo.toml"
        return 0
    fi
    cargo check --workspace
    cargo test --workspace
}

stage_web() {
    if [ ! -f web/package.json ]; then
        skip web "web/package.json 不存在"
        return 0
    fi
    npm --prefix web test
    npm --prefix web run build
}

# 测试读取工作区。检查范围内的工作区必须与暂存区一致，结果才对应将要提交的内容。
require_clean() {
    local paths="$1" dirty
    dirty=$({ git diff --name-only; git ls-files --others --exclude-standard; } | grep -E "$paths" || true)
    if [ -n "$dirty" ]; then
        echo "以下文件有未暂存或未跟踪的改动。暂存、还原或移走后再提交：" >&2
        echo "$dirty" >&2
        exit 1
    fi
}

run_stage() {
    case "$1" in
        repo) stage_repo ;;
        lint) stage_lint ;;
        native) stage_native ;;
        rust) stage_rust ;;
        web) stage_web ;;
        *) echo "未知阶段：$1" >&2; exit 2 ;;
    esac
}

staged() {
    local changed base
    changed=$(git diff --cached --name-only)
    [ -n "$changed" ] || exit 0

    base=HEAD
    git rev-parse -q --verify HEAD >/dev/null || base=
    stage_lint --staged ${base:+--base-ref "$base"}

    local name paths
    for name in native rust web; do
        case $name in
            native) paths=$NATIVE_PATHS ;;
            rust) paths=$RUST_PATHS ;;
            web) paths=$WEB_PATHS ;;
        esac
        if grep -qE "$paths" <<<"$changed"; then
            require_clean "$paths"
            echo "== $name ==" >&2
            # 测试读取工作区，不读暂存区索引。
            ( unset GIT_INDEX_FILE GIT_PREFIX; run_stage "$name" )
        fi
    done
}

case "${1:---staged}" in
    --staged) staged ;;
    all) for s in repo lint native rust web; do echo "== $s ==" >&2; run_stage "$s"; done ;;
    *) run_stage "$1" ;;
esac
