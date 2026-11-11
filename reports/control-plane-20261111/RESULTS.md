# 控制面实测（Jetson）

## 结论

控制面在 Jetson 上通过整机冒烟：`up` / `down` / `status` / `route set` / `mute`、崩溃重启、进程清理与退出码 0/2/3/4 符合预期。
pre/rvc 使用协议桩，音频与推理未参与本次验证。

## 环境

- 设备：Jetson Orin Nano Super 8 GB，JetPack 6，aarch64
- 提交：8938f29
- 二进制：`target/debug/mozart-daemon`、`target/debug/mozart-ctl`、`target/debug/examples/{pre_stub,rvc_stub}`
- 配置：poll 200 ms、rpc 500 ms、`rvc.model_id = "demo"`，运行目录为临时目录
- 测试：`cargo test --workspace` 在 Jetson 上全过（core 12、ctl 3、daemon 14）

## 步骤与结果

| 步骤 | 结果 |
| --- | --- |
| `mozart-ctl up` | `daemon started`，控制 socket 就绪 |
| 运行目录权限 | 预置 0755 的运行目录在 `up` 后收敛为 0700（`stat` 实测），`down` 后保持 |
| 再次 `up` | `daemon already running`，退出码 0（幂等） |
| `up` 后立即 `status` | 短暂显示 `degraded pre/rvc status: No such file or directory`；子进程 socket 就绪后清空 |
| 稳态 `status` | 两进程 `running`；CPU 0.0%，内存 794/7546 MiB，GPU `available=true`、负载 0.0% |
| `route set rvc` | `route: rvc`；`route.active=rvc`、`gpu=realtime`、`rvc.state=streaming`、`pre.route=rvc` |
| `mute on` | `mute: on`；`pre.mute=true` |
| `kill -9` pre 进程 | 重启后 pid 39886→39903，`restarts=1`，`last_exit.signal=9`；路由与静音保持 rvc / on |
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
| daemon 单一权威 + CLI 瘦客户端 + socket | `ctl/src/main.rs`、`daemon/src/server.rs` | 冒烟全流程；控制连接线程化，慢客户端不阻塞其他命令 |
| CLI 名词-动词语法 | `ctl/src/main.rs` | `route set`、`mute on` |
| stdout 数据 / stderr 日志、`--json`、退出码 | `ctl/src/main.rs` | 冒烟与 CLI 用例覆盖退出码 0/2/3/4 |
| clap + serde_json | `ctl/Cargo.toml` | 构建通过 |
| CLI spawn daemon 启动 | `ctl/src/main.rs` 的 `up` | 冒烟；启动期退出检测 |
| 资源仲裁 v1：GPU 单资源 | `daemon/src/state.rs` 的 `commit_route` | 冒烟 `gpu.owner` 在 realtime/null 间切换 |
| 监控归 daemon：进程、轮询、日志 | `daemon/src/supervisor.rs`、`daemon/src/poller.rs` | 冒烟状态与日志文件 |
| 硬件指标读 /proc、/sys | `daemon/src/hwmon.rs` | 冒烟 CPU、内存、GPU 可用性与负载 |
| 监管退避 500 ms 起倍增、上限 10 s | `daemon/src/supervisor.rs` | 配置默认值；冒烟重启 |
| 路由执行顺序与轮询对账 | `daemon/src/services.rs` | 单测 `linkage`、`control_server` |
| socket 布局、实例锁、配置来源 | `daemon/src/server.rs`、`core/src/config.rs` | 单测 `second_instance_is_refused`；冒烟（含目录权限收敛） |
| CLI 命令面与退出码定义 | `ctl/src/main.rs` | 冒烟与 CLI 用例 |

## 适用范围

- 配置：Jetson Orin Nano Super 8 GB，JetPack 6，aarch64；单次冒烟。
- pre/rvc 为协议桩：音频采集、DSP、变声推理与物理回录未验证。阻塞原因：pre/rvc 模块尚未实现；解除条件：对应模块交付后按同一流程复测。
- `/run/mozart` 实际部署路径未验证。阻塞原因：本次使用临时运行目录；解除条件：部署阶段在目标用户下创建该目录并复测（0700 收敛已实测，见上表）。
- systemd 部署未验证。阻塞原因：部署方式留到流水线阶段；解除条件：SPEC-09 确定后补测。
- HTTP/SSE 适配器未实现。阻塞原因：v1 范围外；解除条件：前端模块接入时验证。
- 退出码 5（超时）未实测。阻塞原因：需要可控的 daemon 超时场景；解除条件：加入延迟桩后补测。
