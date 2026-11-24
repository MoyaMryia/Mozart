# daemon：入口与控制面

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。

## 职责

配置、生命周期、资源仲裁、文件任务与 Unix 控制面。网页 HTTP/SSE 适配器的边界见下文已确认条款。

## 已确认

- [x] daemon 是独立进程；基础链路共三个进程：daemon（Rust）、音频进程（Rust）、推理进程（C++）。daemon 不在音频数据路径上，不影响实时延迟。
- [x] daemon 拥有 C++ 推理进程的生命周期：启动、监控、崩溃重启；重启后重发 `Hello` / `LoadModel` / `StartStream`。（`daemon/src/supervisor.rs`）
- [x] daemon 是 C++ 控制消息的唯一发送方；音频进程不参与 C++ 控制面。
- [x] daemon 决定模式与路由策略；音频进程只执行切换、淡变和静音，不在热路径上做决策。
- [x] v1 的 daemon 不含 HTTP/SSE；对外接口推迟到语言与前端模块。
- [x] 控制面形式：daemon 单一权威 + Unix 控制 socket + CLI 瘦客户端（`mozart-ctl`）。CLI 只把命令翻译成控制消息，决策全在 daemon；程序集成直连 socket 用同一协议。网页接口作为独立 HTTP 适配器客户端接入，daemon 自身不起 HTTP；后续硬件控制复用同一协议与权威。
- [x] 以 CLI 为统一启动入口，拉起 daemon 与各服务，便于 pipeline 封装。
- [x] CLI 设计范式（参考 clig.dev 与 git plumbing/porcelain）：名词-动词语法 `mozart-ctl <对象> <动作>`，加整机聚合动词（up/down/status）。CLI 零决策，只解析、组消息、发 socket、格式化回复。（`ctl/src/main.rs:36-197`）
- [x] 输出契约：stdout 放数据、stderr 放日志与错误；`--json` 的 schema 是稳定机器契约，人读输出可变；退出码分类（成功 / 连接失败 / 参数错 / 拒绝 / 超时）。（`ctl/src/main.rs:18-21`，退出码 0/1/2/3/4/5）
- [x] 标志用全名，常用才给短名；非终端不出颜色与动画（认 `NO_COLOR`）；改状态要告知，危险操作确认或 `--force`，命令幂等。
- [x] 实现：clap + serde_json；`mozart-ctl` 独立 crate，与 daemon 共用 core 的协议定义。网页适配器经控制 socket 接入，生命周期操作调用可信 CLI；硬件控制后续复用协议。
- [x] 启动方式：CLI 直接 spawn 控制 daemon；daemon 拉起并监管 pre、rvc 等服务进程。不依赖 systemd，systemd 部署留到流水线阶段再议。daemon spawn 子进程时传 `--socket`（控制面）与 `--config`（同一 TOML），子进程各读自己的配置段。（`daemon/src/supervisor.rs`）
- [x] `--config` 传的是 daemon 自己解析出的 TOML 路径；内置默认启动（无配置文件）时不传，子进程按同一解析顺序得到同一默认值。（`daemon/src/supervisor.rs:260-269`）
- [x] 部署布局：所有运行时二进制安装到 `~/mozart-runtime/bin`，daemon 按自身可执行文件同目录解析 pre/rvc（`process.*_bin` 为裸名时）；rvc 的 CMake 产物复制到同一目录，或用 `MOZART_RVC_BIN` 指绝对路径。目录规范见 [SETUP.md](../SETUP.md)。（`core/src/config.rs:504`）
- [x] 控制 socket 固定在 `run_dir`（默认 `/run/mozart`）：`daemon.sock`、`pre.sock`、`rvc.sock`、`asr.sock`、`bridge.sock`、`tts.sock`；daemon 用 `daemon.lock` 独占实例。运行目录与 `run_dir/logs` 统一由 `ctl` 与 `daemon` 经共用入口收敛为 `0700`，避免默认 `umask 0002` 与 `web-service.sh` 目录校验冲突。（`core/src/config.rs`）
- [x] daemon 平面命令：`Hello`、`GetStatus`、`RouteSet`、`MuteSet`、`Shutdown`。配置读 TOML：`--config` > `MOZART_CONFIG` > `./mozart.toml` > 内置默认；`MOZART_RUN_DIR`、`MOZART_PRE_BIN`、`MOZART_RVC_BIN`、`MOZART_DAEMON_BIN`、`MOZART_POLL_INTERVAL_MS` 覆盖。（`daemon/src/server.rs`）
- [x] 配置持久化（模型甲）：TOML 是持久基线，运行时改动不自动写 TOML；只有显式写回动作（`config set` / `config save`）写 TOML，原子写。用户手改 TOML 随意，不做防冲突保护；手改在重启时被读取。（toml_edit 原子写）
- [x] 运行时状态文件：daemon 在持久数据目录（默认 `~/.local/state/mozart/`，`MOZART_STATE_DIR` 可覆盖）维护 `state.json`，当前记录字幕开关的运行时覆盖（`bridge.translate_enabled` / `bridge.speech_enabled`）；`config set rvc.*` 与 `preset use` 直接写 TOML 基线并经 `SetParams`（或旋钮变化时 `StopStream`→`StartStream`）下发，不进 `state.json`。每次改动先更新内存覆盖再原子写盘（临时文件 + rename）；写盘失败只记 `degraded` 不阻断命令，桥重启仍由内存覆盖恢复，仅 daemon 自身重启失去未落盘更改。启动时读 TOML 基线叠加运行时状态，得到生效配置再拉起服务下发（字幕开关覆盖待桥就绪后下发一次；`Poller` 在桥进程重启且 pid 变化时重新从内存状态覆盖下发）。运行时状态文件是存档快照，不是第二份活权威；活权威在各服务，启动与桥重启恢复，运行时不对账。桥已不再自写 `bridge-state.json`。（`daemon/src/state_file.rs`、`daemon/src/poller.rs`、`daemon/src/server.rs`）
- [x] 配置写回动作（均经 daemon，原子写 TOML）：`config set <键> <值>` 改单个键的持久基线（设备两键 + daemon/file 运行时键 + RVC 会话参数）；`config save` 把状态文件的全部运行时覆盖批量写进 TOML 成基线，随后清空状态文件覆盖，不改生效值、无需重启（`ctl/src/main.rs` ConfigAction::Save、`daemon/src/server.rs` config_save_reply、命令号 0x011C）。
- [x] config set 实现范围：重启类设备键 `pre.capture_device` / `pre.playback_device`——原子写基线（toml_edit 保留注释）、对照当前设备清单校验存在（防呆，设备级；子设备号范围由 pre 打开时验证）、回 `restart_required` + `affected`；设备键分类单一来源 `core::config::device_key_kind`。daemon/file 运行时键写基线后清该键状态覆盖并即时下发（见下条分类）。RVC 会话参数写基线后由 daemon 按写入的键编排 Stop/Start，或只更新生效配置（见下条）。当前支持设备键、daemon/file 运行时键、RVC 会话参数与 `[rvc.realtime]` 四键；pre/bridge 运行时键的对端下发未接入，`config set` 明确拒绝。`config get` 读 TOML 基线（不含环境变量覆盖与运行时状态）。（`core/src/config.rs` set_key、`daemon/src/server.rs` config_set_reply）
- [x] 配置键分类按机制 + 切换成本，能即时生效就归运行时：键值每次操作才读（阈值/上限/开关/超时）或 owning 服务有运行中应用命令 → 运行时；启动时固化进长活结构（打开的设备、绑定的 socket、预分配的环、加载的模型、spawn 的二进制、存储目录）→ 重启。`config set` 运行时键：写 TOML 基线、清该键状态覆盖、立即下发生效；重启键：只写 TOML、不动运行进程、返回需重启提示。逐键分类与机制依据见 [docs/config.md](../config.md)。（分类已定稿入库；daemon/file 运行时键的下发已实现：写基线后重建生效配置共享句柄，服务侧每次操作读最新快照；RVC 会话参数由 daemon 编排 Stop/Start，成功后才替换生效配置（见下条）；pre/bridge 运行时键的对端下发命令未接入，`config set` 明确拒绝）
- [x] RVC 会话参数（`pitch_shift`、`index_rate`、`rms_mix_rate`、`protect`、`f0_method`）归运行时键，运行时生效经 `config set`：写 TOML 基线后由 daemon 按变更键分流走 `SetParams` 块边界无中断更新、Stop/Start 重建或只更新生效配置，判据与编排见下条。
- [x] `config set rvc.<会话参数>` 与 `preset use` 编排：写 TOML 基线。实时流在跑时按本次写入键分流：决定流缓冲结构的 `[rvc.realtime]` 旋钮（`startup_delay_ms`，以及锁定中的 `crossfade_ms`、`sola_search_ms`）变化时执行 `StopStream`→`StartStream`（短暂打断，失败用旧参恢复）；可热更集（`pitch_shift`、`index_rate`、`f0_method`、`realtime.skip_silence`）变化时下发 `SetParams`（`0x0308`）在下一推理块边界无中断生效（失败时在跑流保持旧参，daemon 保留旧生效配置并报错）。判据单点在 `realtime_restart_changed` 与 `realtime_hot_params_changed`（`daemon/src/services.rs`）。设置 `protect`、`rms_mix_rate` 或其他键不触发 `rvc` 命令，即使存在未生效的其他 RVC 基线；值未变化也不触发。仅改 `protect`、`rms_mix_rate` 时只更新 TOML 基线与生效配置。应用成功才更新 daemon 生效配置；失败保留旧生效配置与 `status.rvc_params` 旧值，TOML 保留新基线。失败恢复后不自动重试；下一次同键写入或 daemon 重启时应用 TOML 新基线。rvc 不可达时无法确认实时流，不打断，只更新生效配置，并记录可查询的持久降级原因 `rvc session params: rvc status unavailable; config updated, stream not verified`；Stop/Start 路径旧参恢复起流遇 Busy 视为流仍在跑但参数不确定，同样记录持久降级原因。文件任务在跑不中断，当前任务用旧快照、下一任务用新参；RVC 参数快照在 GPU 交接后、`LoadModel` 成功并写入预设基线后取，交接口之后的改动下一任务生效（SPEC-11）。direct/空闲只更新配置。`status.rvc_params` 表示当前生效配置；存在待生效基线、rvc 不可达或文件任务在跑（当前任务用旧快照）时，它可能不等于正在跑的流参数。
- [x] 模型预设（SPEC-11）：`preset save/use/list` 命令（0x011D–0x011F）与载入编排见 [SPEC-11](SPEC-11-preset.md)。daemon 记录当前 `[rvc]` 参数来自哪个模型预设；模型切换时先读并校验该模型 `presets/default.toml`，`LoadModel` 成功后写基线（实时路由与文件链同序），复用 `config set rvc.<会话参数>` 的基线写回与 Stop/Start 判据；文件链参数快照后移到交接后。预设目录无法解析时降级用 live 参数并记录原因。`model use` 是唯一写 `[rvc].model_id` 基线的路径（见下条）。
- [x] 换模型命令 `model use <id>`（`DAEMON_MODEL_USE` 0x0120）：体 `{"model_id"}`，data `{"model_id","previous_model_id"}`。`model_id` 为空回 InvalidArgument；文件任务占 GPU 时回 Busy「cannot switch realtime model while file job owns GPU」。`config set rvc.model_id` 仍拒绝，只有本命令改它。CLI 用长超时 `config_set_timeout_ms`：预算 7 次串行 RPC、下限 15 s，覆盖失败恢复路径。（`core/include/mozart/ctl.h:72`、`core/src/ctl/schema.rs:523-528`、`daemon/src/server.rs:490-506`、`daemon/src/services.rs:978-996`、`ctl/src/main.rs:24-28,645-664,1103-1109`）
- [x] `model use` 编排为断流切换，全程持 transition 锁：resolve 读校验目标预设 → `StopStream` → `LoadModel` → 预设提交 → `StartStream`。选断流是为避免 8 GB 设备双引擎驻留 OOM，并支持跨采样率重建 `StreamingRvc`。读校验在停流前，损坏预设不打断在跑的流。提交在同一把 toml 锁内先写 `[rvc].model_id` 基线再写五键基线，然后重建生效配置、编排、记录 applied，并返回 `SwitchUndo` 回退快照。提交时流已停，编排只更新配置，成功路径复用 `LoadModel` 的 handle 只起一次流。两步契约见 [SPEC-11](SPEC-11-preset.md)，推理侧见 SPEC-03。（`daemon/src/services.rs:978-1059`、`daemon/src/preset.rs:139-195`）
- [x] `model use` 失败一律用旧模型 + 旧参数恢复流，不套用目标预设参数。起流失败先按 `SwitchUndo` 整体回退 TOML 基线、生效配置与 applied，再恢复旧流。恢复失败记持久降级原因，前缀 `rvc model use`；恢复遇 Busy 记「restore busy, running params unknown」。唯一残留窗口是回退时 TOML 写回本身失败，记降级 `preset: model switch baseline restore: …; TOML baseline may still name the target model`。（`daemon/src/services.rs:1064-1106`、`daemon/src/preset.rs:348-359`）
- [x] 重启提示契约：`config set` 重启生效键的响应带稳定字段 `restart_required` 与 `affected`（受影响服务）；daemon 判定，CLI 与以后 HTTP 适配器读同一字段提示，不各自实现。写了重启键后 TOML 与运行值分歧，`status` 显示该服务待重启。（已实现：`ctl/src/main.rs` config_set 与 print_status；待重启集合见下条）
- [x] 写重启键后 `status` 的待重启显示：daemon 维护 `pending_restart` 集合（写重启键加入、`restart` 该服务受理后清除），`GetStatus` 应答带 `pending_restart` 字段，CLI 人读输出显示待重启提示（`daemon/src/state.rs`、`core/src/ctl/schema.rs` Status）。同一分歧另记键级 `pending_restart_keys`（键 → 尚未重启的受影响服务集合），`GetStatus` 与 `config get` 应答都带该键名数组（无则空数组）；`restart` 某服务只从键的服务集合里去掉它，全部受影响服务都重启后才删键。`config get` 人读输出在值后列出待重启键，单键查询在该键待重启时提示，按段查询（如 `config get pre`）在该段下有待重启键时提示。受影响服务取 `core::config::set_key` 的 `affected`，daemon 不重新推导。
- [x] 重启两粒度：`restart <service>` 重启单个服务（小模块）；整栈重启（大模块）用 `down`/`up`。`restart <service>` 重读 TOML 后重起该服务（应用 spawn 期键），但不更新 daemon 生效配置；手改的 RVC 会话参数仍需 daemon `down`/`up` 或下一次 `config set` 同键才生效。
- [x] 控制连接由独立线程处理，空闲 30 s 断开，慢客户端不阻塞其他命令；daemon 退出时先向子进程发 SIGTERM，2 s 后 SIGKILL。
- [x] `mozart-ctl` v1 命令：`up`、`down`、`status`、`route set <direct|rvc>`、`mute on|off`、`config get|set|save`、`preset save|use|list`、`restart <service>`、`translate on|off`、`speech on|off|stop`、`file submit <path> [--model <id>]`、`file pause`、`file resume`、`file cancel <job_id>`、`file fetch <job_id> [-o path]`、`file status <job_id>`、`file list`、`model list`、`model use <id>`、`device list`；`--json` 输出稳定数据；退出码：0 成功、1 其他错误、2 参数错、3 daemon 不可达、4 请求被拒、5 超时。（`ctl/src/main.rs`）
- [x] 控制面命令覆盖字幕链路：字幕开关命令经 daemon 转发到桥执行，命令契约见 SPEC-05。daemon 不持有桥的活状态，只转发命令并轮询汇总。（`daemon/tests/language.rs`）
- [x] 模式建模用资源仲裁，不用大模式枚举：资源为 GPU 与扬声器；活动（实时变声、文件转换、翻译播报、直出）声明占用，daemon 仲裁，冲突按裁决判据处理；mode 是派生状态。继承旧原则：控制面不碰音频、GPU 互斥、文件队列串行加取消检查点、上限 50。（`daemon/src/file/arbitrate.rs`、`daemon/src/services.rs:327-333`）
- [x] v1 资源仲裁范围：GPU 仲裁（实时 / 文件互斥）+ 扬声器分时（播报时静音实时、播完恢复，沿用旧实现）；播报经变声占 GPU 的仲裁与混音见 [FUTURE.md](../FUTURE.md)。
- [x] 文件任务队列 v1 落盘持久化（上限 50 沿用）；daemon/rvc 重启后续转待转任务（推理状态不保留，从头重跑），源副本已在受管存储目录持久。模式是资源仲裁的派生活状态，归 daemon 内存，不入配置状态文件，重启不恢复；RVC 会话参数实时与文件共用一份配置覆盖。（`daemon/src/file/queue.rs:76-109`、`daemon/tests/file_queue.rs:193`）
- [x] 冲突裁决判据：等待能解决的接受意图并排队/挂起，不让用户重试；等待不能解决的拒绝带原因。文件提交遇 GPU 被实时占用 → 入文件队列等待；GPU 空闲且有排队任务 → daemon 自动启动转换，无需显式启动命令。文件任务执行中收到 `route set rvc` → 挂路由等待槽（单槽，后请求覆盖前请求），当前任务结束后切换，剩余文件任务继续排队。参数非法、daemon 关停中 → 拒绝（模型缺失不拒收，执行时报错，见 SPEC-10）。队列满 50 拒收是容量准入规则，独立于本判据。
- [x] 文件队列暂停/恢复保留：`file pause` 置暂停标志，不再启动新任务，正在执行的任务跑完；`file resume` 清除标志继续。暂停是队列级布尔标志，独立于 GPU 仲裁。
- [x] 文件任务按任务选模型保留：`file submit <path> [--model <id>]`，不带 `--model` 用当前已加载模型。rvc 增 `ListModels` 查询，供 CLI `model list`（`--available` 滤空）。入队不预检模型存在性，报错统一归推理层（SPEC-10）：执行时 `LoadModel` 缺失回 InvalidArgument、损坏回 InferenceFailed，失败标 failed 带原因、队列继续。
- [x] 文件转换数据面中间层归 daemon（Rust）：解码文件为 16 kHz 帧喂给 rvc、收 48 kHz 输出帧编码为结果文件；文件期间 daemon 绑定输出 socket 作数据面对端。此路径离线，不在实时音频路径上，不影响实时延迟。（`daemon/src/file/`）
- [x] 数据面所有权归 daemon 单一权威（根治共享 socket 约定式所有权竞态）：pre 的 rvc AI 支路（发 `input_socket` + 绑 `output_socket`）仅在 daemon 断言实时态时活跃，pre 启动/重启后静默待断言、不单边 reclaim。daemon 每轮对账 pre 模式到权威态（`gpu_owner==File` → 文件接管），并按 pre 上报的 `startup_id` 识别重启重新断言；文件交接窗口内对账让位给交接守卫。文件任务期间 pre 重启不再丢失交接：pre 静默不污染攒帧、不孤儿化输出绑定，daemon 重断言恢复交接、任务继续；仅 pre 持续不可达才带原因中止。（`daemon/src/services.rs:627-653`）
- [x] 文件降噪保留（旧 `file_denoise`，默认关）：daemon 离线调用与 pre 同一套共享 DSP（SPEC-02）对文件先降噪再喂 rvc；实时链路降噪不受影响。（`daemon/Cargo.toml:11` 共用 `mozart-dsp`）
- [x] 文件任务取消由 daemon 编排：排队中→移出队列、标 cancelled、删源副本；执行中→发 `StopStream` 给 rvc、停后标 cancelled。临时文件（解码裸音频、半成品输出）归 daemon 清理：取消/失败删除，成功改名发布；C++ 不碰文件。CLI `file cancel <job_id>`。
- [x] 文件存储与结果生命周期：源副本与结果同放一个受管存储目录，默认 `~/.local/state/mozart/files/`（持久存储，不放 /run tmpfs），TOML 可配。结果原子发布：先写 `.part`、成功再 rename。驱逐沿用旧策略：每任务后目录总大小超上限（默认 1000 MiB，TOML 键）按时间戳从旧到新删至 80%，未完成任务的源/结果受保护，已完成结果可被删——结果是有限缓存，需及时取走。（`daemon/src/file/storage.rs:12,86-91`）
- [x] 取结果 v1 用 CLI `file fetch <job_id> [-o 路径]`，默认名原文件名主干 + `.wav`（输出固定 WAV）；HTTP 下载归 web 适配器，不沿用旧固定下载名 `mozart_output.wav`。
- [x] 文件任务状态与进度：字段 `job_id` / `name` / `model_id` / `state` / `progress` / `error`，state 六态（queued / processing / cancelling / completed / failed / cancelled），completed 附结果可取标记（fetch 提示）；不带模式枚举（mode 派生）。进度 = 粗阶段（解码/喂入/推理/发布）+ 推理段用 rvc `GetStatus` 块/窗计数细化（daemon 每秒轮询已有）；依赖 rvc 文件模式给出有意义块计数，缺则退回粗阶段。CLI `file status <job_id>` 查单个、`file list` 看队列。
- [x] 文件转换入口（v1）：CLI `file submit <path>` 提交本地路径；daemon 复制文件到受管存储目录后入队，队列只引用副本，原文件变动不影响排队任务。入口校验：扩展名白名单 `.wav` / `.mp3` / `.m4a` / `.aac` / `.flac` / `.ogg`，100 MiB 大小预检；300 s 时长上限在解码后检查。HTTP 上传归 web 适配器，流式写盘后经同一入队契约提交。（`daemon/src/file/decode.rs:46,75-80`）
- [x] 监控归 daemon，不独立进程：进程存活与崩溃重启；每秒轮询各服务 `GetStatus` 汇总业务指标与降级原因；收集子进程日志。（`daemon/src/poller.rs`、`daemon/src/supervisor.rs`）
- [x] 音频设备识别归 daemon（硬件发现，与 hwmon 一类）：用 ALSA API（`snd_device_name_hint`）枚举采集/播放设备，不 shell `arecord`/`aplay`；按需查询不缓存（USB 插拔）；返回设备 id（ALSA 名，配置用）+ 人读标签。CLI `device list` 缺省只列配置首选（`plughw:` 与 `default`），`--all` 返回完整 ALSA 别名清单。配置/切换设备时对照当前清单校验存在。pre 只打开配置的设备（使用）、不做枚举；持久配置 `config set` 仍由 daemon 重启 pre，运行时临时热切换见下方 `device use`。（`daemon/src/device.rs:14-82`、`ctl/src/main.rs`）
- [x] 语言服务（识别、桥、TTS）各绑控制 socket，daemon 连接并每秒轮询，接法与 pre/rvc 一致。llama-server 不说控制协议：daemon 只管其进程存活与重启，业务可达性由桥的 `GetStatus` 带报。（`daemon/src/services.rs`）
- [x] 监管退避：崩溃后从 500 ms 起倍增，上限 10 s；连续稳定运行 30 s 后重置。子进程日志追加到 `run_dir/logs/<name>.log`，daemon 退出前先停止子进程。（`daemon/src/supervisor.rs:87-175`）
- [x] 路由执行顺序：切到 rvc 先 `LoadModel` / `StartStream` 再让 pre `SetRoute(rvc)`，失败回滚停流；切回 direct 先 pre `SetRoute(direct)` 再 `StopStream` 并释放 GPU。轮询对账按同一顺序：rvc 方向先确保实时流在跑再切 pre 路由，流未就绪（rvc 缺席或起流失败）本轮不切、下一轮重试；direct 方向先切路由再停流，文件 GPU 门控不变。（`daemon/src/services.rs`；次序回归 `daemon/tests/linkage.rs`）
- [x] 主机硬件指标（CPU/内存/GPU/可用性）由 daemon 读 `/proc`、`/sys` 采集（移植旧 `SystemMonitor`），并入 status。监控出口经 CLI `status`/`logs`，HTTP 适配器推网页；历史时序、告警、面板推迟。（`daemon/src/hwmon.rs`）
- [x] RPC 超时（`rpc_timeout_ms`）覆盖 connect、send、recv 全程；控制通道任何一步不得无限阻塞，卡死的子进程不得拖住控制面。
- [x] 子进程设 `PDEATHSIG` 后校验父进程仍存活（ppid 未变为 1），daemon 崩溃瞬间不留孤儿。（`daemon/src/supervisor.rs:275-286`）
- [x] 关停幂等：关停进行中再次收到 `Shutdown` 仍应答 ok；其余命令返回 `Busy`，原因为 "daemon is shutting down"。
- [x] daemon 捕获 SIGTERM/SIGINT，走与 `Shutdown` 相同的优雅关停（停子进程、退出），幂等。运行时状态每次改动已落盘，关停无需额外写盘。

