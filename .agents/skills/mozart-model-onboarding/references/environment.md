# 导出环境搭建

导出（pth→onnx）和 golden 参考都在 **CPU** 上跑，不需要 GPU 版 PyTorch。
环境在 `~/mozart-toolkit/rvc-env`，全链路在 Jetson 完成。设备目录见 [SETUP.md](../../../../docs/SETUP.md) §目录规范，本文只补搭建步骤、版本与使用前检查。

## 搭建

```bash
python3 -m venv ~/mozart-toolkit/rvc-env
~/mozart-toolkit/rvc-env/bin/pip install --upgrade pip
~/mozart-toolkit/rvc-env/bin/pip install \
    torch transformers==4.46.3 onnx onnxruntime \
    librosa soundfile scipy numpy
~/mozart-toolkit/rvc-env/bin/pip freeze > ~/mozart-toolkit/requirements.txt
```

仓库也有 `tools/model/setup-export-env.sh` 做同样的事；`TORCH_WHEEL=/path/to/*.whl` 可指定本地轮子。

导出用到的非仓内资源（SETUP 目录树之外由本文约定）：
- RVC 源码：`~/mozart-assets/RVC`（`--rvc-source`）
- 基础模型：`~/mozart-assets/RVC/assets/{hubert_base,rmvpe}`
- trtexec：PATH 中的 `trtexec`（可用 `TRTEXEC` 或 `--trtexec` 指定路径，编译引擎用）

## 版本推荐

| 包 | 推荐 | 理由 |
| --- | --- | --- |
| torch | CPU 轮子优先（已知可用 2.13.0） | 导出/golden 用 CPU；避免 JetPack CUDA 版本不匹配。镜像给的 CUDA 版如能用也可，但导出不受益。 |
| transformers | **4.46.3** | 导出脚本面向 4.x。实测 5.23 用本地路径 `from_pretrained` 直接报错。 |
| onnx / onnxruntime | 跟随依赖 | 导出校验与 runtime 检查 |
| librosa / soundfile / scipy / numpy | 跟随依赖 | 音频读写与重采样 |

固定版本后写入 `requirements.txt`，新机器按它复现。

## 使用前检查

- 确认 `~/mozart-assets/RVC/assets/hubert_base/` 含 `pytorch_model.bin` 与 `config.json`；缺失时补齐。
- 确认目标权重 `.pth` 与 `rmvpe.pt` 已就位。
- 首次使用先跑一个最小导出，确认导出链路可用。

## 自检

```bash
~/mozart-toolkit/rvc-env/bin/python -c "import torch, transformers, onnx, onnxruntime, librosa, soundfile, scipy; print('env ok')"
```