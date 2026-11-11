# daemon：入口与控制面

状态：讨论中。

## 职责

配置、生命周期、HTTP/SSE、控制命令。

## 已确认

- daemon 是独立进程；基础链路共三个进程：daemon（Rust）、音频进程（Rust）、推理进程（C++）。daemon 不在音频数据路径上，不影响实时延迟。
- daemon 拥有 C++ 推理进程的生命周期：启动、监控、崩溃重启；重启后重发 `Hello` / `LoadModel` / `StartStream`。
- daemon 是 C++ 控制消息的唯一发送方；音频进程不参与 C++ 控制面。
- daemon 决定模式与路由策略；音频进程只执行切换、淡变和静音，不在热路径上做决策。
- v1 的 daemon 不含 HTTP/SSE；对外接口推迟到语言与前端模块。
- 控制面形式：daemon 单一权威 + Unix 控制 socket + CLI 瘦客户端（`mozart-ctl`）。CLI 只把命令翻译成控制消息，决策全在 daemon；程序集成直连 socket 用同一协议。网页接口以后作为独立 HTTP 适配器客户端接入，daemon 自身不起 HTTP；后续硬件控制复用同一协议与权威。
- 以 CLI 为统一启动入口，拉起 daemon 与各服务，便于 pipeline 封装。
- CLI 设计范式（参考 clig.dev 与 git plumbing/porcelain）：名词-动词语法 `mozart-ctl <对象> <动作>`，加整机聚合动词（up/down/status）。CLI 零决策，只解析、组消息、发 socket、格式化回复。
- 输出契约：stdout 放数据、stderr 放日志与错误；`--json` 的 schema 是稳定机器契约，人读输出可变；退出码分类（成功 / 连接失败 / 参数错 / 拒绝 / 超时）。
- 标志用全名，常用才给短名；非终端不出颜色与动画（认 `NO_COLOR`）；改状态要告知，危险操作确认或 `--force`，命令幂等。
- 实现：clap + serde_json；`mozart-ctl` 独立 crate，与 daemon 共用 core 的协议定义。网页适配器、硬件控制以后经同一 CLI 与 socket 协议接入。
- 启动方式：CLI 直接 spawn 控制 daemon；daemon 拉起并监管 pre、rvc 等服务进程。不依赖 systemd，systemd 部署留到流水线阶段再议。
- 控制 socket 固定在 `run_dir`（默认 `/run/mozart`）：`daemon.sock`、`pre.sock`、`rvc.sock`；daemon 用 `daemon.lock` 独占实例。
- daemon 平面命令：`Hello`、`GetStatus`、`RouteSet`、`MuteSet`、`Shutdown`。配置读 TOML：`--config` > `MOZART_CONFIG` > `./mozart.toml` > 内置默认；`MOZART_RUN_DIR`、`MOZART_PRE_BIN`、`MOZART_RVC_BIN`、`MOZART_POLL_INTERVAL_MS` 覆盖。
- `mozart-ctl` v1 命令：`up`、`down`、`status`、`route set <direct|rvc>`、`mute on|off`；`--json` 输出稳定数据；退出码：0 成功、1 其他错误、2 参数错、3 daemon 不可达、4 请求被拒、5 超时。
- 模式建模用资源仲裁，不用大模式枚举：资源为 GPU 与扬声器；活动（实时变声、文件转换、翻译播报、直出）声明占用，daemon 仲裁，冲突拒绝或挂等待槽；mode 是派生状态。继承旧原则：控制面不碰音频、GPU 互斥、文件队列串行加取消检查点、上限 50。
- v1 资源仲裁范围：先做 GPU 单资源（实时 / 文件互斥），扬声器仲裁留接口；完整仲裁与播报活动见 [FUTURE.md](../FUTURE.md)。
- 监控归 daemon，不独立进程：进程存活与崩溃重启；每秒轮询各服务 `GetStatus` 汇总业务指标与降级原因；收集子进程日志。
- 监管退避：崩溃后从 500 ms 起倍增，上限 10 s；连续稳定运行 30 s 后重置。子进程日志追加到 `run_dir/logs/<name>.log`，daemon 退出前先停止子进程。
- 路由执行顺序：切到 rvc 先 `LoadModel` / `StartStream` 再让 pre `SetRoute(rvc)`，失败回滚停流；切回 direct 先 pre `SetRoute(direct)` 再 `StopStream` 并释放 GPU。轮询发现实际状态偏离期望时按同一顺序对账。
- 主机硬件指标（CPU/内存/GPU/可用性）由 daemon 读 `/proc`、`/sys` 采集（移植旧 `SystemMonitor`），并入 status。监控出口经 CLI `status`/`logs`，以后 HTTP 适配器推网页；历史时序、告警、面板推迟。

## 改进点

- 模式名与状态字段统一。
- 端口集中配置。

## 待讨论

- daemon 与音频进程之间的控制通道：命令集、传输、状态上报。契约写在 SPEC-02（音频进程是提供方）。
- daemon 与 api 是否合并。
- 控制面协议范围。