## 本机程序集成

本机程序连接配置的 `daemon.run_dir` / `daemon.socket`，默认文件名为 `daemon.sock`。
传输和信封见 [SPEC-01](SPEC-01-core.md)；命令号的单一来源为
`core/include/mozart/ctl.h`，JSON schema 为 `core/src/ctl/schema.rs`。
Rust 调用方复用 `mozart-core::ctl`，其他语言按同一小端信封编码。
这是 Unix `SOCK_SEQPACKET` 接口，不是 HTTP，也不是无头 JSON 字节流。
响应回显请求的命令号与 `request_id`；调用方必须校验关联关系。

外部客户端统一经过 daemon，不直连业务子进程控制面。
HTTP 方法、路径、上传下载和 SSE 契约归 [SPEC-06](SPEC-06-web.md)，不在 daemon 内实现第二套网络入口。
CLI 的 `--json` 输出通常为响应的 `data`，不是完整 socket 响应信封。
`up` / `down` 是 CLI 的生命周期组合操作，不对应新的控制命令号。

### 程序集成待实现补充

- [x] `GetStatus.data` 增加 `file_limits: {"max_upload_bytes": <整数>}`。
  从 daemon 当前生效配置取得，不从磁盘 TOML 单独重读；配置即时更新后下次查询应反映新值。
  旧响应缺少此字段时，HTTP 上传返回 `Unsupported`，不得使用默认常量替代。
  此字段只供传输入口提前限流；最终准入仍由文件队列入口按当时生效限制裁定。
