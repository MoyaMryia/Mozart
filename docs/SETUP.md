# Jetson 开发环境与部署

最后更新：2026-11-15。本文是 Jetson 目录、资产、部署与测试的唯一权威。
连接方式不在仓库记录；部署时用参数或 `MOZART_JETSON` 提供 SSH 目标。
资产分层与工具链的设计规范见 [SPEC/SPEC-10-env.md](SPEC/SPEC-10-env.md)，TOML 字段见 [config.md](config.md)。

## 平台

- 设备：NVIDIA Jetson Orin Nano Super 8 GB，ARM64，JetPack 6。
- 推理后端：TensorRT 10.16.2 与 ONNX Runtime。引擎绑设备，须在设备上编译。
- 音频：ALSA。无物理声卡时用 snd-aloop。

## 目录规范

设备只使用下列活动目录。新增目录先写入本文。

```text
~/Mozart/                     # 部署源码与设备构建；deploy 管理，不手改
├── target/debug/             # Rust 构建（含 examples/rvc_stub）
├── build/jetson-release/     # C++/CTest 产物
└── tools/                    # 板端测试脚本（随源码同步）

~/.mozart-prev/               # 部署前源码快照（回滚锚点）

~/mozart-runtime/             # 运行时根；同一时间只跑这一份
├── mozart.toml               # 运行配置（唯一手改入口）
├── bin/                      # deploy 安装：7 个程序 + examples/rvc_stub
├── bin.prev/                 # 上一次成功部署的二进制（deploy 管理）
├── run/                      # socket、daemon.lock、logs/
├── state/                    # 持久状态（字幕、文件任务缓存）
├── in/                       # 手工测试输入
└── out/                      # 手工测试输出

~/mozart-assets/              # 成品资产与工具链依赖（只读）
├── lib/                      # sherpa 等共享库
├── llama/                    # llama-server 与 .so
├── RVC/                      # 上游 RVC 源码树（导出/golden 的 --rvc-source）
└── models/
    ├── rvc/<id>/
    ├── asr/<id>/
    ├── asr-review/<id>/
    ├── tts/<id>/
    └── llm/<file>.gguf

~/rvc-pth/                    # 原料权重（pth，不参与推理）
~/mozart-samples/             # 测试音频样本
~/mozart-archive/             # 归档：原始权重、基准引擎
└── legacy/                   # 历史实验目录，仅作追溯
```

规则：

- `~/Mozart` 由部署脚本管理，不在设备上手改源码。
- `~/mozart-runtime` 是唯一运行实例；配置只改 `mozart.toml`。
- `~/mozart-assets` 只读；测试只写 `/tmp/mozart-*` 与 `~/mozart-runtime/{run,state,out}`。
- 历史实验目录只放 `~/mozart-archive/legacy/`；旧报告中的路径视为历史记录。

## 工作流

本机执行，三条命令职责分开：

```bash
# 本机全量即时测试
tools/ci-local.sh all

# 部署：同步 → 构建 → 安装 → 重启；不跑测试
MOZART_JETSON=<SSH目标> tools/deploy-jetson.sh

# 部署后测试：设备端单测 + 功能测试，按套件可选
MOZART_JETSON=<SSH目标> tools/jetson-e2e.sh
```

提交前运行 `tools/ci-local.sh --staged`；pre-commit hook 调用同一脚本。

### 本机检查阶段

`tools/ci-local.sh` 按阶段拆分，也可单独运行：

| 阶段 | 内容 |
| --- | --- |
| `repo` | 仓库卫生：忽略规则一致、单文件不超过 5 MiB |
| `lint` | 文档行尾空白、报告“适用范围”一节 |
| `native` | `host-debug` 预设构建 + CTest |
| `rust` | `cargo check` + `cargo test`（全 workspace） |
| `web` | 前端测试与构建；`web/` 未建时跳过 |

### 部署行为

`tools/deploy-jetson.sh` 按以下顺序执行，任一步失败自动恢复：

1. 快照 `~/Mozart` 源码到 `~/.mozart-prev`。
2. rsync 本机仓库到 `~/Mozart`，排除 `target/`、`build/`、`.git/`。
3. 前拨源码 mtime 并删除 workspace 成员的 cargo 指纹，强制全量重编。
4. 构建 Rust 全部二进制与示例；构建 `jetson-release` C++ 产物；rvc 对齐到 `target/debug`。
5. 停止运行时并确认无残留进程。
6. 备份 `~/mozart-runtime/bin` 到 `bin.prev`；安装 7 个程序与 `examples/rvc_stub`。
7. 用 `~/mozart-runtime/mozart.toml` 重启，等待 `pre` 与 rvc 健康。
8. 写 `~/mozart-runtime/deploy.json`。

