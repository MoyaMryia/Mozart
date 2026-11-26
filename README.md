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
| [docs/ARCHITECTURE-HIGHLIGHTS.md](docs/ARCHITECTURE-HIGHLIGHTS.md) | 架构亮点：维护性、开发流程与 DevOps |
| [TARGET.md](TARGET.md) | 产品目标与验收原则 |
| [docs/FUTURE.md](docs/FUTURE.md) | 第一版之后的提升点 |
| [AGENTS.md](AGENTS.md) | 代理与提交者工作约定 |

## 前端

`web/` 提供 Vue/TypeScript 控制台，沿用旧界面的亮暗主题与分栏布局。
独立 Rust 进程 `mozart-web-adapter` 托管静态页面，并通过 HTTP/SSE 连接 daemon。
开发入口见 [web/README.md](web/README.md) 和 [web-adapter/README.md](web-adapter/README.md)，接口及命令面欠账见 [SPEC-06](docs/SPEC/SPEC-06-web.md)。
本机适配器阶段包含真实 daemon、真实 ctl 和协议桩的集成测试，不执行实际 RVC 推理。
板端已完成真实 RVC 文件、字幕恢复和翻译播报联调，见 [适配器报告](reports/web-adapter-20261117/RESULTS.md)。
`pre/` 已接入 WebRTC AEC3 在线声学回声消除（默认关闭，`pre.aec.enabled` 热切换），契约与多房间声学验收待办见 [SPEC-02](docs/SPEC/SPEC-02-pre.md)。

## 许可

MIT，见 [LICENSE](LICENSE)。
