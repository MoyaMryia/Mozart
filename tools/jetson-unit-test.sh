#!/bin/bash
# Jetson 设备端单元测试：cargo test --workspace + ctest（jetson-release）。
# 前置：仓库已同步到 ~/Mozart 并完成构建（tools/deploy-jetson.sh）。
# 正常由 tools/jetson-e2e.sh 通过 ssh 调用，也可在设备上直接运行。
#   ASSETS_DIR  仓外资产根（默认 $HOME/mozart-assets）
set -uo pipefail

ASSETS_DIR=${ASSETS_DIR:-${MOZART_ASSETS_DIR:-$HOME/mozart-assets}}

[ -d "$HOME/Mozart" ] || { echo "缺 ~/Mozart，先部署" >&2; exit 1; }
cd "$HOME/Mozart" || exit 1
. "$HOME/.cargo/env" 2>/dev/null || true

if [ -e "$ASSETS_DIR/lib/libsherpa-onnx-c-api.so" ]; then
  export SHERPA_ONNX_LIB_DIR="$ASSETS_DIR/lib"
fi
export LD_LIBRARY_PATH="$ASSETS_DIR/lib:$ASSETS_DIR/llama${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

echo '== cargo test --workspace =='
# 设备端进程桩测试对时序敏感，串行执行避免并行负载造成假失败。
cargo test --workspace -- --test-threads=1 || exit 1

if ctest_list=$(ctest --test-dir build/jetson-release -N 2>&1); then
  if grep -q 'Test #' <<<"$ctest_list"; then
    echo '== ctest（jetson-release）=='
    ctest --test-dir build/jetson-release || exit 1
  else
    echo 'jetson-release 无注册 ctest 用例，跳过 ctest'
  fi
else
  echo 'ctest 清点失败（jetson-release 未配置或工具故障）' >&2
  exit 1
fi