失败恢复分两段：

- 构建失败：恢复源码快照，清理 `target/debug` 部署产物，不打断正在运行的运行时。
- 安装或重启失败：从 `bin.prev` 恢复上一代二进制，重启旧栈。

`deploy.json` 记录提交号、是否有未提交改动、时间、构建预设和安装清单。

### 部署后测试

`tools/jetson-e2e.sh` 支持套件选择；不指定时默认 unit + smoke + file + language。

| 套件 | 脚本 | 内容 |
| --- | --- | --- |
| `--unit` | `tools/jetson-unit-test.sh` | 设备端 `cargo test --workspace` + `ctest` |
| `--smoke` | `tools/jetson-smoke.sh` | daemon + pre + rvc 三进程冒烟 |
| `--file` | `tools/jetson-file-test.sh` | 文件转换、取消、队列与入口校验 |
| `--language` | `tools/jetson-language-test.sh` | ASR → 翻译 → TTS 播报 |
| `--measure` | `tools/jetson-measure.sh` | 延迟、淡变、长跑、自愈测量 |

示例：

```bash
tools/jetson-e2e.sh <SSH目标> --unit
tools/jetson-e2e.sh <SSH目标> --smoke --file
tools/jetson-e2e.sh <SSH目标> --measure latency
```

功能套件开始前停止运行时，结束后自动重启。measure 需要 release 构建，部署只产 debug。
证据目录：`/tmp/mozart-smoke`、`/tmp/mozart-file-test`、`/tmp/mozart-language-test`；measure 证据在 `~/mozart-t7`。

## 资产布局

原料层放待处理内容，成品层放可直接推理的整套资产。

| 层 | 路径 | 内容 |
| --- | --- | --- |
| 原料 | `~/rvc-pth/` | 原始权重 `<id>.pth`，待导出编译 |
| 成品 | `~/mozart-assets/models/rvc/<id>/` | config + 生成器 + 特征引擎引用 + index |

成品目录名 = 模型 id = 文件前缀。加载器按 `<id>` 前缀找文件，同名 `.engine` 优先于 `.onnx`。

### RVC 模型清单

成品层 `~/mozart-assets/models/rvc/`：

| 模型 | 离线 | 实时 | 备注 |
| --- | --- | --- | --- |
| qiqi-zh | 齐全 | 齐全 | 主模型，特征引擎共享源 |
| qiqi-en | 齐全 | 缺 | 无 front/decoder |
| de-narrator | 待编译 | 齐全 | 离线生成器 engine 缺，现回退 onnx |

原料层 `~/rvc-pth/` 另有 qiqi-ja、qiqi-ko、mike-tyson、liuying 权重，未导出编译。

### 共享特征引擎

HuBERT/RMVPE 所有 v2 模型共用，存于 qiqi-zh 目录，其余模型符号链接引用：

- `hubert_base_dynamic.onnx`/`.engine`：离线 HuBERT，engine 链接到 `hubert_base_64k.engine`。
- `hubert-realtime.onnx`/`.engine`：实时 HuBERT。
- `rmvpe_dynamic.onnx`/`.engine`：离线 RMVPE。
- `rmvpe-realtime.onnx`/`.engine`：实时 RMVPE。

`[rvc]` 的 `hubert_path` 等显式指向 qiqi-zh 目录。换模型只换生成器，特征引擎不动。

### 其他资产

`~/mozart-assets/models/` 下另有 `asr/`、`asr-review/`、`tts/`、`llm/`，供语言链（识别、播报、翻译）使用。

## 运行配置

`~/mozart-runtime/mozart.toml` 是唯一手改入口。字段定义见 [config.md](config.md)。
手动文件变声 harness：`~/mozart-runtime/run-remote.sh`，执行 `ctl up → file submit → file fetch`。

当前运行配置只启用 pre、rvc 与文件转换。未配置 `[process.env]`、`[asr]`、`[tts]`、`[bridge]` 时，`status` 显示 asr/tts/llama 降级属预期；语言链由 e2e 的临时配置验证。

## 工具链

Python 脚本，居仓库 `tools/model/`，直接调用，不做 CLI 封装。
原则：**逻辑在仓库，资产在仓外，路径全部由参数或环境变量注入**，脚本不写死资产位置。

