#!/usr/bin/env bash
# 在 Jetson 上创建 RVC 导出用的 Python 环境（venv）。
# 导出与 golden 参考都只用 CPU PyTorch（run_reference/export 均 device=cpu），
# 所以不需要 CUDA 版 torch。Jetson aarch64 的 torch 轮子来源随 JetPack 而异：
#   - 有本地 wheel 时用 TORCH_WHEEL=/path/to/torch-*.whl 指定；
#   - 否则回退 pip 官方源，失败时打印下一步。
# 用法：tools/model/setup-export-env.sh [venv路径]
#   VENV=/path tools/model/setup-export-env.sh
set -euo pipefail

VENV=${VENV:-${1:-$HOME/mozart-assets/rvc-env}}
PY=${PYTHON:-python3}

echo "== 创建 venv: $VENV =="
"$PY" -m venv "$VENV"
# shellcheck disable=SC1091
source "$VENV/bin/activate"
pip install --upgrade pip

if [ -n "${TORCH_WHEEL:-}" ]; then
    echo "== 安装 torch: $TORCH_WHEEL =="
    pip install "$TORCH_WHEEL"
else
    echo "== 安装 torch（pip 官方源）=="
    if ! pip install torch; then
        echo "torch 安装失败。Jetson aarch64 通常需要 NVIDIA 提供的 wheel。" >&2
        echo "重新运行并指定本地 wheel：" >&2
        echo "  TORCH_WHEEL=/path/to/torch-*.whl $0 $VENV" >&2
        exit 1
    fi
fi

echo "== 安装导出/golden 依赖 =="
pip install transformers onnx onnxruntime numpy scipy librosa soundfile

echo "== 校验导入 =="
python - <<'PY'
import torch, transformers, onnx, onnxruntime, numpy, scipy, librosa, soundfile
print("torch", torch.__version__, "| transformers", transformers.__version__,
      "| onnx", onnx.__version__, "| onnxruntime", onnxruntime.__version__)
PY

echo "环境就绪：$VENV"
echo "用法：source $VENV/bin/activate 后运行 tools/model/*.py"
echo "RVC 源码：导出时用 --rvc-source ~/mozart-assets/RVC（或 RVC_SOURCE 环境变量）"