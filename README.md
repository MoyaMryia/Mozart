# Mozart

面向 NVIDIA Jetson Orin Nano Super 8 GB 的本地语音处理系统。
一次麦克风采集同时用于实时变声、翻译字幕与翻译播报，全部计算在设备本地完成，不上传音频。

## 开发流程

```bash
tools/ci-local.sh all                    # 本机全量即时测试
tools/deploy-jetson.sh <SSH目标>          # 部署，不跑测试
tools/jetson-e2e.sh <SSH目标> [套件]      # 部署后测试
```

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/GUIDE.md](docs/GUIDE.md) | 配置概念与 CLI 操作 |
| [docs/SETUP.md](docs/SETUP.md) | Jetson 目录规范、资产、部署与测试（唯一权威） |
| [docs/config.md](docs/config.md) | TOML 配置 schema |
| [docs/SPEC/SPEC.md](docs/SPEC/SPEC.md) | 模块规格索引与跨模块分工 |
| [TARGET.md](TARGET.md) | 产品目标与验收原则 |
| [docs/FUTURE.md](docs/FUTURE.md) | 第一版之后的提升点 |
| [AGENTS.md](AGENTS.md) | 代理与提交者工作约定 |

## 许可

MIT，见 [LICENSE](LICENSE)。