| 脚本 | 作用 |
| --- | --- |
| `export_generator_onnx.py` | 生成器 pth → onnx（离线，固定帧数） |
| `export_generator_split_onnx.py` | 生成器 pth → front/decoder onnx（实时拆分） |
| `export_hubert_onnx.py` | HuBERT base → onnx |
| `export_rmvpe_onnx.py` | RMVPE → onnx |
| `build_dynamic_engines.py` | 文件路径动态 onnx → engine（trtexec 包装，须在设备上） |
| `build_realtime_engines.py` | 实时定长 onnx → engine（front/decoder/hubert/rmvpe，设备上） |
| `run_reference.py` | PyTorch 参考推理，保存各模型输入张量 |
| `verify_golden.py` | golden 对拍校验 |
| `compare_streaming_quality.py` | 流式输出质量对比（verify_golden 调用） |

### 外部资产依赖

工具链本身不含资产，运行需要以下外部输入：

| 依赖 | 用于 | 传入方式 | 位置 |
| --- | --- | --- | --- |
| RVC 源码树 | 导出、golden | `--rvc-source` / `RVC_SOURCE` 必填 | `~/mozart-assets/RVC` |
| HuBERT base 模型 | export_hubert、run_reference | `--hubert-path` 必填 | RVC 归档 assets |
| RMVPE 权重 | export_rmvpe、run_reference | `--rmvpe-path` 必填 | RVC 归档 assets |
| 目标权重 | export_generator、run_reference | 位置参数 / `--model` | `~/rvc-pth/<id>.pth` |
| golden 参考 | run_reference、verify_golden | `--input` / `--manifest` 等必填 | 仓外 |

导出依赖 PyTorch，在开发机运行；编译依赖 trtexec，在设备运行。RVC 源码树是上游实现，不是旧仓库代码。

### 调用示例

```bash
# 导出离线生成器（开发机）
python3 tools/model/export_generator_onnx.py ~/rvc-pth/qiqi-zh.pth \
    --rvc-source ~/mozart-assets/RVC --output qiqi-zh.onnx --frames 200

# 导出实时拆分生成器（帧数须匹配实时契约：front 280 / decoder 30）
python3 tools/model/export_generator_split_onnx.py ~/rvc-pth/qiqi-zh.pth \
    --rvc-source ~/mozart-assets/RVC \
    --front-output qiqi-zh-front.onnx --decoder-output qiqi-zh-decoder.onnx \
    --front-frames 280 --decoder-frames 30 --skip-head 250 --return-frames 30

# 编译引擎（设备）
python3 tools/model/build_dynamic_engines.py --assets <onnx目录> --feature-assets <特征onnx目录>

# golden 参考与对拍
RVC_SOURCE=~/mozart-assets/RVC python3 tools/model/run_reference.py \
    --input in.wav --model ~/rvc-pth/qiqi-zh.pth \
    --hubert-path <hubert_base目录> --rmvpe-path <rmvpe.pt> \
    --output ref.wav --tensor-dir tensors --metadata reference.json
python3 tools/model/verify_golden.py --manifest golden_manifest.json
```

### 实时链路资产生成

实时链路用 4 个定长引擎，与文件路径的动态引擎不同。契约形状（`rvc/src/rvc/pipeline.cpp`）：

| 引擎 | 定长形状 |
| --- | --- |
| 生成器 front | feats `1×280×768` |
| 生成器 decoder | z `1×192×30` → audio |
| hubert-realtime | audio `1×44800` |
| rmvpe-realtime | mel `1×128×32` |

几何来源：窗 3840 样本、过去上下文 40000、淡化 2400、SOLA 搜索 480（`rvc/src/rvc/audio_worker.cpp`）。front 280 帧 = (40000+3840+960)/160；decoder 30 帧 = out_block 14400/480；skip-head 250 = 40000/160。

生成步骤：

1. 导出 front/decoder（每模型）：`export_generator_split_onnx.py --front-frames 280 --decoder-frames 30 --skip-head 250 --return-frames 30`。
2. 导出实时特征引擎（共享，一次）：`export_hubert_onnx.py --samples 44800`、`export_rmvpe_onnx.py --frames 32`。
3. 设备上编译定长引擎：

```bash
python3 tools/model/build_realtime_engines.py \
    --front qiqi-zh-front.onnx --decoder qiqi-zh-decoder.onnx \
    --hubert hubert-realtime.onnx --rmvpe rmvpe-realtime.onnx \
    --output-dir <成品目录>
```

`build_realtime_engines.py` 从 onnx 读回定长形状再调 trtexec，模型相关维度（inter_channels、source_dim）不写死。`build_dynamic_engines.py` 只管文件路径动态引擎，不覆盖实时。

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
- 目录规范与部署、测试流程于 2026-11-15 调整；历史目录归集后旧报告中的路径不再对应设备现状。
