# Jetson 开发环境

最后更新：2026-11-15。本文描述目标设备上已有的资产、模型、库与工具，供快速上手。
连接方式不在本仓库记录。资产与工具链的设计规范见 [SPEC/SPEC-10-env.md](SPEC/SPEC-10-env.md)。

## 平台

- 设备：NVIDIA Jetson Orin Nano Super 8 GB，ARM64，JetPack 6。
- 推理后端：TensorRT 10.16.2 与 ONNX Runtime。引擎绑设备，须在设备上编译。
- 音频：ALSA。无物理声卡时用 snd-aloop。

## 资产两层布局

原料层放待处理内容，成品层放可直接推理的整套资产。

| 层 | 路径 | 内容 |
| --- | --- | --- |
| 原料 | `~/rvc-pth/` | 原始权重 `<id>.pth`，待导出编译 |
| 成品 | `~/mozart-assets/models/rvc/<id>/` | config + 生成器 + 特征引擎引用 + index |

成品目录名 = 模型 id = 文件前缀。加载器按 `<id>` 前缀找文件，同名 `.engine` 优先于 `.onnx`。

## RVC 模型清单

成品层 `~/mozart-assets/models/rvc/`：

| 模型 | 离线 | 实时 | 备注 |
| --- | --- | --- | --- |
| qiqi-zh | 齐全 | 齐全 | 主模型，特征引擎共享源 |
| qiqi-en | 齐全 | 缺 | 无 front/decoder |
| de-narrator | 待编译 | 齐全 | 离线生成器 engine 缺，现回退 onnx |

原料层 `~/rvc-pth/` 另有 qiqi-ja、qiqi-ko、mike-tyson、liuying 权重，未导出编译。

## 共享特征引擎

HuBERT/RMVPE 所有 v2 模型共用，存于 qiqi-zh 目录，其余模型符号链接引用：

- `hubert_base_dynamic.onnx`/`.engine`：离线 HuBERT，engine 链接到 `hubert_base_64k.engine`。
- `hubert-realtime.onnx`/`.engine`：实时 HuBERT。
- `rmvpe_dynamic.onnx`/`.engine`：离线 RMVPE。
- `rmvpe-realtime.onnx`/`.engine`：实时 RMVPE。

`[rvc]` 的 `hubert_path` 等显式指向 qiqi-zh 目录。换模型只换生成器，特征引擎不动。

## 其他资产目录

`~/mozart-assets/models/` 下另有 `asr/`、`asr-review/`、`tts/`、`llm/`，供语言链（识别、播报、翻译）使用。

## runtime 配置

`~/mozart-runtime/`：

- `mozart.toml`：运行配置。注册 qiqi-zh、qiqi-en、de-narrator 三模型，`models_dir` 指成品层父目录。
- `bin/`：mozart-ctl、daemon、pre、rvc，同目录解析。
- `in/`、`out/`：手动测试输入输出。
- `run-remote.sh`：手动文件变声 harness（ctl up → submit → fetch）。
- `run/`、`state/files/`：运行时 socket、日志与文件任务缓存。

## 运行与测试

两套，都在设备上：

- 手动 harness：`~/mozart-runtime/run-remote.sh`，用 `mozart.toml`，跑固定输入。
- 自动测试：仓库 `tools/jetson-{file-test,smoke,measure,language-test}.sh`，自生成配置到 `/tmp/mozart-*`，用 `~/Mozart/target/debug` 二进制。

常用命令见 [GUIDE.md](GUIDE.md)。模型切换：`mozart-ctl file submit <wav> --model <id>`。

## 工具链

Python 脚本，居仓库 `tools/model/`，直接调用，不做 CLI 封装。
原则：**逻辑在仓库，资产在仓外，路径全部由参数或环境变量注入**，脚本不写死资产位置。

| 脚本 | 作用 |
| --- | --- |
| `export_generator_onnx.py` | 生成器 pth → onnx（离线，固定帧数） |
| `export_generator_split_onnx.py` | 生成器 pth → front/decoder onnx（实时拆分） |
| `export_hubert_onnx.py` | HuBERT base → onnx |
| `export_rmvpe_onnx.py` | RMVPE → onnx |
| `build_dynamic_engines.py` | onnx → engine（trtexec 包装，须在设备上） |
| `run_reference.py` | PyTorch 参考推理，保存各模型输入张量 |
| `verify_golden.py` | golden 对拍校验 |
| `compare_streaming_quality.py` | 流式输出质量对比（verify_golden 调用） |

### 外部资产依赖

工具链本身不含资产，运行需要以下外部输入：

| 依赖 | 用于 | 传入方式 | 位置 |
| --- | --- | --- | --- |
| RVC 源码树 | 导出、golden | `--rvc-source` / `RVC_SOURCE` 必填 | `~/mozart-archive/RVC` |
| HuBERT base 模型 | export_hubert、run_reference | `--hubert-path` 必填 | RVC 归档 assets |
| RMVPE 权重 | export_rmvpe、run_reference | `--rmvpe-path` 必填 | RVC 归档 assets |
| 目标权重 | export_generator、run_reference | 位置参数 / `--model` | `~/rvc-pth/<id>.pth` |
| golden 参考 | run_reference、verify_golden | `--input` / `--manifest` 等必填 | 仓外 |

导出依赖 PyTorch，在开发机运行；编译依赖 trtexec，在设备运行。RVC 源码树是上游实现，不是旧仓库代码。

### 调用示例

```bash
# 导出离线生成器（开发机）
python3 tools/model/export_generator_onnx.py ~/rvc-pth/qiqi-zh.pth \
    --rvc-source ~/mozart-archive/RVC --output qiqi-zh.onnx --frames 200

# 导出实时拆分生成器
python3 tools/model/export_generator_split_onnx.py ~/rvc-pth/qiqi-zh.pth \
    --rvc-source ~/mozart-archive/RVC \
    --front-output qiqi-zh-front.onnx --decoder-output qiqi-zh-decoder.onnx \
    --skip-head 250 --return-frames 30

# 编译引擎（设备）
python3 tools/model/build_dynamic_engines.py --assets <onnx目录> --feature-assets <特征onnx目录>

# golden 参考与对拍
RVC_SOURCE=~/mozart-archive/RVC python3 tools/model/run_reference.py \
    --input in.wav --model ~/rvc-pth/qiqi-zh.pth \
    --hubert-path <hubert_base目录> --rmvpe-path <rmvpe.pt> \
    --output ref.wav --tensor-dir tensors --metadata reference.json
python3 tools/model/verify_golden.py --manifest golden_manifest.json
```

## 加新模型流程

1. 权重落入 `~/rvc-pth/<id>.pth`。
2. 开发机运行导出脚本，产出 onnx。
3. 设备上运行编译脚本，产出生成器 engine。特征引擎共享，不用重编。
4. 成品层建 `<id>/` 目录，放 config 与生成器，登记 `[[models]]`。
5. 测试：`file submit --model <id>`，按需 golden 对拍。

## 适用范围

- 模型状态与资产布局为 2026-11-15 板端核对。模型切换经 qiqi-en、de-narrator 短文件转换实测通过。
- asr、tts、llm 资产内容未逐项核对。
- de-narrator 离线 engine、qiqi-en 实时资产未补齐。
- 工具链脚本已迁入并通过语法检查；功能运行需 PyTorch 与 RVC 源码树，未在本文档环境实跑。