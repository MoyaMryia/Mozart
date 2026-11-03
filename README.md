# Mozart

面向 NVIDIA Jetson Orin Nano Super 8 GB 的本地语音处理系统。
一次麦克风采集同时用于实时变声与翻译字幕，另一种输出使用参考音色播报译文。
全部计算在设备本地完成，不上传音频。

## 当前状态

本仓库处于**重写阶段**，只有骨架、约定和 CI。
旧实现归档在 `~/Projects/Project-Mozart`，仅作参考，不作为依赖。

| 能力 | 状态 |
| --- | --- |
| 仓库骨架、AGENTS.md、CI | 已就绪 |
| 组件代码 | 未开始 |
| 目标设备验收 | 未开始 |

## 目录约定

| 目录 | 职责 |
| --- | --- |
| `core/` | 契约帧、环形缓冲、传输层（UDP、Unix 数据报） |
| `pre/` | ALSA 采集与播放、降噪、重采样、变速 |
| `rvc/` | 语音转换推理，ONNX Runtime 或 TensorRT |
| `daemon/` | 进程生命周期、模式切换、HTTP 与控制面 |
| `web/` | 控制台前端 |
| `tools/` | Python 服务与脚本：识别、翻译、合成、USB 桥 |
| `tests/` | 跨组件用例与小型夹具 |
| `docs/` | 长期维护的说明与手册 |
| `reports/` | 带日期的实测记录，只跟踪 Markdown |

依赖方向：`core` 不依赖其他组件；`pre`、`rvc`、`daemon` 依赖 `core`；
`daemon` 是唯一对外提供接口的进程；`tools` 通过 `daemon` 的接口协作；
`web` 只调用 `daemon` 的 HTTP 接口。

模型、音频和引擎等大资产不进仓库，路径由配置文件或环境变量给出。

## 检查

本地安装 hook：

```bash
tools/install-hooks.sh
```

完整检查与 CI 使用同一脚本：

```bash
tools/ci-local.sh repo      # 仓库卫生：体积上限、索引与忽略规则一致
tools/ci-local.sh lint      # 文档与代码风格
tools/ci-local.sh native    # C/C++ 构建与单元测试
tools/ci-local.sh python    # Python 单元测试
tools/ci-local.sh web       # 前端测试与构建
```

没有对应组件的阶段会跳过并说明原因，因此骨架阶段也能全绿。

## 文档

| 文档 | 内容 |
| --- | --- |
| [TARGET.md](TARGET.md) | 产品目标、功能路径与验收原则 |
| [AGENTS.md](AGENTS.md) | 代理与提交者的工作约定 |

## 许可

MIT，见 [LICENSE](LICENSE)。
