---
name: mozart-model-onboarding
description: 在 Mozart 里接入新的 RVC 音色模型：从原始 .pth 权重到最终 TensorRT engine 的完整流程（文件链与实时链），含导出环境搭建、模型配置、登记与清理，全链路在 Jetson 上完成。务必在用户提到「加新模型」「载入新音色」「新模型接入」「换模型」「onboard model」「model onboarding」，或要把某个 .pth 变成可推理资产、搭建导出环境、把下载来的声音加进 Mozart 时使用。即使用户只说「下了个模型怎么用上」「把这个声音加进来」也应触发。
---

# Mozart 模型接入

把一个新的 RVC 音色从原始权重变成可推理的成品资产。**全链路在 Jetson 上完成**：导出用 PyTorch，编译用 trtexec。

本 SKILL.md 是入口和流程顺序；细节在 `references/` 里，按需读取。规范的权威仍是 `docs/SETUP.md`（设备目录/工具链）和 `docs/SPEC/SPEC-10-env.md`（资产分层），本 skill 与它们不一致时以文档和代码为准，并回头修本 skill。

## 两种链路与最终资产

先明确目标产物：最终都是 `~/mozart-assets/models/rvc/<id>/` 下一套可直接加载的资产。特征引擎（HuBERT/RMVPE）所有 v2 模型共享，导出编译一次。

**文件转换链**（离线，动态形状）：

- `<id>/config.json`
- `<id>/<id>.onnx` + `<id>.engine`（动态生成器，engine 优先）
- 共享特征引擎：`hubert_base_dynamic.onnx`/`.engine`、`rmvpe_dynamic.onnx`/`.engine`
- 可选：`<id>.index`

**实时变声链**（定长形状）：

- `<id>/config.json`
- `<id>/<id>-front.onnx`/`.engine` + `<id>-decoder.onnx`/`.engine`
- 共享特征引擎：`hubert-realtime.onnx`/`.engine`、`rmvpe-realtime.onnx`/`.engine`
- 可选：`<id>.index`

两条链可只要一条，也可都要。只要文件链就只导动态生成器；要实时再加 front/decoder。

## 流程（8 步）

1. **确认场景**：只要文件转换，还是也要实时变声？已有特征引擎（HuBERT/RMVPE）可复用就不重编。
2. **搭环境**（仅首次/新机器）：建导出用 venv，装固定版本依赖。读 [references/environment.md](references/environment.md)。
3. **原料校验**：权重放 `~/rvc-pth/<id>.pth`；核对 `config.json`（采样率、emb、version=v2、f0）。命名统一小写连字符，**id = 目录名 = 文件前缀**。
4. **导出 pth→onnx**：文件链动态、实时链定长，参数不同。读 [references/pipeline.md](references/pipeline.md)。
5. **编译 onnx→engine**：动态引擎与实时定长引擎用不同脚本。读 [references/pipeline.md](references/pipeline.md)。
6. **组装成品 + 登记**：成品目录契约、`[[models]]` 注册。读 [references/assemble-test.md](references/assemble-test.md)。
7. **测试**：`mozart-ctl file submit <wav> --model <id>`；按需 golden 对拍。
8. **清理**：删中间产物/缓存/下载遗留。读 [references/assemble-test.md](references/assemble-test.md)。

## 参考文件

| 文件 | 何时读 |
| --- | --- |
| [references/environment.md](references/environment.md) | 首次在板端搭导出环境，或环境报错 |
| [references/pipeline.md](references/pipeline.md) | 导出与编译（含参数、版本推荐、已知缺口） |
| [references/assemble-test.md](references/assemble-test.md) | 组装成品目录、登记、测试、清理 |

## 前置

- 设备：Jetson。SSH 目标由参数或 `MOZART_JETSON` 提供，不写死。
- RVC 源码树：`~/mozart-assets/RVC`（`--rvc-source` / `RVC_SOURCE` 必填）。
- 基础模型：`~/mozart-assets/RVC/assets/{hubert_base,rmvpe}`（导出 HuBERT/RMVPE 时用；生成器导出不需要）。
- trtexec：`/usr/bin/trtexec`（系统自带，编译引擎用）。
- 原始权重：`~/rvc-pth/<id>.pth`。

## 常见坑（细节见 references）

- **transformers 锁 4.46.3**：导出脚本面向 4.x；5.x 用本地路径 `from_pretrained` 直接报错。另，HuBERT 导出需 `hubert_base` 含 `pytorch_model.bin`（板端当前缺失，见 environment.md）。
- **id 对齐**：加载器按 `<id>.onnx` 找文件，目录名/文件名前缀不一致会 `exists` 为假。
- **动态 vs 定长**：文件链动态形状，实时链定长形状，别混导混编。
- **引擎绑设备**：只在目标 Jetson 上 trtexec，别拿别处的 engine。
- **特征引擎共享**：换模型只换生成器，HuBERT/RMVPE 不动。
- **model not found**：只在推理层运行时报错，不做回退；登记前先确认 `<id>` 已成套。