- [x] `GetStatus.data.daemon` 增加生效 `poll_interval_ms`，供状态 SSE 共享轮询使用。
  旧响应缺少此字段时，状态 SSE 返回 `Unsupported`，普通 `/status` 查询仍可透传。
- [x] `FileFetch` 结果缺失时，错误 `reason` 使用 job_id 和失败原因，不包含受管文件绝对路径。
  路径只记 daemon 日志；成功响应的 `output_path` 仍供同机适配器打开，不作为 HTTP JSON 发布。
  完成此修改后，`GetStatus.data.api_capabilities` 数组才可声明 `file_fetch_safe_errors_v1`。
  字段缺失按空能力列表处理；适配器仅在同一 daemon 连接上确认该能力后开放下载。
- [x] 将 CLI 普通请求、配置写入和启动停止的截止时间计算提取为共用逻辑，供适配器复用。
  保持现有 CLI 预算，不因增加 HTTP 层缩短控制请求时间。
- [ ] glossary 编辑和 subtitle clear 的提供方命令另行补齐；实时模型选择已有 `ModelUse`，Web 映射已接入。
  在提供方契约确定前，适配器不得直接向 RVC 发 `LoadModel` 或自行截断字幕文件。

## 改进点

- [x] 新增 `SetOutputMode`（`0x0122`，`direct`／`rvc`／`broadcast`），依次执行停播、路由选择和新播报许可。`GetStatus.output_mode` 分开报告请求、应用、过渡阶段和最近错误；等待文件归还时不抢占。不重启语言链。设路由或播报的旧命令显式退出模式编排，并沿用各自语义。模式选择本身只存在 daemon 内存，停止后的播报关闭覆盖仍按原规则落盘。

