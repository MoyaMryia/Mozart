# rvc：变声

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。
`[x]` 条目详情已于 2026-11-24 移入 [archive/SPEC-03-rvc.md](archive/SPEC-03-rvc.md)，本文件保留契约、关键决策摘要与活跃待办。

## 边界

- C++ 推理进程：HuBERT、RMVPE、Generator。
- 输入源包括实时麦克风帧和文件任务帧。翻译播报路径见 [SPEC-05](SPEC-05-language.md#播报投递)。
- daemon 侧（Rust）：模式、队列、文件任务、状态。

## 链路分工

文件链路和实时链路的目标、资产与几何不同，分别设计、分别验收，不用文件链路的结果证明实时质量。

| 链路 | 目标 | 资产 | 几何与约束 |
| --- | --- | --- | --- |
| 文件 | 质量优先 | 整段、大窗或动态生成器；可用文件专用 profile | 无实时节拍；优先大窗，小窗只作回退 |
| 实时 | 延迟优先 | 必须实时 front/decoder 与实时 HuBERT/RMVPE；模型可与文件链路不同 | 固定 240 ms 块、2.5 s 过去上下文；淡化 50 ms 与 SOLA 10 ms 为定值，启动 340 ms 可调；输出采样率随模型，块几何由采样率推导 |

- 同一模型可以同时具备文件和实时能力；缺实时资产时只提供文件能力，不静默降级。
- 文件链路的大窗/小窗策略不能直接搬到实时链路；实时链路已有的 SOLA、淡化与启动缓冲按实时约束单独优化。

## 已确认决策（摘要）

> 全文条目与证据见 [archive/SPEC-03-rvc.md](archive/SPEC-03-rvc.md)。与归档冲突时以归档原文为准并回来修正本节。

- 推理核心保持独立进程，非推理逻辑归 Rust；板端 60 s 长跑确认小缓冲布局成立（1:1、丢弃与欠载零增长、净滞后约 0.5 s），启动缓冲留 C++、音频进程只留小缓冲。
- 实时原生多采样率（32k/40k/48k）已实现：几何由采样率推导，播放固定 48 kHz、边界重采样归 pre；采样率生态调研结论见归档与 `reports/realtime-sample-rate-20261116/RESULTS.md`。
- 实时换模型为断流切换：`StartStream` 重建 `StreamingRvc`，过去上下文、SOLA 状态与音高缓存不跨模型保留，跨采样率重建几何；空闲切换先释放旧 inferencer 防 OOM，活动流期间保持旧模型直到新模型换入。
- 文件链路多模型板端验收通过（40k/48k 共 10 个模型，`frames_in == frames_out`）；空闲切换先释放旧 inferencer 并腾出缓存槽位，避免 TensorRT OOM 回退 ONNX。
- 实时几何单一来源已落地：导出/编译脚本与 C++ 门控镜像同一 JSON 公式；`[rvc.realtime]` 四键接入校验、`StartStream` 与 SPEC-04 Stop/Start 判据；RMVPE 逐块帧数由几何推导并进门控。
- `SetParams`（`0x0308`）在块边界取参数快照、原子更新 `skip_silence`，不重建推理器与流缓冲；模型加载移出控制锁；LoadModel 入口预检与注册表冲突已修复，路径解析与报错归 ModelManager。
- 板端多采样率与换模型冒烟通过（48k↔40k↔32k、overlap 定值拒绝、失败恢复、文件占 GPU 回 Busy）；采集静音时须置 `skip_silence = false` 才真跑推理，否则 `blocks` 恒 0。

## 已确认

- C++ 进程只保留推理：`StreamSession` 封装 `StreamingRvc` 和 `RVCInferencer`；`RealtimeRvcWorker` 与 `AudioWorker` 的外层传输、队列、模式、统计归 Rust。
- 静音和旁路归音频进程；C++ 不实现这两个概念。
- 初始参数随 `StartStream` 下发，实时流活动期间 `pitch_shift`、`index_rate`、`f0_method` 与 `[rvc.realtime].skip_silence` 经 `SetParams`（`0x0308`）在下一推理块边界无中断生效（保留 `StreamingRvc` 缓冲与 `RVCInferencer` 音高缓存/随机状态）；`startup_delay_ms` 仍走 `StopStream`→`StartStream`（见 [SPEC-04](SPEC-04-daemon.md)）；模型切换时 `[rvc]` 会话参数由该模型的预设载入（见 [SPEC-11](SPEC-11-preset.md)），`StartStream` 契约不变。实时换模型由 daemon `model use` 断流编排（SPEC-04），rvc 不新增命令。
- 输出节拍由输入帧驱动：C++ 每收到一帧输入就回一帧输出，输出环不足时补零。启动缓冲留在 `StreamingRvc.startup_delay_out`（默认 340 ms，`startup_delay_ms` 旋钮），音频进程只保留小缓冲吸收送达抖动。大缓冲只有一个主人。
- [ ] 1:1 泵假设采集与播放时钟同源；使用不同声卡时，长期漂移会导致周期性欠载或缓冲增长，需时钟漂移补偿（见 [FUTURE.md](../FUTURE.md)）。推迟项，未实现。

## 接口约束

- 粗粒度边界：数据面只有“输入帧进、输出帧出”，控制面只有加载、清单查询、启动、停止、重置、在线调参和状态查询；不暴露张量、引擎和 stream。
- 数据面沿用 core 的帧契约：16 kHz 输入，一帧一条消息。实时输出按模型原生采样率（32/40/48 kHz）填 `sample_rate` 与 `valid_samples`；文件输出仍固定 48 kHz，离线收尾重采样。字段与偏移见 SPEC-01。（`rvc/src/rvc/audio_worker.cpp:137-139`、`rvc/src/rvc/file_worker.cpp:116-117`）
- 文件转换复用帧数据面：C++ 只收帧/发帧、不接触文件，对端是 pre（实时）还是 daemon（文件）无感知。文件帧发送必须不丢（阻塞/流控，区别于实时的非阻塞丢弃）；输出 socket 由数据面对端绑定（实时 pre、文件 daemon），实时/文件互斥故不冲突，切换时交接绑定。（`rvc/src/rvc/file_worker.cpp:67-127`、`rvc/transport/unix_dgram_stream.cpp:290-311`）
- 离线推理 v1 整段批处理：C++ 收齐 daemon 喂入的帧、攒成整段跑现有离线流程再回输出帧；质量参数沿用旧实现不动（峰值归一化 0.95、48 Hz 高通、整段 F0、按引擎形状整段一次过或滑窗 + 重叠淡化、RMS 混合、限幅 0.99）。滑窗窗长随生成器帧数 T 按 T/398 缩放（T=398 为 2 s 窗、1 s hop；T=200 为 1 s 窗、0.5 s hop）。内存由 300 s 时长上限兜底。分块流式推迟，见 [FUTURE.md](../FUTURE.md)。（`rvc/src/rvc/inferencer.cpp:24-26,85,234-235,284,330,374,430`）
- 错误用返回码或状态原因表达；不跨进程抛异常，不传 C++ 容器和指针。
- 内存所有权明确：消息自带长度，谁分配谁释放，进程间不共享裸指针。
- 线程模型：Rust 调度独占模式和队列；C++ 进程内单推理线程；控制命令不阻塞音频路径。（模型加载不占控制锁：`rvc/control/stream_session.cpp:152,159`）
- 协议和配置带版本号；不兼容时拒绝并给出原因。
- 状态可查询：队列深度、丢弃计数、推理耗时、模型版本；不向 Rust 实时线程回调。
- [ ] 若以后并入同一进程，改为粗粒度 C ABI：不透明句柄、调用方提供缓冲、无 STL 类型跨边界。条件项，未触发。

## 参数边界

| 层 | 提供方 | 内容 |
| --- | --- | --- |
| 进程启动 | 部署配置 | `device`、`half`、`cache_limit`；模型路径由 `[[models]]` 注册表提供（回退 `models_dir` 扫描）；特征引擎路径可选，缺省自动发现 |
| `LoadModel` | Rust 控制命令 | `model_id`；C++ 读 `config.json` 并报告 `capabilities` 和模型版本 |
| `StartStream` | Rust 控制命令 | `kind`、`model_handle`、`pitch_shift`、`index_rate`、`rms_mix_rate`、`protect`、`f0_method`；实时模式另带四个 `[rvc.realtime]` 旋钮（可选，缺省用 JSON 默认）；文件模式另带 `total_frames`、可选 `total_samples`，见下文 |
| `SetParams` | Rust 控制命令 | `pitch_shift`、`index_rate`、`rms_mix_rate`、`protect`、`f0_method`、可选 `skip_silence`；实时流活动时在下一推理块边界无中断生效 |
| C++ 内部 preset | C++ | `StreamingRvc` 的分块、历史、淡化、SOLA 和启动缓冲参数 |
| Rust / 音频进程 | Rust | 播放侧小缓冲、静音 / 旁路、数据面队列；340 ms 启动缓冲留在 C++，不重复设大缓冲 |

- `kind` 只分 `realtime` 和 `file`。C++ 按模型能力选择实时资产或 quality / full-history preset；缺实时资产时拒绝实时，不静默降级。
- 文件模式的 `total_frames` 必填，类型为正整数 u32，表示本次输入帧数。C++ 收齐后开始整段推理；取消仍用 `StopStream`。`total_samples` 可选，类型为 u64，表示 16 kHz 输入的有效采样数。显式传入时须在 `1..=total_frames × 320` 内；C++ 在推理前据此裁掉尾帧补零。省略时保留全部帧样本。实时模式忽略这两个字段。
- `StartStream` 会话参数取自 TOML `[rvc]` 会话默认值；模型切换时先由该模型的预设载入 `[rvc]`（SPEC-11），`StartStream` 仍携带五个会话参数。`config set rvc.<会话参数>` 与 `preset use` 在实时流活动期间对可热更参数下发 `SetParams`（`0x0308`）无中断生效（编排见 SPEC-04）。将来若增加不写 TOML 基线的独立运行时调参命令，其覆盖才记入 daemon 运行时状态文件。两者不混用。
- `config set rvc.<会话参数>` 的触发、失败恢复与重试见 [SPEC-04](SPEC-04-daemon.md)。
- [x] `protect`、`rms_mix_rate` 已接入实时链路（见下方「实时变声衔接自然度分步优化」步骤 4）。
- `filter_radius` 在旧 C++ 里只保存、未参与计算，v1 不放进 `StartStream`、不暴露死参数；保留为 FUTURE 提升项，推迟实现 F0 中值滤波，见 [FUTURE.md](../FUTURE.md)。
- [ ] 若板端实测小缓冲扛不住抖动或时钟漂移，再把大缓冲搬到音频进程，同时把 `startup_delay_out` 设为 0 并重测首帧。条件项，未触发。

## 控制消息

- 控制面传输：Unix `SOCK_SEQPACKET`，可靠、保留消息边界；数据面保持 Unix `SOCK_DGRAM`。
- 消息格式：定长头 + JSON 体。头含 `magic`、`version`、`type`、`request_id`、`body_len`。
- 命令：`Hello`、`LoadModel`、`ListModels`、`StartStream`、`StopStream`、`ResetStream`、`GetStatus`、`SetParams`（`0x0308`）。第一版不加 `UnloadModel` 和 `Shutdown`；退出用 SIGTERM。（`core/include/mozart/ctl.h:88-97`、`rvc/control/stream_session.cpp:115-150`、`rvc/main.cpp:100-101`）
- 响应：`request_id` + `status`（`ok` / `error`）+ `code` + `reason` + 可选体。
- 错误码：`InvalidArgument`、`NotLoaded`、`NotReady`、`Busy`、`Unsupported`、`InferenceFailed`、`Internal`。
- `Hello` 做协议版本握手；不兼容时拒绝并关闭连接。
- `StartStream` 携带参数边界表中的会话参数；已有活动流时返回 `Busy`，daemon 须先 `StopStream`。`SetParams` 在实时流活动期间于下一推理块边界无中断生效：未加载模型返回 `NotLoaded`，无活动流返回 `NotReady`，文件流活动返回 `Busy`，参数越界或类型不符返回 `InvalidArgument`（`rvc/control/stream_session.cpp`、`rvc/src/rvc/inferencer.cpp`、`rvc/src/rvc/streaming_pipeline.cpp`）。`StopStream` 停止收新帧，等当前推理块结束后返回：实时约 240 ms，文件停在下一个检查点（整段 F0 前后、Generator 窗/引擎调用前后）；`GetStatus` 只读原子快照，不阻塞推理线程。（`rvc/control/stream_session.cpp:295-303`、`rvc/src/rvc/audio_worker.cpp:63`、`rvc/src/rvc/file_worker.cpp:38-46`、`inferencer.cpp:219,253,382,698-708`）
- `ListModels` 返回模型目录清单，每项含 `id`、`exists`（资产齐全可加载）、`loaded`（当前已加载）。只读，不加载模型、不阻塞推理线程；供 CLI `model list`（入队不预检，见 SPEC-10）。（`rvc/model/model_loader.cpp:220-243`）
- 状态字段：`state`、`model_id`、`model_version`、`capabilities`、`stream_kind`、`frames_in`、`frames_out`、`frames_dropped`、`blocks`、`inference_ms_last` / `avg` / `p95`、`queue_depth`、`last_error`。（`rvc/control/stream_session.cpp:110-133`，共 14 字段）
- 文件模式下 `blocks` 按 Generator 窗计数，供 daemon 推导文件转换推理进度（见 SPEC-04）。（`rvc/src/rvc/file_worker.cpp:59,150`）
- Rust 每秒轮询一次状态；C++ 不向 Rust 实时线程回调。
- 控制消息由 daemon 发送；音频进程不参与 C++ 控制面。C++ 进程崩溃由 daemon 重启，重启后重发 `Hello` / `LoadModel` / `StartStream`。

## 模型注册表

TOML 顶层 `[[models]]` 数组声明可用模型，每项含 `id`、`type`（rvc/asr/tts/llm）、`path`。

- `LoadModel(model_id)` 在注册表中按 id + type=rvc 查找 path，不再拼 `models_dir/model_id`；注册表为空时回退 `models_dir/model_id`。入口不预检目录，路径解析与报错归 ModelManager。（`rvc/control/stream_session.cpp:465-484`、`rvc/src/rvc/model_loader.cpp:262-292`）
- `ListModels` 返回注册表中 type=rvc 的全部条目；注册表为空时回退扫描 `models_dir` 子目录。（`rvc/model/model_loader.cpp:220-243`）
- 注册表为空时保持旧行为，向后兼容。
- 特征引擎路径（hubert_path / rmvpe_path / realtime_hubert_path / realtime_rmvpe_path）可选；缺省时从 `models_dir` 按约定文件名自动发现（`hubert_base_dynamic.onnx`、`rmvpe_dynamic.onnx`、`hubert-realtime.onnx`、`rmvpe-realtime.onnx`），找到即用，找不到报错。显式配置优先。（`rvc/config/rvc_config.cpp:77-116`）

## 实时几何与资产参数

实时几何、资产形状与编译参数的单一来源是 `tools/model/realtime_chain_params.json`，三段：`model_contract` 只读训练常量、`geometry` 自由参数、`compile` 编译参数。运行时门控与导出/编译脚本镜像同一套公式，取代「C++ 常量、CLI 手填、门控字面量」三处手动同步。会话五参数归参数边界与 [SPEC-11](SPEC-11-preset.md) 预设（两链共用、按模型存）；文件链几何归 [SPEC-12](SPEC-12-file-chain-params.md)；`[rvc.realtime]` 旋钮文件链不读不扫。

`geometry` 段还定义四个非形状旋钮的默认值（`crossfade_ms`、`sola_search_ms`、`startup_delay_ms`、`skip_silence`）与启动缓冲上限 `startup_delay_max_ms`。C++ 侧常量、默认值、上限与整除模数集中在 `rvc/include/rvc/realtime_geometry.hpp`，`validate_realtime_knobs` 单点判定范围、整除与 overlap 定值，`StartStream` 入口与 `AudioWorker` 默认值都引用它。Rust 侧 `core/src/config.rs:140-158` 镜像同一套值。两侧由对拍测试钉住：`tests/test_rvc_realtime_params.cpp` 运行时读 JSON 逐项比对 C++ 常量、按 32k/40k/48k 独立重算几何，并校验 `validate_realtime_knobs` 的接受与拒绝分支；`core/src/config.rs` 的 `realtime_knobs_mirror_params_json` 读同一 JSON 比对 Rust 常量。

### 推导公式

帧率：生成器 100 Hz（hop 160 @ 16 kHz）、HuBERT 50 Hz、倍频 2。输入固定 16 kHz；输出采样率 `sr` 取模型原生值（32000 / 40000 / 48000），升采样比 `sr / 16000`。设 `W = window_samples`、`P = past_context_samples`（16 kHz 样本），`C = crossfade_max_out`、`S = sola_search_max_out`（48 kHz 契约参考样本，运行时按 `sr` 同比缩放）：

- `overlap_out = (C + S) × sr / 48000`；`margin_16k = (C + S) / 3`（按契约参考值算，与 `sr` 无关）
- `front_frames = (P + W + margin_16k) / 160`；`skip_head = P / 160`
- `return_frames = decoder_frames = (W + margin_16k) / 160`
- `hubert_samples = front_frames × 160`；`model_frame_out = sr / 100`；`output_frame_samples = sr / 50`
- `decoder_out_samples = return_frames × model_frame_out`；`output_hop = W × sr / 16000`（接缝网格周期）
- `flow_head = skip_head − 24`；`latent_frames = front_frames − flow_head`；`decoder_head = skip_head − flow_head`

16 kHz 侧帧数与 `sr` 无关，输出侧样本数按 `sr` 缩放。48 kHz 当前值：overlap 2880、margin 960、front 280、skip 250、return/decoder 30、hubert 44800、decoder_out 14400、hop 11520、flow_head 226、latent 54、decoder_head 24、每帧 960。40 kHz：overlap 2400、decoder_out 12000、hop 9600、每帧 800。32 kHz：overlap 1920、decoder_out 9600、hop 7680、每帧 640。（代码确认：`rvc/include/rvc/realtime_geometry.hpp:119-201`、`rvc/src/rvc/audio_worker.cpp:64-73`、`tools/model/realtime_chain_params.py:50-137`）

RMVPE 前端 mel 128 带、n_fft 1024、hop 160、30–8000 Hz、HTK Slaney（代码确认：`rvc/src/rvc/feature_extractor.cpp:342-343`）。实时块帧数 32 是契约常量（`realtime_geometry.hpp:27`），引擎形状校验与补整都引用它（`feature_extractor.cpp:80-82,525-528`）。逐块帧数由几何推导：`f0_samples = W + 800`，向上补到 `32 × 160` 的倍数再减 160，mel 帧数再向上补 32 倍数（`realtime_geometry.hpp:166-173`）；门控按推导值校验引擎（`feature_extractor.cpp:215-220`）。`emb_channels`/`inter_channels`/`source_dim` 由模型定，门控校验 front emb 一致（`rvc/src/rvc/pipeline.cpp:236-240`），不入 JSON 常量。

边界：运行时 `crossfade + sola` 恒等于 `overlap_out`（两键锁为定值，见下方旋钮表）；`P`、`W`、`(P + W + margin_16k)` 整除 160；`return ≤ latent`；`decoder == return`；RMVPE 帧数 32 倍数；模型原生采样率须属 {32000, 40000, 48000}（`is_supported_realtime_sample_rate`）。ms 旋钮整除约束：`startup_delay_ms % 20 == 0`（契约帧网格 20 ms）。overlap 锁为定值 60 ms 后天然落在模型帧网格（10 ms）上，入口不再重复校验该整除（不写不会被触发的守卫）；解耦放开范围时须恢复。违反在入口回 InvalidArgument，不到推理层抛异常。

### 资产生产与运行时

每模型一套 front/decoder（同 pth 成对），共享一套 hubert-realtime/rmvpe-realtime。导出默认值从 JSON 推（`--front-frames/--skip-head/--return-frames/--decoder-frames`、`--samples`、`--frames`），CLI 覆盖仅实验、登记注明偏离。编译校验定长与推导形状；`compile` 段当前写死脚本、`--fp16` 已暴露。引擎绑设备板上编译，build-manifest 记几何来源与哈希。形状类（W/P）改值即新资产变体，v1 单 profile，变体命名待多 profile 阶段定。decoder 输出长度取模型 `dec.upp`（即原生采样率 / 100），采样率同样是资产变体维度；front 形状与采样率无关。（`tools/model/export_generator_split_onnx.py:176`）

门控 `supports_realtime_streaming` 读 `derive_realtime_geometry(model_sr)` 的推导形状，不写字面量：采样率属支持集、HuBERT 样本数、RMVPE 帧数、front 帧数与 emb 通道、decoder 帧数逐项比对引擎形状（`rvc/src/rvc/pipeline.cpp:211-241`）。`[rvc.realtime]` 段存非形状类旋钮，实时独有、不进预设（与模型无关，全局一套）：

| 键 | 默认 | 边界 | 生效 |
| --- | --- | --- | --- |
| `crossfade_ms` | 50 | 定值 50，偏离拒绝 | StartStream |
| `sola_search_ms` | 10 | 定值 10，偏离拒绝 | StartStream |
| `startup_delay_ms` | 340 | 0..=15000，且 % 20 == 0 | StartStream |
| `skip_silence` | true | bool | StartStream / SetParams |

`crossfade_ms + sola_search_ms` 锁为定值 60 ms（50 + 10），等于最大 overlap。入口按「各自 ≤ 上限且和 == 60」判定，等价于两键各锁定值，任何偏离都拒绝；拒绝原因写明「引擎形状按最大 overlap 固定，可调 overlap 尚未实现」。定值按 48 kHz 契约换算：`crossfade_max_out 2400 / 48 = 50`、`sola_search_max_out 480 / 48 = 10`、两者之和 `2880 / 48 = 60`；各采样率的 max_out 同比缩放，故 ms 边界与采样率无关。ms 按模型输出采样率换算样本。`startup_delay_ms` 与 `skip_silence` 不参与块几何，保持可调；前者上限 15000 来自 C++ 输出环 16 s @48 kHz 留 1 s 余量（`rvc/src/rvc/streaming_pipeline.cpp:24`）。

锁定原因是分析窗尚未与引擎形状解耦（代码确认）。资产按 `overlap_ref = 2400 + 480 = 2880` 固定，推出 `margin_16k 960`、front 280 帧、decoder 30 帧（`rvc/include/rvc/realtime_geometry.hpp:142-159`）；`StreamingRvc` 的分析窗却按实际旋钮值算（`rvc/src/rvc/streaming_pipeline.cpp:40-43`）。淡化 40 ms + SOLA 10 ms 得分析窗 279 帧，与引擎 280 帧不符，推理层每块抛异常、输出补零（`rvc/src/rvc/streaming_pipeline.cpp:286-296`）。只有默认 50 + 10 = 60 ms 与引擎形状一致；main 上这两个值是写死常量，永远匹配。锁为定值即回到 main 的既有几何行为，音质不退化。解耦做法与解除条件见改进点「淡化与 SOLA 真正可调」。

延迟影响（推断，按代码公式推算，非实测听感）：48 kHz 单块 decoder 输出 = 11520 样本 + 淡化 + SOLA，默认 14400 样本 = 300 ms。淡化调小 10 ms 只省 10 ms 块长，而净滞后约 0.5 s 中 400 ms 是启动缓冲（板端实测见 `reports/realtime-pipeline-20261112/RESULTS.md`）。降延迟的可用旋钮是 `startup_delay_ms`，不是调小淡化。

四键归 R 类运行时键（overlap 两键只接受定值）：`config set` 修改 `skip_silence` 走 `SetParams` 块边界无中断生效，`startup_delay_ms` 走 `StopStream`→`StartStream`（[SPEC-04](SPEC-04-daemon.md)，`daemon/src/services.rs`），复用同一失败恢复与降级语义。形状类不做运行时调参。

验收：形状推导 C++/Python 双镜像单测（`tests/test_rvc_window_geometry.cpp` 黄金锚点、`tests/test_rvc_realtime_params.cpp` 读 JSON 对拍并按 32k/40k/48k 重算、`tools/model/test_realtime_chain_params.py`）；旋钮范围、整除与 overlap 定值拒绝由 `validate_realtime_knobs` 与 `core::config` 两侧单测钉住；流式 golden 对拍按变体跑（`run_streaming_reference.py` 24 帧窗参考需泛化到契约块长）；板端按 [RVC-FILE-BENCHMARK](../RVC-FILE-BENCHMARK.md) 四层适配实时（接缝网格 = `output_hop`），记推理耗时/净延迟/接缝伪影比/音分配对差，变体入 `reports/`；非形状类固定形状扫 `startup_delay_ms` 与 `skip_silence`，按延时 + 效果定值，crossfade/sola 扫描待解耦后做。

## 改进点

- [x] 数据面 socket 初始化任何失败均释放已取得的描述符；连接失败不留下新输入绑定。 见[本机回归](../../reports/postprocessing-stability-20261124/RESULTS.md)。
- [x] 正常块、跳静音和失败补零均在发布临界区核对代次。重置后的首个推理窗会清掉旧淡化尾和缓存；回归覆盖成功、短输出、异常和并发跳静音。
- [x] 文件结果末帧携带有效样本数；daemon 校验回帧的序号、采样率和有效长度，再发布准确采样长度的 WAV。本机协议桩覆盖有信号的非整帧尾样本。

- [ ] 模型主导参数动态化（多模型适配，总方向）：凡由模型决定的参数（采样率、帧率、hop、特征维度、上采样比、合成帧数 T、音高量化）从模型动态读取，据此推导分窗几何、推理模式（整段/动态/分窗）与重采样，不写死常量。实时形状已改为按 JSON 几何常量加模型采样率与 emb 通道推导，门控不再写字面量（`rvc/src/rvc/pipeline.cpp:211-241`）；`W`/`P` 仍是单 profile 自由参数，未由模型能力推导。前置的跨模型实测已覆盖 40k/48k 共 10 个模型（文件链路）。
- [ ] 文件分窗几何动态化：398 只作默认参考；窗长、hop、pad 和超 profile 分块由引擎 min/opt/max、T、采样率与输入时长推导；拟增 `[file].chunk_frames`。验收：10/30/120 s 的 F0、log-mel、内存和延迟，22 s 内整段结果不变。（`rvc/src/rvc/inferencer.cpp:26,262-266,336-346`）
- [ ] 文件链路优先大窗资产：编译时为文件链路提供覆盖目标时长的大窗或动态大 profile；加载器区分实时/文件资产，长文件不默认 T=200。验收：同输入的窗数、F0、log-mel、显存和延迟。
- [ ] 文件小窗质量方向：必须小窗时，先做互相关/SOLA 对齐，再用等功率淡化；评估真实前后文、跨窗 F0 平滑、窗级增益匹配和噪声/相位连续性。验收看 F0 P90、log-mel、边界阶跃、显存与延迟。
- [ ] 实时链路单独优化：实时模型可与文件不同，能力区分仅文件/仅实时/两者；按 240 ms 块、overlap 定值 60 ms、340 ms 启动单独调优；40 kHz 见上方实时原生多采样率条。几何与旋钮契约见实时几何与资产参数节。验收：48 kHz 无回归，具备实时资产的新模型起流、延迟 P95 与连续性。
- [ ] 淡化与 SOLA 真正可调（解耦分析窗与淡化量）：分析窗恒按引擎形状喂帧（16 kHz 侧 280 帧，与采样率无关，`margin_16k` 按最大 overlap 算），实际 `crossfade_ms`/`sola_search_ms` 只用于 overlap-add 与 SOLA 搜索，不再决定分析窗长度（耦合点 `rvc/src/rvc/streaming_pipeline.cpp:40-43`，拼接点同文件 `349-360`）。解耦后入口才放开两键范围；现状锁为定值 60 ms，原因见实时几何与资产参数节。现在不做：引擎形状按最大 overlap 固定，解耦要同时改取窗与拼接两处，且淡化量变化的音质与接缝收益须板端声学对拍才有意义，单独开一轮。解除条件：完成解耦，并通过板端 32k/40k/48k 声学对拍与接缝听感验证（按 [RVC-FILE-BENCHMARK](../RVC-FILE-BENCHMARK.md) 四层适配实时），再放开范围校验、恢复 overlap 的 10 ms 整除校验（锁定期间作为不可达守卫已移除），并扫淡化/SOLA 定值。
- [ ] 板端剩余未验证：延迟 P95/连续性/长跑稳定性、`model use` 与 `SetParams` 在线调参（变调/调检索比/切换 `skip_silence`）期间的接缝声学听感与音质主观评价。阻塞原因：现有套件仅对 Loopback 静音流置 `skip_silence=false` 驱动推理并断言日志计数、块递增、帧守恒与 `last_error=None`，不测延迟 P95 也不做带人声输出的听感评价。解除条件：在 Jetson 上向 Loopback 播放测试人声同时执行在线调参与 `model use`，录制输出音频并按 `reports/` 测量流程做声学波形与主观听感验收。32 kHz 实时冒烟已由上条覆盖。
- 板端已实测（2026-11-18，接入 `SetParams` 后）：`tools/jetson-e2e.sh --unit --smoke --preset --realtime-mm --file` 全部通过（设备端 `cargo test` 30 套 647 项、`ctest` 10/10 全部通过；三进程冒烟与 `--file` 通过；`--preset` 8 场景含实时 `preset use` 通过；`--realtime-mm` 10 场景全过，含新增 `SetParams 在线调参不断流` 场景：6 次调参均落入 `rvc.log`、`StartStream` 计数未增、`frames_in` 与 `blocks` 单调递增、`帧守恒=94`、`last_error=None`）。
- 本机已实测（2026-11-18，接入 `SetParams` 后）：`tools/ci-local.sh all` 通过（repo、lint、native、rust 等全部本地检查；含 C++ `RVCInferencer`/`RealRVCPipeline`/`StreamSession` 与 Rust `rvc_params`/`preset` 在线调参用例）。
- [ ] Python 镜像单测接入检查入口：`tools/model/test_realtime_chain_params.py` 覆盖 32k/40k 几何与旋钮边界，但 `tools/ci-local.sh` 与 CTest 都未调用它，目前只能人工运行。解除条件：在检查脚本加一个 Python 单测调用。
- [ ] 长文件改分块，限制内存。（FUTURE）正文已定推迟（分块流式见 FUTURE），本条同步标推迟。
- [ ] 离线推理三条路径（动态整段 / 静态整段 / 分窗）各自重复 `apply_rms_mix` → `resample` → `limit_peak`；抽公共收尾，避免改一处漏两处导致不同导出形状听感不一致。（`rvc/src/rvc/inferencer.cpp:280-284,318-322,426-430`）
- [ ] 多说话人模型缺 per-job 选说话人：`spk_id` 只从 config 读，202 个说话人只用到 0。需每任务参数（与 SetParams 协同）。（`rvc/src/rvc/inferencer.cpp:800-801`）
- [ ] 小项：`infer_window` 无条件复制 feats，protect 关闭时白复制（`rvc/src/rvc/inferencer.cpp:703`）；`apply_rms_mix` 逐样本 `std::pow`，可在 RMS 网格算再插值（`rvc/src/rvc/inferencer.cpp:924-930`）。

## 实时变声衔接自然度与延迟分步优化

本节用直白背景说明实时变声的流水线机制、当前听感瑕疵（咔嗒声、轻微颤动、字头怪音）的根因，以及按步实现、按步验收的待办清单。

### 背景：实时变声流水线是怎么工作的

1. **20 ms 小包收音**：`pre` 每 20 ms（0.02 s）从麦克风采一小包音频（16 kHz 下 320 个采样点）发给 `rvc`，并按 1:1 节拍从 `rvc` 取回一包变声结果播放。
2. **240 ms（约一个字）攒块推理**：模型只看 20 ms 无法识别字音和音调。`rvc` 每攒够 **240 ms（12 包，约一个字的长度）** 触发一次 GPU 推理；每次推理把 **过去 2.5 s 历史声音 + 刚收到的 240 ms 新声音 + 60 ms 重叠余量**（合计 2.8 s = 280 帧）一起喂给模型，最后只取出 **最新 300 ms（240 ms 正文 + 60 ms 重叠区）** 的合成波形。
3. **60 ms 块间重叠拼接（SOLA + 交叉淡化）**：每一块算出 300 ms 后，先播出前 240 ms，把末尾 **50 ms 留作「尾巴」（`tail_`）**。下一块算出来后，先在开头 **10 ms 搜索窗（SOLA）** 内左右微调找到与上一段「尾巴」波形波峰最对齐的位置，再用 **50 ms 交叉淡化**（上一段尾巴渐弱、下一段开头渐强）混合拼成连续声音。

### 衔接自然度四步待办（按步推进、每步独立验收）

- [x] **步骤 1：修复静音跳过（`skip_silence`）硬切尾巴与历史音调记忆错位（解决停顿处咔嗒声与再开口怪音）**
  - **问题根因**：
    1. **停顿硬砍尾巴**：当 240 ms 块内无语音活动（VAD=0，如换气或字间停顿）触发 `skip_silence` 时，代码直接把上一个字留下的 50 ms `tail_` 清零丢弃并输出纯 0（`rvc/src/rvc/streaming_pipeline.cpp:258-268`），波形从有幅值突然阶跃到 0，产生咔嗒声；停顿结束再开口的首块也直接硬起无淡入。
    2. **历史音调记忆未随静音推进**：模型内部用 `realtime_pitch_cache_`（1024 帧）记住过去 2.5 s 的音高曲线。静音块跳过推理时，没有向该缓存推进这 240 ms（24 帧）的静音零音高（`rvc/src/rvc/inferencer.cpp:697-708`）。导致停顿后再开口时，过去 2.5 s 历史音频已滑过静音区，但配对的历史音高还停留在停顿前的旧字上，音画错位引发字头跑调或突发爆音（实测 `gkw30` 在 `21.46 s` 静音过渡处出现 19 倍跳变峰值）。
  - **改动方案**：
    1. 有声→静音跳过时，若 `tail_valid_` 为真，将残留 50 ms `tail_` 按余弦/线性淡出写入本块输出开头，再补零并置 `tail_valid_ = false`；静音→有声恢复的首块（`!tail_valid_`）对开头做短淡入。
    2. 静音跳过块同步通知 `pipeline` 向 `realtime_pitch_cache_` 推进 `hop`（24 帧）无声帧，保持历史音高缓存与输入音频环的时间轴严格对齐。
  - **独立验收**：C++ 单测验证静音跳过前后 `realtime_pitch_cache_` 帧偏移守恒、有声↔静音边界无单样本阶跃；板端复跑 `gkw30` 并人工试听停顿/起音，确认消除停顿咔嗒声与 `21.46 s` 类过渡爆音。

- [x] **步骤 2：放宽时间戳抖动误重置与加固播放缓冲（解决偶发断音咔嗒声与吞字，跨 SPEC-02）**
  - **问题根因**：
    1. **网络/调度抖动误触发全量清空**：`StreamingRvc::push` 检查相邻两包采集时间戳 `pts_ns` 差值 > 200 ms 即判定断流（`rvc/src/rvc/streaming_pipeline.cpp:124-133`）。在 `jetson-audio.py` SSH 桥接或系统调度偶发抖动时，哪怕包序号 `frame_idx` 完全连续、一包没丢，只要单次送达间隔超 200 ms 就会触发 `handle_discontinuity()` 清空 2.5 s 历史、清空输出环并重新沉默等待 **400 ms 启动缓冲 + 240 ms 攒块**，说话中途被硬生生吞掉 0.64 s。
    2. **声卡播放缓冲仅留 1.33 ms 余量**：`pre` 播放侧详见 [SPEC-02 实时播放缓冲余量与欠载平滑](SPEC-02-pre.md#改进点)。
  - **改动方案**：
    1. `StreamingRvc::push` 仅在 `frame_idx` 出现真实缺口/回退（`idx_gap == 0 || idx_gap > kFrameIdxResetGap`）或 `pts_ns` 单调回退时才触发全量重置；`frame_idx` 连续时的单纯送达延迟抖动不重置流状态。
    2. 配合 SPEC-02 加大播放 ALSA 缓冲余量并平滑接收环短时欠载。
  - **独立验收**：C++ 单测验证 `frame_idx` 连续但 `pts_ns` 间隔 > 200 ms 时 `resets` 计数保持 0、输出连续；板端运行 30 s 实时流与 `jetson-audio.py` 对讲，`resets == 0`、`pre.log` 播放 underrun 为 0。

- [ ] **步骤 3：跨块声带初相/气流噪声连续化与 SOLA 弱音防抖（实验后已回滚）**
  - **回滚实测依据（2026-11-25 四相位锁死直驱消融）**：在固定块网格相位（`0/60/120/180 ms` 均值）下，原实时绝对网格初相/噪声与 `3e-3` SOLA 门限均造成真实退化：
    1. 强行向 RVC v2 实时解码器传入非零累计 `source_phase[0]`（训练期基波初相恒 0，且累计周期未耦合 SOLA 偏移）反而破坏 SOLA 对齐（`sola_span 10.94 → 13.61 ms`、`seam_wav 0.7147 → 0.7226`）；
    2. 逐块计算 2.4 万次 `splitmix64 + Box-Muller` 噪声额外增加约 `+4.9 ms` CPU 耗时且 `logmel_l1` 略退 `0.07 dB`；
    3. `3e-3`（-50 dBFS）SOLA 门限误拦可听轻辅音/字尾对齐（`logmel_l1 +0.08 dB`），降至 `1e-3` 亦无净收益。
  - **当前策略**：实时路径保持轻量 `realtime_random_`（基波初相 0）与原 SOLA 归一化相关；后续仅在引入带 SOLA 偏移反馈或独立听感验证的新方案后再重启。

- [x] **步骤 4：实时链路补齐清辅音保护（`protect`）、音量跟随（`rms_mix_rate`）与峰值限幅（解决字头吐字发虚与音量跳变）**
  - **问题根因**：文件链路在特征检索后按 `pitchf` 对清辅音（s / sh / t / k 等无基频帧）保留 `protect` 比例的原始未检索特征，并在输出后按原声 RMS 包络做 `rms_mix_rate` 音量对齐和 `0.99` 限幅；实时链路 `infer_realtime` 目前完全未执行这两步（`rvc/src/rvc/inferencer.cpp:719-745,837-854`），导致开启 `index_rate` 时字头清辅音被训练集特征覆盖发粘，且字尾呼吸声容易被模型放大。
  - **改动方案**：在 `infer_realtime` 中保存尾部检索前特征副本，对 `return_frames`（或送入 `front` 的新块区间）内 `pitchf <= 0` 的帧按 `protect` 混合原始特征；对 `decoder` 输出的 300 ms 音频与对应的 300 ms 输入音频做块级短窗 `rms_mix_rate` 包络对齐与 `0.99` 峰值限幅。
  - **独立验收**：C++ 单测验证实时模式下 `protect` 与 `rms_mix_rate` 改变输出且边界值（`protect=0.5`、`rms_mix_rate=1.0`）符合契约；板端实测单块推理耗时增量 < 2 ms，对比 `logmel_vs_file` / `rms_vs_file` 指标与字头吐字清晰度。

- [x] **步骤 5：实时音高关闭无声插值（恢复声码器清浊音开关与 `protect` 生效，减轻电音感）**
  - **问题根因**：`f0_to_pitch` 将 RMVPE 检出的无基频帧（`f0 <= 0`）全部线性插值覆盖并同时写入 `pitch` 与 `pitchf`，导致声码器 NSF 激励源在清辅音与停顿帧仍持续注入正弦谐波（产生电音），同时 `apply_realtime_protect` 因 `pitchf > 0` 几乎无法在清辅音触发。
  - **改动方案**：`RVCInferencer::f0_to_pitch` 增加 `interpolate_unvoiced` 开关；实时链路 `infer_realtime` 传入 `false`，保留无基频帧为 `pitch = 1` 且 `pitchf = 0.0f`；离线文件链路 `infer` 保持 `true` 以维持离线对拍口径不变。
  - **独立验收**：C++ 单测验证 `f0_to_pitch(..., false)` 在混合清浊音序列中保持无声帧 `pitch=1 / pitchf=0`、有声帧正常移调；板端四起点三档对照（`interp` / `pitchf0` / `native`）显示 `native` 静音底噪降 22.5%（`0.0071 → 0.0055`）、人声/背景信噪比升 `+0.95 dB`、停顿跳变降 21%、SOLA 标准差从 `3.62` 降至 `3.39 ms`，实听电音明显减轻。

---

### 实时延迟拆解与分阶段降延迟调查

板端物理回录实测当前实时变声稳态净延迟约 **500 ms**（`reports/realtime-pipeline-20261112/RESULTS.md`）。按数据流逐站拆解，这 500 ms 的真实去向如下：

| 延迟环节 | 当前耗时 | 为什么会产生这段延迟（大白话解释） | 能否压缩 |
| --- | --- | --- | --- |
| **1. 启动起播水位（`startup_delay_ms`）** | **400 ms** | `StreamingRvc::pop_output` 在流启动后故意先吐 **400 ms（20 包）纯静音**，同时后台攒块推理，等 400 ms 倒计时走完才开始从输出环吐真实变声（`rvc/src/rvc/streaming_pipeline.cpp:494-501`）。因为后面 1 进 1 出，**开头多等的余量会永久变成后续每一句话的排队滞后**。实际上第一块在第 240 ms 收齐、第 ~325 ms（240 + 81 ms 推理）就已算完入环，400 ms 倒计时比首块出炉还多等了约 80 ms，且稳态推理（81 ms）远快于块周期（240 ms），无需 400 ms 这么厚的垫子。 | **立即可压（免重编引擎）**：调至 `280~320 ms` 可直接省 **80~120 ms**；若改为「首块算完后再留 20~40 ms 防抖余量即起播」，在现有 81 ms 推理下可省 **~120 ms**。 |
| **2. 单块攒音时长（`window_samples`）** | **240 ms** | 必须先收满 240 ms（12 包）新声音才能开始算这一块。这是流式分块的固有节拍下限。 | **需重导/重编引擎**：从 240 ms 缩到 160 ms（8 包）可再省 **80 ms**，前提是单块推理耗时 < 160 ms（当前 81 ms 已满足）。 |
| **3. GPU 单块推理耗时（`infer_ms`）** | **~81 ms**（p95 87 ms） | 攒满 240 ms 后，GPU 跑 HuBERT + RMVPE + front + decoder + SOLA 耗时约 81 ms。在「首块算完即播」机制下，**推理每快 10 ms，总延迟就少 10 ms**。目前 5 个实时引擎均为 **FP32** 编译（`realtime_chain_params.json` 中 `"fp16": false`），且携带跨设备 engine 警告；此外 HuBERT 每次重复重算全部 2.8 s（44800 样本）的卷积特征。 | **重编引擎可压**：板端同机开启 `--fp16` 重编 TensorRT 引擎，预计单块推理可从 ~81 ms 降到 **~45–55 ms**（省 **25~35 ms**）。 |
| **4. 音频进程（`pre`）采集 + DSP + 播放管道** | **~45 ms** | ALSA 采集攒 20 ms 帧 + WOLA 降噪 64 样本（1.33 ms）+ 跨进程 socket + 播放缓冲（约 20 ms）。若开启可选 `UL-UNAS` 神经网络增强，会额外增加 2 帧积攒（40 ms，实测 p50 45.9 ms）。 | 已接近 20 ms 帧契约物理下限，保持不动。 |

#### 降延迟三步待办（按成本从低到高推进）

- [x] **降延迟步骤 A（配置级 `startup_delay_ms` 扫描与下界验证）**：实测验证流式块化启动延迟硬下界满足 $D_{\min} = W + T_{\text{infer,max}}$。原始 FP32 引擎下（大索引 `bocchi-v2-48k` 单块 177 ms，含 88.6 ms 标量索引检索），下调 `startup_delay_ms` 至 `320/280/240/200 ms` 不会降低净延迟（维持 ~582 ms），反而导致 `pre_gap_frames` 线性增至 `6~10`；在完成 `IndexSearch` NEON 向量化 + 质心分块 + 多线程并行优化（单块 177 ms → 101 ms）后，既有 48k/40k FP32 资产在 `startup_delay_ms = 340 ms` 均达零缺帧（净延迟 ~479 ms，较基线降 ~103 ms），已将默认 `startup_delay_ms` 同步更新为 `340`。
- [x] **降延迟步骤 B（同机 `--fp16` 重编实时引擎与索引加速）**：在 Jetson 本机使用 `build_realtime_engines.py --fp16` 重建四引擎并消除跨设备 plan 警告；配合 `IndexSearch` 加速（88.6 ms → 6.0 ms），`bocchi-v2-48k`（`index_rate=0.8`，3561 质心 / 138915 向量）单块推理从 `176.7 ms` 降至 `58.6 ms`（P95 `63.8 ms`，3.0× 提速），声学近无损（vs 文件参考 Log-Mel L1 `4.864` vs FP32 `4.742 dB`，RMS L1 `2.691` vs `2.529 dB`）；`startup_delay_ms = 320 ms` 实测物理回录净延迟 `460.2 ms`（`-122.2 ms`），60 s 连续长跑 253 块零欠载、零缺帧、零重置。已将 `realtime_chain_params.json` 的 `compile.fp16` 默认设为 `true`。
- [x] **降延迟步骤 C（短块 profile 实验：240 ms → 160 ms）**：在隔离目录完成 `window_samples = 2560`（160 ms 块，`front=272`、`decoder=22`、`hubert=43520`、`rmvpe=32`）全套 FP16 资产导出与板端实测：单块推理 `51.3~52.4 ms`（P95 `56.9~62.7 ms`，GPU 占空比 32.8%），`startup_delay_ms = 240 / 220 ms` 均通过 60 s 长跑（378 块零欠载、零缺帧、零重置），物理回录净延迟压至 **`385.6 / 365.6 ms`**（相对 `direct` 纯 RVC 滞后 `292 / 266 ms`，较基线降 **`216.8 ms` / `-37.2%`**）。但 160 ms 块相对 240 ms 存在轻微声学代价（Log-Mel L1 `+0.11~0.23 dB`、RMS L1 `+0.21~0.35 dB`、接缝频率 6.25 vs 4.17 缝/s）且破坏既有 240 ms 资产形状兼容，故**暂不切换主干默认块长**，完整数据归档于 [realtime-quality-latency-20261125/RESULTS.md](../../reports/realtime-quality-latency-20261125/RESULTS.md)。
- [x] **降延迟步骤 D（板端定频 + `TrtEngine` CUDA 锁页缓冲并行）**：板端启用 `jetson_clocks` 锁定 CPU/GPU/EMC 最高频，消除 240 ms 块间隙空闲降频（`1020 → 306 MHz`），单块 FP16 推理从 `56.8 ms` 降至 `32.2 ms`（P95 `32.4 ms`）；随后在 `TrtEngine`（`rvc/include/rvc/trt_engine.hpp`、`rvc/src/rvc/trt_engine.cpp`）为每个张量引入复用 CUDA 锁页缓冲（`cudaMallocHost` / `cudaFreeHost`）消除非锁页 `cudaMemcpyAsync` 的隐式同步，单块推理进一步降至 **`29.89 ms`**（P95 `30.05 ms`，逐位误差 `0.0000000`），240 ms 切块在 `startup_delay_ms = 260 ms` 下零缺帧达成 **`398~405 ms`** 净延迟（较基线降 `~184 ms`）。后续补充锁页缓冲长跑与动态尺寸切换压测，并将开机定频与 `mozart` 启动纳入受控 `systemd` 单元及部署适配。