# rvc：变声

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。

## 边界

- [x] C++ 推理进程：HuBERT、RMVPE、Generator。
- [x] 输入源包括实时麦克风帧和文件任务帧。翻译播报路径见 [SPEC-05](SPEC-05-language.md#播报投递)。
- [x] daemon 侧（Rust）：模式、队列、文件任务、状态。

## 链路分工

文件链路和实时链路的目标、资产与几何不同，分别设计、分别验收，不用文件链路的结果证明实时质量。

| 链路 | 目标 | 资产 | 几何与约束 |
| --- | --- | --- | --- |
| 文件 | 质量优先 | 整段、大窗或动态生成器；可用文件专用 profile | 无实时节拍；优先大窗，小窗只作回退 |
| 实时 | 延迟优先 | 必须实时 front/decoder 与实时 HuBERT/RMVPE；模型可与文件链路不同 | 固定 240 ms 块、50 ms 淡化、2.5 s 过去上下文、SOLA、400 ms 启动；采样率与块几何单独评估 |

- 同一模型可以同时具备文件和实时能力；缺实时资产时只提供文件能力，不静默降级。
- 文件链路的大窗/小窗策略不能直接搬到实时链路；实时链路已有的 SOLA、淡化与启动缓冲按实时约束单独优化。

## 已确认

- [x] 推理核心保持独立进程。
- [x] 非推理逻辑归 Rust。
- [x] C++ 进程只保留推理：`StreamSession` 封装 `StreamingRvc` 和 `RVCInferencer`；`RealtimeRvcWorker` 与 `AudioWorker` 的外层传输、队列、模式、统计归 Rust。
- [x] 静音和旁路归音频进程；C++ 不实现这两个概念。
- [x] 参数在 `StartStream` 时固定；v1 运行时调参经 `config set rvc.<会话参数>` 写持久基线，由 daemon 编排应用（见 [SPEC-04](SPEC-04-daemon.md)）；模型切换时 `[rvc]` 会话参数由该模型的预设载入（见 [SPEC-11](SPEC-11-preset.md)），`StartStream` 契约不变。块边界 `SetParams` 仍推迟，见 [FUTURE.md](../FUTURE.md)。
- [x] 输出节拍由输入帧驱动：C++ 每收到一帧输入就回一帧输出，输出环不足时补零。400 ms 启动缓冲留在 `StreamingRvc.startup_delay_out`，音频进程只保留小缓冲吸收送达抖动。大缓冲只有一个主人。
- [ ] 1:1 泵假设采集与播放时钟同源；使用不同声卡时，长期漂移会导致周期性欠载或缓冲增长，需时钟漂移补偿（见 [FUTURE.md](../FUTURE.md)）。推迟项，未实现。
- [x] 板端实测确认小缓冲布局成立：同源 60 s 长跑 49.86 帧/s 1:1、丢弃与欠载零增长，rvc 端到端净滞后约 0.5 s（400 ms 启动缓冲为主）。维持启动缓冲留 C++、音频进程小缓冲的现状；数据见 `reports/realtime-pipeline-20261112/RESULTS.md`。

## 接口约束

- [x] 粗粒度边界：数据面只有“输入帧进、输出帧出”，控制面只有加载、启动、停止、重置和状态查询；不暴露张量、引擎和 stream。
- [x] 数据面沿用 core 的帧契约：16 kHz 输入、48 kHz 输出，一帧一条消息。
- [x] 文件转换复用帧数据面：C++ 只收帧/发帧、不接触文件，对端是 pre（实时）还是 daemon（文件）无感知。文件帧发送必须不丢（阻塞/流控，区别于实时的非阻塞丢弃）；输出 socket 由数据面对端绑定（实时 pre、文件 daemon），实时/文件互斥故不冲突，切换时交接绑定。（`rvc/src/rvc/file_worker.cpp:67-127`、`rvc/transport/unix_dgram_stream.cpp:290-311`）
- [x] 离线推理 v1 整段批处理：C++ 收齐 daemon 喂入的帧、攒成整段跑现有离线流程再回输出帧；质量参数沿用旧实现不动（峰值归一化 0.95、48 Hz 高通、整段 F0、按引擎形状整段一次过或滑窗 + 重叠淡化、RMS 混合、限幅 0.99）。滑窗窗长随生成器帧数 T 按 T/398 缩放（T=398 为 2 s 窗、1 s hop；T=200 为 1 s 窗、0.5 s hop）。内存由 300 s 时长上限兜底。分块流式推迟，见 [FUTURE.md](../FUTURE.md)。（`rvc/src/rvc/inferencer.cpp:24-26,85,234-235,284,330,374,430`）
- [x] 错误用返回码或状态原因表达；不跨进程抛异常，不传 C++ 容器和指针。
- [x] 内存所有权明确：消息自带长度，谁分配谁释放，进程间不共享裸指针。
- [x] 线程模型：Rust 调度独占模式和队列；C++ 进程内单推理线程；控制命令不阻塞音频路径。（模型加载不占控制锁：`rvc/control/stream_session.cpp:152,159`）
- [x] 协议和配置带版本号；不兼容时拒绝并给出原因。
- [x] 状态可查询：队列深度、丢弃计数、推理耗时、模型版本；不向 Rust 实时线程回调。
- [ ] 若以后并入同一进程，改为粗粒度 C ABI：不透明句柄、调用方提供缓冲、无 STL 类型跨边界。条件项，未触发。

## 参数边界

| 层 | 提供方 | 内容 |
| --- | --- | --- |
| 进程启动 | 部署配置 | `device`、`half`、`cache_limit`；模型路径由 `[[models]]` 注册表提供（回退 `models_dir` 扫描）；特征引擎路径可选，缺省自动发现 |
| `LoadModel` | Rust 控制命令 | `model_id`；C++ 读 `config.json` 并报告 `capabilities` 和模型版本 |
| `StartStream` | Rust 控制命令 | `kind`、`model_handle`、`pitch_shift`、`index_rate`、`rms_mix_rate`、`protect`、`f0_method`；文件模式另带 `total_frames`、可选 `total_samples`，见下文 |
| C++ 内部 preset | C++ | `StreamingRvc` 的分块、历史、淡化、SOLA 和启动缓冲参数 |
| Rust / 音频进程 | Rust | 播放侧小缓冲、静音 / 旁路、数据面队列；400 ms 启动缓冲留在 C++，不重复设大缓冲 |

- [x] `kind` 只分 `realtime` 和 `file`。C++ 按模型能力选择实时资产或 quality / full-history preset；缺实时资产时拒绝实时，不静默降级。
- [x] 文件模式的 `total_frames` 必填，类型为正整数 u32，表示本次输入帧数。C++ 收齐后开始整段推理；取消仍用 `StopStream`。`total_samples` 可选，类型为 u64，表示 16 kHz 输入的有效采样数。显式传入时须在 `1..=total_frames × 320` 内；C++ 在推理前据此裁掉尾帧补零。省略时保留全部帧样本。实时模式忽略这两个字段。
- [x] `StartStream` 会话参数取自 TOML `[rvc]` 会话默认值；模型切换时先由该模型的预设载入 `[rvc]`（SPEC-11），不新增控制命令，`StartStream` 仍携带五个会话参数。`config set rvc.<会话参数>` 是持久基线写回（编排见 SPEC-04）；将来无中断 `SetParams` 的运行时改动才记入 daemon 运行时状态文件，重启恢复，不回写 TOML。两者不混用。
- [x] `config set rvc.<会话参数>` 的触发、失败恢复与重试见 [SPEC-04](SPEC-04-daemon.md)。
- [x] `protect`、`rms_mix_rate` 在实时链路不应用，仍只对文件链路生效；实时补应用留 [FUTURE.md](../FUTURE.md)。
- [x] `filter_radius` 在旧 C++ 里只保存、未参与计算，v1 不放进 `StartStream`、不暴露死参数；保留为 FUTURE 提升项，推迟实现 F0 中值滤波，见 [FUTURE.md](../FUTURE.md)。
- [ ] 若板端实测小缓冲扛不住抖动或时钟漂移，再把大缓冲搬到音频进程，同时把 `startup_delay_out` 设为 0 并重测首帧。条件项，未触发。

## 控制消息

- [x] 控制面传输：Unix `SOCK_SEQPACKET`，可靠、保留消息边界；数据面保持 Unix `SOCK_DGRAM`。
- [x] 消息格式：定长头 + JSON 体。头含 `magic`、`version`、`type`、`request_id`、`body_len`。
- [x] 命令：`Hello`、`LoadModel`、`ListModels`、`StartStream`、`StopStream`、`ResetStream`、`GetStatus`。第一版不加 `UnloadModel` 和 `Shutdown`；退出用 SIGTERM。（`core/include/mozart/ctl.h:75-81`、`rvc/control/stream_session.cpp:79-137`、`rvc/src/main.cpp:100-101,307-344`）
- [x] 响应：`request_id` + `status`（`ok` / `error`）+ `code` + `reason` + 可选体。
- [x] 错误码：`InvalidArgument`、`NotLoaded`、`Busy`、`Unsupported`、`InferenceFailed`、`Internal`。
- [x] `Hello` 做协议版本握手；不兼容时拒绝并关闭连接。
- [x] `StartStream` 携带参数边界表中的会话参数；已有活动流时返回 `Busy`，daemon 须先 `StopStream`。`StopStream` 停止收新帧，等当前推理块结束后返回：实时约 240 ms，文件停在下一个检查点（整段 F0 前后、Generator 窗/引擎调用前后）；`GetStatus` 只读原子快照，不阻塞推理线程。（`rvc/control/stream_session.cpp:295-303`、`rvc/src/rvc/audio_worker.cpp:63`、`rvc/src/rvc/file_worker.cpp:38-46`、`inferencer.cpp:219,253,382,698-708`）
- [x] `ListModels` 返回模型目录清单，每项含 `id`、`exists`（资产齐全可加载）、`loaded`（当前已加载）。只读，不加载模型、不阻塞推理线程；供 CLI `model list`（入队不预检，见 SPEC-10）。（`rvc/model/model_loader.cpp:220-243`）
- [x] 状态字段：`state`、`model_id`、`model_version`、`capabilities`、`stream_kind`、`frames_in`、`frames_out`、`frames_dropped`、`blocks`、`inference_ms_last` / `avg` / `p95`、`queue_depth`、`last_error`。（`rvc/control/stream_session.cpp:110-133`，共 14 字段）
- [x] 文件模式下 `blocks` 按 Generator 窗计数，供 daemon 推导文件转换推理进度（见 SPEC-04）。（`rvc/src/rvc/file_worker.cpp:59,150`）
- [x] Rust 每秒轮询一次状态；C++ 不向 Rust 实时线程回调。
- [x] 控制消息由 daemon 发送；音频进程不参与 C++ 控制面。C++ 进程崩溃由 daemon 重启，重启后重发 `Hello` / `LoadModel` / `StartStream`。

## 模型注册表

TOML 顶层 `[[models]]` 数组声明可用模型，每项含 `id`、`type`（rvc/asr/tts/llm）、`path`。

- [x] `LoadModel(model_id)` 在注册表中按 id + type=rvc 查找 path，不再拼 `models_dir/model_id`；注册表为空时回退 `models_dir/model_id`。入口不预检目录，路径解析与报错归 ModelManager。（`rvc/control/stream_session.cpp:465-484`、`rvc/src/rvc/model_loader.cpp:262-292`）
- [x] `ListModels` 返回注册表中 type=rvc 的全部条目；注册表为空时回退扫描 `models_dir` 子目录。（`rvc/model/model_loader.cpp:220-243`）
- [x] 注册表为空时保持旧行为，向后兼容。
- [x] 特征引擎路径（hubert_path / rmvpe_path / realtime_hubert_path / realtime_rmvpe_path）可选；缺省时从 `models_dir` 按约定文件名自动发现（`hubert_base_dynamic.onnx`、`rmvpe_dynamic.onnx`、`hubert-realtime.onnx`、`rmvpe-realtime.onnx`），找到即用，找不到报错。显式配置优先。（`rvc/config/rvc_config.cpp:77-116`）

## 改进点

- [ ] 模型主导参数动态化（多模型适配，总方向）：凡由模型决定的参数（采样率、帧率、hop、特征维度、上采样比、合成帧数 T、音高量化）从模型动态读取，据此推导分窗几何、推理模式（整段/动态/分窗）与重采样，不写死常量。实时契约的固定形状（front 280 / decoder 30 / hubert 44800 / rmvpe 32）也应由模型能力推导。（`rvc/src/rvc/pipeline.cpp:202-213`）前置的跨模型实测已覆盖 40k/48k 共 10 个模型（文件链路）；实时固定形状仍待模型能力推导，动态输出采样率方案见 [FUTURE](../FUTURE.md)。
- [x] 文件链路多模型：静态定长、静态整段与动态生成器（40k/48k）共 10 个模型板端完成，输出按输入时长统一收口，`frames_in == frames_out`；空闲模型切换先释放旧 inferencer 并腾出缓存槽位，避免旧模型显存未释放时触发 TensorRT OOM 回退 ONNX；活动流期间保持旧模型直到新模型换入。（`rvc/src/rvc/inferencer.cpp:243-442`、`rvc/src/rvc/pipeline.cpp:136-151`、`rvc/src/rvc/model_loader.cpp:307-342`、`reports/multi-model-file-20261115/RESULTS.md`）
- [ ] 文件分窗几何动态化：398 只作默认参考；窗长、hop、pad 和超 profile 分块由引擎 min/opt/max、T、采样率与输入时长推导；拟增 `[file].chunk_frames`。验收：10/30/120 s 的 F0、log-mel、内存和延迟，22 s 内整段结果不变。（`rvc/src/rvc/inferencer.cpp:26,262-266,336-346`）
- [ ] 文件链路优先大窗资产：编译时为文件链路提供覆盖目标时长的大窗或动态大 profile；加载器区分实时/文件资产，长文件不默认 T=200。验收：同输入的窗数、F0、log-mel、显存和延迟。
- [ ] 文件小窗质量方向：必须小窗时，先做互相关/SOLA 对齐，再用等功率淡化；评估真实前后文、跨窗 F0 平滑、窗级增益匹配和噪声/相位连续性。验收看 F0 P90、log-mel、边界阶跃、显存与延迟。
- [ ] 实时链路单独优化：实时模型可与文件不同，能力区分仅文件/仅实时/两者；按 240 ms 块、50 ms 淡化、SOLA、400 ms 启动单独调优；40 kHz 见 [FUTURE](../FUTURE.md)。验收：48 kHz 无回归，具备实时资产的新模型起流、延迟 P95 与连续性。
- [x] 模型加载移出控制锁。（`rvc/control/stream_session.cpp:152,159`）
- [ ] 长文件改分块，限制内存。（FUTURE）正文已定推迟（分块流式见 FUTURE），本条同步标推迟。
- [x] 配置项必须与实际行为一致。原 LoadModel 入口预检与注册表定稿冲突已修复（见模型注册表第一节）。
- [ ] 离线推理三条路径（动态整段 / 静态整段 / 分窗）各自重复 `apply_rms_mix` → `resample` → `limit_peak`；抽公共收尾，避免改一处漏两处导致不同导出形状听感不一致。（`rvc/src/rvc/inferencer.cpp:280-284,318-322,426-430`）
- [ ] 多说话人模型缺 per-job 选说话人：`spk_id` 只从 config 读，202 个说话人只用到 0。需每任务参数（与 SetParams 协同）。（`rvc/src/rvc/inferencer.cpp:800-801`）
- [ ] 小项：`infer_window` 无条件复制 feats，protect 关闭时白复制（`rvc/src/rvc/inferencer.cpp:703`）；`apply_rms_mix` 逐样本 `std::pow`，可在 RMS 网格算再插值（`rvc/src/rvc/inferencer.cpp:924-930`）。
