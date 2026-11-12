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
- 启动方式：CLI 直接 spawn 控制 daemon；daemon 拉起并监管 pre、rvc 等服务进程。不依赖 systemd，systemd 部署留到流水线阶段再议。daemon spawn 子进程时传 `--socket`（控制面）与 `--config`（同一 TOML），子进程各读自己的配置段。
- `--config` 传的是 daemon 自己解析出的 TOML 路径；内置默认启动（无配置文件）时不传，子进程按同一解析顺序得到同一默认值。
- 部署布局：pre/rvc 二进制默认按 daemon 可执行文件同目录解析（`process.*_bin` 为裸名时）；rvc 的 CMake 构建产物复制到该目录，或用 `MOZART_RVC_BIN` 指绝对路径。
- 控制 socket 固定在 `run_dir`（默认 `/run/mozart`）：`daemon.sock`、`pre.sock`、`rvc.sock`、`asr.sock`、`bridge.sock`、`tts.sock`；daemon 用 `daemon.lock` 独占实例。运行目录由 daemon 收敛为 0700。
- daemon 平面命令：`Hello`、`GetStatus`、`RouteSet`、`MuteSet`、`Shutdown`。配置读 TOML：`--config` > `MOZART_CONFIG` > `./mozart.toml` > 内置默认；`MOZART_RUN_DIR`、`MOZART_PRE_BIN`、`MOZART_RVC_BIN`、`MOZART_DAEMON_BIN`、`MOZART_POLL_INTERVAL_MS` 覆盖。
- 配置持久化（模型甲）：TOML 是持久基线，运行时改动不自动写 TOML；只有显式写回动作（`config set` / `config save`）写 TOML，原子写。用户手改 TOML 随意，不做防冲突保护；手改在重启时被读取。
- 运行时状态文件：daemon 在持久数据目录（默认 `~/.local/state/mozart/`）维护运行时状态文件，记录运行时改动（字幕开关，将来 RVC 会话参数）。每次改动原子写（临时文件 + rename），崩溃也保留。启动时读 TOML 基线叠加运行时状态，得到生效配置再拉起服务下发。运行时状态文件是存档快照，不是第二份活权威；活权威在各服务，启动恢复一次，运行时不对账。
- 配置写回动作（均经 daemon，原子写 TOML）：`config set <键> <值>` 改单个键的持久基线；`config save` 把状态文件的全部运行时覆盖批量写进 TOML 成基线，随后清空状态文件覆盖。`config save` 不改生效值，无需重启。
- 配置键分类按机制 + 切换成本，能即时生效就归运行时：键值每次操作才读（阈值/上限/开关/超时）或 owning 服务有运行中应用命令 → 运行时；启动时固化进长活结构（打开的设备、绑定的 socket、预分配的环、加载的模型、spawn 的二进制、存储目录）→ 重启。`config set` 运行时键：写 TOML 基线、清该键状态覆盖、立即下发生效；重启键：只写 TOML、不动运行进程、返回需重启提示。逐键分类与机制依据见 [docs/config.md](../config.md)。
- RVC 会话参数运行时生效经 `StopStream`→`StartStream` 带新参：实时在跑立刻用新参（短暂打断）；文件任务在跑则当前任务用旧参跑完、新参下一任务生效，不中途废弃文件任务。干净的块边界 `SetParams` 仍见 [FUTURE.md](../FUTURE.md)。
- 重启提示契约：`config set` 重启生效键的响应带稳定字段 `restart_required` 与 `affected`（受影响服务）；daemon 判定，CLI 与以后 HTTP 适配器读同一字段提示，不各自实现。写了重启键后 TOML 与运行值分歧，`status` 显示该服务待重启。
- 重启两粒度：`restart <service>` 重启单个服务（小模块）；整栈重启（大模块）用 `down`/`up`。重启重读配置生效。
- 控制连接由独立线程处理，空闲 30 s 断开，慢客户端不阻塞其他命令；daemon 退出时先向子进程发 SIGTERM，2 s 后 SIGKILL。
- `mozart-ctl` v1 命令：`up`、`down`、`status`、`route set <direct|rvc>`、`mute on|off`、`config get|set|save`、`restart <service>`、`translate on|off`、`speech on|off|stop`、`file submit <path> [--model <id>]`、`file pause`、`file resume`、`file cancel <job_id>`、`file fetch <job_id> [-o path]`、`file status <job_id>`、`file list`、`model list`；`--json` 输出稳定数据；退出码：0 成功、1 其他错误、2 参数错、3 daemon 不可达、4 请求被拒、5 超时。
- 控制面命令覆盖字幕链路：字幕开关命令经 daemon 转发到桥执行，命令契约见 SPEC-05。daemon 不持有桥的活状态，只转发命令并轮询汇总。
- 模式建模用资源仲裁，不用大模式枚举：资源为 GPU 与扬声器；活动（实时变声、文件转换、翻译播报、直出）声明占用，daemon 仲裁，冲突按裁决判据处理；mode 是派生状态。继承旧原则：控制面不碰音频、GPU 互斥、文件队列串行加取消检查点、上限 50。
- v1 资源仲裁范围：先做 GPU 单资源（实时 / 文件互斥），扬声器仲裁留接口；完整仲裁与播报活动见 [FUTURE.md](../FUTURE.md)。
- 文件任务队列 v1 为有界内存队列（上限 50 沿用），不落盘；daemon/rvc 重启丢待转任务，不续转。落盘持久化见 [FUTURE.md](../FUTURE.md)。模式是资源仲裁的派生活状态，归 daemon 内存，不入配置状态文件，重启不恢复；会话参数实时与文件共用一份配置覆盖。
- 冲突裁决判据：等待能解决的接受意图并排队/挂起，不让用户重试；等待不能解决的拒绝带原因。文件提交遇 GPU 被实时占用 → 入文件队列等待；GPU 空闲且有排队任务 → daemon 自动启动转换，无需显式启动命令。文件任务执行中收到 `route set rvc` → 挂路由等待槽（单槽，后请求覆盖前请求），当前任务结束后切换，剩余文件任务继续排队。模型不存在、参数非法、daemon 关停中 → 拒绝。队列满 50 拒收是容量准入规则，独立于本判据。
- 文件队列暂停/恢复保留：`file pause` 置暂停标志，不再启动新任务，正在执行的任务跑完；`file resume` 清除标志继续。暂停是队列级布尔标志，独立于 GPU 仲裁。
- 文件任务按任务选模型保留：`file submit <path> [--model <id>]`，不带 `--model` 用当前已加载模型。rvc 增 `ListModels` 查询；daemon 入队时经它校验模型存在，不存在当场拒收（沿用旧行为）。执行时 `LoadModel` 仍可能失败（如模型损坏），失败标 failed 带原因、队列继续。
- 文件转换数据面中间层归 daemon（Rust）：解码文件为 16 kHz 帧喂给 rvc、收 48 kHz 输出帧编码为结果文件；文件期间 daemon 绑定输出 socket 作数据面对端。此路径离线，不在实时音频路径上，不影响实时延迟。
- 文件降噪保留（旧 `file_denoise`，默认关）：daemon 离线调用与 pre 同一套共享 DSP（SPEC-02）对文件先降噪再喂 rvc；实时链路降噪不受影响。
- 文件任务取消由 daemon 编排：排队中→移出队列、标 cancelled、删源副本；执行中→发 `StopStream` 给 rvc、停后标 cancelled。临时文件（解码裸音频、半成品输出）归 daemon 清理：取消/失败删除，成功改名发布；C++ 不碰文件。CLI `file cancel <job_id>`。
- 文件存储与结果生命周期：源副本与结果同放一个受管存储目录，默认 `~/.local/state/mozart/files/`（持久存储，不放 /run tmpfs），TOML 可配。结果原子发布：先写 `.part`、成功再 rename。驱逐沿用旧策略：每任务后目录总大小超上限（默认 1000 MiB，TOML 键）按时间戳从旧到新删至 80%，未完成任务的源/结果受保护，已完成结果可被删——结果是有限缓存，需及时取走。
- 取结果 v1 用 CLI `file fetch <job_id> [-o 路径]`，默认名原文件名主干 + `.wav`（输出固定 WAV）；HTTP 下载归 web 适配器，不沿用旧固定下载名 `mozart_output.wav`。
- 文件任务状态与进度：字段 `job_id` / `name` / `model_id` / `state` / `progress` / `error`，state 六态（queued / processing / cancelling / completed / failed / cancelled），completed 附结果可取标记（fetch 提示）；不带模式枚举（mode 派生）。进度 = 粗阶段（解码/喂入/推理/发布）+ 推理段用 rvc `GetStatus` 块/窗计数细化（daemon 每秒轮询已有）；依赖 rvc 文件模式给出有意义块计数，缺则退回粗阶段。CLI `file status <job_id>` 查单个、`file list` 看队列。
- 文件转换入口（v1）：CLI `file submit <path>` 提交本地路径；daemon 复制文件到受管存储目录后入队，队列只引用副本，原文件变动不影响排队任务。入口校验：扩展名白名单 `.wav` / `.mp3` / `.m4a` / `.aac` / `.flac` / `.ogg`，100 MiB 大小预检；300 s 时长上限在解码后检查。HTTP 上传归 web 适配器，流式写盘后经同一入队契约提交。
- 监控归 daemon，不独立进程：进程存活与崩溃重启；每秒轮询各服务 `GetStatus` 汇总业务指标与降级原因；收集子进程日志。
- 语言服务（识别、桥、TTS）各绑控制 socket，daemon 连接并每秒轮询，接法与 pre/rvc 一致。llama-server 不说控制协议：daemon 只管其进程存活与重启，业务可达性由桥的 `GetStatus` 带报。
- 监管退避：崩溃后从 500 ms 起倍增，上限 10 s；连续稳定运行 30 s 后重置。子进程日志追加到 `run_dir/logs/<name>.log`，daemon 退出前先停止子进程。
- 路由执行顺序：切到 rvc 先 `LoadModel` / `StartStream` 再让 pre `SetRoute(rvc)`，失败回滚停流；切回 direct 先 pre `SetRoute(direct)` 再 `StopStream` 并释放 GPU。轮询发现实际状态偏离期望时按同一顺序对账。
- 主机硬件指标（CPU/内存/GPU/可用性）由 daemon 读 `/proc`、`/sys` 采集（移植旧 `SystemMonitor`），并入 status。监控出口经 CLI `status`/`logs`，以后 HTTP 适配器推网页；历史时序、告警、面板推迟。
- RPC 超时（`rpc_timeout_ms`）覆盖 connect、send、recv 全程；控制通道任何一步不得无限阻塞，卡死的子进程不得拖住控制面。
- 子进程设 `PDEATHSIG` 后校验父进程仍存活（ppid 未变为 1），daemon 崩溃瞬间不留孤儿。
- 关停幂等：关停进行中再次收到 `Shutdown` 仍应答 ok；其余命令返回 `Busy`，原因为 "daemon is shutting down"。
- daemon 捕获 SIGTERM/SIGINT，走与 `Shutdown` 相同的优雅关停（停子进程、退出），幂等。运行时状态每次改动已落盘，关停无需额外写盘。

## 改进点

- 模式名与状态字段统一。
- 端口集中配置。

## 待讨论

- HTTP/SSE 适配器见前端模块。