- [x] 用显式 RVC 租约区分空闲、实时、文件和实时待归还。切回直通或补偿起流结果未知时，未收到 RVC 停流确认不释放租约；确认前不调度文件或发起下一次实时起流。状态区分已下发的直通与仍待归还的 RVC。`daemon/tests/file_queue.rs` 覆盖停流命令未执行及已执行但应答丢失两种结果。

- [x] 文件驱逐仅删除队列已知任务的受管产物，保留其他文件与符号链接，报告访问错误。

- [x] 模式名与状态字段统一。
- [x] 端口集中配置。（`core/src/config.rs`）
- [ ] （文档）职责行 `HTTP/SSE` 为残留措辞，v1 daemon 不起 HTTP，待删。
- [x] 实时换模型命令与编排：`model use <id>`（0x0120）落地 [docs/config.md](../config.md)「运行时换模型走控制命令」的设计意图。编排定为断流切换（StopStream→LoadModel→预设提交→StartStream），失败用旧模型旧参数恢复并回报原因；`[rvc].model_id` 由本命令写回基线，`config set` 继续拒绝。命令契约、次序与回退见本文件「已确认」三条。推理侧状态语义见 SPEC-03。板端切换延迟、接缝与帧守恒未验证，见 SPEC-03 板端条目。（`ctl/src/main.rs:278,645-664`、`daemon/src/server.rs:340-341,490-506`）
- [x] 实时原生多采样率（控制面）：不做播放采样率协商，播放固定 48 kHz、边界重采样归 pre（SPEC-02）；跨采样率换模型走 `model use` 断流切换，几何随 `StartStream` 重建。主条见 SPEC-03，板端断流代价与多采样率验收见 SPEC-03 板端条目。
- [x] 音频设备实时热切换命令 `device use <capture|playback> <设备名>`（`DAEMON_DEVICE_USE` `0x0121`）：体 `{"kind","device"}`，data `{"kind","device","previous_device"}`。入口按设备清单校验存在性与方向匹配，再经 `PRE_SET_DEVICE`（`0x0208`）下发 pre 实时生效；不写回 `mozart.toml`、不进 `state.json`，`config set pre.capture_device/playback_device` 维持 S 类。daemon 内存保留当前临时生效设备，pre 意外崩溃自愈重启时通过 `--effective-capture-device` / `--effective-playback-device` 沿用；显式 `restart pre` 或 `down`/`up` 清空临时覆盖、恢复 `mozart.toml` 基线。（`daemon/src/server.rs`、`daemon/src/supervisor.rs`、`daemon/src/runtime.rs`、`ctl/src/main.rs`、`daemon/tests/device_use.rs`）
- [x] 文件租约归还：文件结果与数据面归还分开；确认停流、释放输出 socket、确认 pre 恢复，才释放文件租约。未知结果由 daemon 对账，可查询且阻止新任务与实时起流。 `arbiter.file_return_pending` 与降级原因区分文件终态和资源状态；停流前、停流后丢应答及 pre 归还失败均有本机故障注入回归。
- [x] 主模式互斥切换与默认实时变声：有配置模型时默认托管实时变声，无模型时默认托管直出。翻译播报仅输出 TTS 并在空窗静音；实时变声和直出不插播 TTS。三种模式均保留识别和翻译字幕。旧命令与 `startup_mode = "manual"` 保留独立控制。
- [ ] 统一监管 Web 适配器（方案 1）：将 `mozart-web-adapter` 纳入 `Supervisor`（新增 `web` 角色，读取 `[process].web_bin` 与 `[web]` 配置，随 daemon 拉起、崩溃退避重启、`PDEATHSIG` 孤儿防护、日志落 `run_dir/logs/web.log`）；`GetStatus.processes` 上报 `web` 状态，支持 `restart web`；`daemon` 自身仍不内置 HTTP，适配器继续经 `daemon.sock` 接入。
- [ ] 业务子服务组启停控制命令（方案 1）：新增 daemon 控制命令，支持在保持 `daemon` 与 `web` 在线的前提下停止或恢复音频与 AI 业务子服务（`pre`、`enhance`、`rvc`、`asr`、`tts`、`bridge`、`llama-server`），供 Web 控制台释放或恢复 GPU 与内存资源；整机完全关停仍走 `mozart-ctl down` / `Shutdown`。
- [ ] 控制面超时表与连接队列加固：将 `DAEMON_MODEL_USE`、`DAEMON_FILE_SUBMIT` 纳入 `core::ctl::request_timeout_ms` 共用长超时预算，避免切模型与大文件入队误超时；将 `daemon.sock` 的 `listen` 队列上限由 `8` 调大至 `64`，保留 `transition` 互斥锁串行语义。
- [ ] 优雅关停与停机超时加固：关停标志置位后 `poller` 立即提前退出、不再继续对已停子服务做轮询等待；`mozart-ctl down` 停机等待时限与 `start_timeout_ms` 解耦并给足缓冲（至少 `10–15 s`），避免满载停栈误超时。

## 待讨论

- HTTP/SSE 接口契约已归 [SPEC-06](SPEC-06-web.md)，实现按该规格推进，不再互相等待。
