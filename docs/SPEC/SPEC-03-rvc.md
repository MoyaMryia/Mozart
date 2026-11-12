# rvc：变声

状态：讨论中。

## 边界

- C++ 推理进程：HuBERT、RMVPE、Generator。
- 输入源包括实时麦克风帧和翻译播报音频；翻译播报使用同一目标音色。
- daemon 侧（Rust）：模式、队列、文件任务、状态。

## 已确认

- 推理核心保持独立进程。
- 非推理逻辑归 Rust。
- C++ 进程只保留推理：`StreamSession` 封装 `StreamingRvc` 和 `RVCInferencer`；`RealtimeRvcWorker` 与 `AudioWorker` 的外层传输、队列、模式、统计归 Rust。
- 静音和旁路归音频进程；C++ 不实现这两个概念。
- 参数在 `StartStream` 时固定；在线 `SetParams` 推迟，见 [FUTURE.md](../FUTURE.md)。
- 输出节拍由输入帧驱动：C++ 每收到一帧输入就回一帧输出，输出环不足时补零。400 ms 启动缓冲留在 `StreamingRvc.startup_delay_out`，音频进程只保留小缓冲吸收送达抖动。大缓冲只有一个主人。
- 1:1 泵假设采集与播放时钟同源；使用不同声卡时，长期漂移会导致周期性欠载或缓冲增长，需时钟漂移补偿（见 [FUTURE.md](../FUTURE.md)）。
- 板端实测确认小缓冲布局成立：同源 60 s 长跑 49.86 帧/s 1:1、丢弃与欠载零增长，rvc 端到端净滞后约 0.5 s（400 ms 启动缓冲为主）。维持启动缓冲留 C++、音频进程小缓冲的现状；数据见 `reports/realtime-pipeline-20261112/RESULTS.md`。

## 接口约束

- 粗粒度边界：数据面只有“输入帧进、输出帧出”，控制面只有加载、启动、停止、重置和状态查询；不暴露张量、引擎和 stream。
- 数据面沿用 core 的帧契约：16 kHz 输入、48 kHz 输出，一帧一条消息。
- 错误用返回码或状态原因表达；不跨进程抛异常，不传 C++ 容器和指针。
- 内存所有权明确：消息自带长度，谁分配谁释放，进程间不共享裸指针。
- 线程模型：Rust 调度独占模式和队列；C++ 进程内单推理线程；控制命令不阻塞音频路径。
- 协议和配置带版本号；不兼容时拒绝并给出原因。
- 状态可查询：队列深度、丢弃计数、推理耗时、模型版本；不向 Rust 实时线程回调。
- 若以后并入同一进程，改为粗粒度 C ABI：不透明句柄、调用方提供缓冲、无 STL 类型跨边界。

## 参数边界

| 层 | 提供方 | 内容 |
| --- | --- | --- |
| 进程启动 | 部署配置 | `models_dir`、`device`、`half`、`cache_limit`、HuBERT / RMVPE / 实时资产路径 |
| `LoadModel` | Rust 控制命令 | `model_id`；C++ 读 `config.json` 并报告 `capabilities` 和模型版本 |
| `StartStream` | Rust 控制命令 | `kind`、`model_handle`、`pitch_shift`、`index_rate`、`rms_mix_rate`、`protect`、`f0_method` |
| C++ 内部 preset | C++ | `StreamingRvc` 的分块、历史、淡化、SOLA 和启动缓冲参数 |
| Rust / 音频进程 | Rust | 播放侧小缓冲、静音 / 旁路、数据面队列；400 ms 启动缓冲留在 C++，不重复设大缓冲 |

`kind` 只分 `realtime` 和 `file`。C++ 按模型能力选择实时资产或 quality / full-history preset；缺实时资产时拒绝实时，不静默降级。

`StartStream` 会话参数取自 TOML `[rvc]` 会话默认值；v1 无运行时改参数命令。将来 `SetParams` 的运行时改动记入 daemon 运行时状态文件（见 SPEC-04），重启恢复，不回写 TOML。

`filter_radius` 在旧 C++ 里只保存、未参与计算，v1 不放进 `StartStream`、不暴露死参数；保留为 FUTURE 提升项，推迟实现 F0 中值滤波，见 [FUTURE.md](../FUTURE.md)。

若板端实测小缓冲扛不住抖动或时钟漂移，再把大缓冲搬到音频进程，同时把 `startup_delay_out` 设为 0 并重测首帧。

## 控制消息

- 控制面传输：Unix `SOCK_SEQPACKET`，可靠、保留消息边界；数据面保持 Unix `SOCK_DGRAM`。
- 消息格式：定长头 + JSON 体。头含 `magic`、`version`、`type`、`request_id`、`body_len`。
- 命令：`Hello`、`LoadModel`、`StartStream`、`StopStream`、`ResetStream`、`GetStatus`。第一版不加 `UnloadModel` 和 `Shutdown`；退出用 SIGTERM。
- 响应：`request_id` + `status`（`ok` / `error`）+ `code` + `reason` + 可选体。
- 错误码：`InvalidArgument`、`NotLoaded`、`Busy`、`Unsupported`、`InferenceFailed`、`Internal`。
- `Hello` 做协议版本握手；不兼容时拒绝并关闭连接。
- `StartStream` 携带参数边界表中的会话参数；已有活动流时返回 `Busy`，daemon 须先 `StopStream`。`StopStream` 停止收新帧，等当前推理块结束（约 240 ms）后返回；`GetStatus` 只读原子快照，不阻塞推理线程。
- 状态字段：`state`、`model_id`、`model_version`、`capabilities`、`stream_kind`、`frames_in`、`frames_out`、`frames_dropped`、`blocks`、`inference_ms_last` / `avg` / `p95`、`queue_depth`、`last_error`。
- Rust 每秒轮询一次状态；C++ 不向 Rust 实时线程回调。
- 控制消息由 daemon 发送；音频进程不参与 C++ 控制面。C++ 进程崩溃由 daemon 重启，重启后重发 `Hello` / `LoadModel` / `StartStream`。

## 改进点

- 模型加载移出控制锁。
- 长文件改分块，限制内存。
- 配置项必须与实际行为一致。
