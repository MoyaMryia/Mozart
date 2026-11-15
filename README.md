# Mozart

面向 NVIDIA Jetson Orin Nano Super 8 GB 的本地语音处理系统。
一次麦克风采集同时用于实时变声与翻译字幕，另一种输出使用参考音色播报译文。
全部计算在设备本地完成，不上传音频。

## 当前状态

本仓库处于**重写阶段**，已实现音频、变声、文件转换与语言服务的基础链路。
旧实现归档在仓外，仅作参考，不作为依赖。归档路径见 [AGENTS.md](AGENTS.md)。

| 能力 | 状态 |
| --- | --- |
| 仓库骨架、AGENTS.md、CI | 已就绪 |
| 构建契约（Rust workspace、C++20、Presets、CTest） | 已就绪 |
| 业务组件 | 已有基础实现；实现进度不代表全部规格完成 |
| 板端测试 | 已有实时、语言与文件链路实测，范围见各报告 |
| 网页前端 | 后续 |

实测记录：[实时链路](reports/realtime-pipeline-20261112/RESULTS.md)、
[语言链路](reports/language-chain-20261113/RESULTS.md)、
[文件链路](reports/file-chain-20261113/RESULTS.md)。

运行时以 Rust 为主，推理层保留 C++ 进程，不依赖 Python。离线模型导出可以使用 Python，但不进入构建与部署路径。

## 目录约定

| 目录 | 职责 |
| --- | --- |
| `core/` | 契约帧、环形缓冲、Unix 数据报与控制协议、共享配置 |
| `dsp/` | pre 与 daemon 共用的音频 DSP |
| `pre/` | ALSA 采集与播放、DSP 调用、音频分发与路由 |
| `rvc/` | 语音转换推理，ONNX Runtime 或 TensorRT |
| `daemon/` | 进程生命周期、资源仲裁、文件任务与 Unix 控制面 |
| `ctl/` | CLI 控制入口 |
| `asr/` | 语音识别与复核 |
| `tts/` | 参考音色合成与播报帧输出 |
| `bridge/` | 字幕、翻译调度与播报投递 |
| `web/` | 控制台前端（后续） |
| `tests/` | 跨组件用例与小型夹具 |
| `cmake/` | 编译配置模块 |
| `tools/` | 构建、检查与离线导出脚本 |
| `docs/` | 长期维护的说明与手册 |
| `reports/` | 带日期的实测记录，只跟踪 Markdown |
| `.claude/`、`CLAUDE.md` | 拒绝在 Claude Code 下工作的阻断配置与声明 |

依赖方向：`core` 不依赖其他组件；其余 Rust crate 与 `rvc` 依赖 `core`。
`pre` 与 `daemon` 共用 `dsp`。
控制入口为 `mozart-ctl`，控制权威为 `daemon`。
网页 HTTP 适配器的边界见 [SPEC-04](docs/SPEC/SPEC-04-daemon.md)。

模型、音频和引擎等大资产不进仓库，路径由配置文件或环境变量给出。

## 工具链

| 项 | 要求 |
| --- | --- |
| 语言 | Rust（edition 2021，Cargo workspace）与 C++20（`rvc`） |
| 编译标准 | C++20，关闭编译器扩展 |
| 构建系统 | Rust：Cargo workspace；C++：CMake ≥ 3.22 + Ninja + CTest |
| 编译器缓存 | ccache，检测到即自动启用 |
| 预设 | `host-debug`、`host-asan`、`host-tsan`、`jetson-release` |

构建目录只使用预设名，不再手写构建参数：

```bash
cmake --preset host-debug
cmake --build --preset host-debug --parallel 4
ctest --preset host-debug
```

`host-tsan` 在 Linux 6.x 上需要先降低 ASLR 熵，否则 ThreadSanitizer 报
`unexpected memory mapping`：`sudo sysctl vm.mmap_rnd_bits=28`。

## 检查

本地安装 hook：

```bash
tools/install-hooks.sh
```

完整检查与 CI 使用同一脚本：

```bash
tools/ci-local.sh repo      # 仓库卫生：体积上限、索引与忽略规则一致
tools/ci-local.sh lint      # 暂存区的行尾空白、报告“适用范围”一节
tools/ci-local.sh native    # 配置、构建与 CTest
tools/ci-local.sh rust      # cargo check 与 cargo test
tools/ci-local.sh web       # 前端测试与构建
tools/ci-local.sh all       # 依次运行全部阶段
tools/ci-local.sh --staged  # 提交前检查，pre-commit hook 调用
```

CI 与本地 pre-commit 调用同一脚本的对应阶段。

没有对应组件的阶段会跳过并说明原因，因此骨架阶段也能全绿。

## 文档

| 文档 | 内容 |
| --- | --- |
| [TARGET.md](TARGET.md) | 产品目标、功能路径与验收原则 |
| [AGENTS.md](AGENTS.md) | 代理与提交者的工作约定 |
| [docs/SPEC/SPEC.md](docs/SPEC/SPEC.md) | 模块索引、跨模块分工与规格状态 |
| [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) | 赛事评价标准原文转换 |
| [docs/FUTURE.md](docs/FUTURE.md) | 第一版之后的提升点 |
| [docs/GUIDE.md](docs/GUIDE.md) | 配置与 CLI 使用指南 |
| [docs/config.md](docs/config.md) | TOML 配置 schema |

## 许可

MIT，见 [LICENSE](LICENSE)。
