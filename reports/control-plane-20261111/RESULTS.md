# 控制面实测（Jetson）

## 结论

控制面在 Jetson 上通过整机冒烟：`up` / `down` / `status` / `route set` / `mute`、崩溃重启、退出码与进程清理均符合预期。
pre/rvc 使用协议桩，音频与推理未参与本次验证。

## 环境

- 设备：Jetson Orin Nano Super 8 GB，JetPack 6，aarch64
- 提交：e059177
- 二进制：`target/debug/mozart-daemon`、`target/debug/mozart-ctl`、`target/debug/examples/{pre_stub,rvc_stub}`
- 配置：poll 200 ms、rpc 500 ms、`rvc.model_id = "demo"`，运行目录为临时目录

## 步骤与结果

| 步骤 | 结果 |
| --- | --- |
| `mozart-ctl up` | `daemon started`，控制 socket 就绪 |
| 再次 `up` | `daemon already running`，退出码 0（幂等） |
| `status --json` | 两进程 `running`；硬件指标 CPU 1.65%，内存 688/7546 MiB，GPU `available=true` |
| `route set rvc` | `route: rvc`；状态显示 `route.active=rvc`、`gpu=realtime`、`rvc.state=streaming`、`pre.route=rvc` |
| `mute on` | `mute: on`；`pre.mute=true` |
| `kill -9` pre 进程 | 重启后 pid 38336→38349，`restarts=1`，`last_exit.signal=9`；路由与静音保持 rvc / on |
| `route set direct` | `route: direct`；GPU 释放，rvc 回到 `loaded` |
| `down` | `daemon stopped`，socket 移除 |
| `down` 后再 `status` | 退出码 3（daemon 不可达） |
| 再次 `down` | `daemon not running`，退出码 0（幂等） |
| 残留进程检查 | 无 daemon、无 pre/rvc 桩进程 |

## SPEC-04 已确认清单对照

| 条目 | 实现位置 | 验证方式 |
| --- | --- | --- |
| daemon 独立进程，不在音频数据路径 | `daemon/src/server.rs`、`daemon/src/services.rs` | 冒烟中 daemon 只收发控制消息 |
| daemon 拥有 pre/rvc 生命周期与重启重发 | `daemon/src/supervisor.rs`、`daemon/src/services.rs` | 单测 `reapplies_route_and_stream_after_restart`；冒烟 `kill -9` |
| daemon 是 C++ 控制消息唯一发送方 | `daemon/src/services.rs` | 代码路径唯一；pre/rvc 只被动应答 |
| 模式与路由策略归 daemon | `daemon/src/state.rs`、`daemon/src/services.rs` | 冒烟 `route set` |
| v1 不含 HTTP/SSE | 无实现 | 按规格 |
| daemon 单一权威 + CLI 瘦客户端 + socket | `ctl/src/main.rs`、`daemon/src/server.rs` | 冒烟全流程 |
| CLI 名词-动词语法 | `ctl/src/main.rs` | `route set`、`mute on` |
| stdout 数据 / stderr 日志、`--json`、退出码 | `ctl/src/main.rs` | 冒烟中 JSON 输出与退出码 0/3 |
| clap + serde_json | `ctl/Cargo.toml` | 构建通过 |
| CLI spawn daemon 启动 | `ctl/src/main.rs` 的 `up` | 冒烟 |
| 资源仲裁 v1：GPU 单资源 | `daemon/src/state.rs` 的 `commit_route` | 冒烟 `gpu.owner` 在 realtime/null 间切换 |
| 监控归 daemon：进程、轮询、日志 | `daemon/src/supervisor.rs`、`daemon/src/poller.rs` | 冒烟状态与日志文件 |
| 硬件指标读 /proc、/sys | `daemon/src/hwmon.rs` | 冒烟 CPU、内存、GPU 可用性 |
| 监管退避 500 ms 起倍增、上限 10 s | `daemon/src/supervisor.rs` | `config` 默认值；冒烟重启 |
| 路由执行顺序与轮询对账 | `daemon/src/services.rs` | 单测 `linkage`、`control_server` |
| socket 布局、实例锁、配置来源 | `daemon/src/server.rs`、`core/src/config.rs` | 单测 `second_instance_is_refused`；冒烟 |
| CLI 命令面与退出码定义 | `ctl/src/main.rs` | 冒烟 |

## 适用范围

- 配置：Jetson Orin Nano Super 8 GB，JetPack 6，aarch64；单次冒烟。
- pre/rvc 为协议桩：音频采集、DSP、变声推理与物理回录未验证。
- 未验证：真实 pre/rvc 进程与模型、`/run/mozart` 目录权限、systemd 部署、HTTP/SSE 适配器。
