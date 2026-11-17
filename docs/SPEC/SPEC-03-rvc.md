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
| 实时 | 延迟优先 | 必须实时 front/decoder 与实时 HuBERT/RMVPE；模型可与文件链路不同 | 固定 240 ms 块、2.5 s 过去上下文；淡化 50 ms 与 SOLA 10 ms 为定值，启动 400 ms 可调；输出采样率随模型，块几何由采样率推导 |

- 同一模型可以同时具备文件和实时能力；缺实时资产时只提供文件能力，不静默降级。
- 文件链路的大窗/小窗策略不能直接搬到实时链路；实时链路已有的 SOLA、淡化与启动缓冲按实时约束单独优化。

## 已确认

- [x] 推理核心保持独立进程。
- [x] 非推理逻辑归 Rust。
- [x] C++ 进程只保留推理：`StreamSession` 封装 `StreamingRvc` 和 `RVCInferencer`；`RealtimeRvcWorker` 与 `AudioWorker` 的外层传输、队列、模式、统计归 Rust。
- [x] 静音和旁路归音频进程；C++ 不实现这两个概念。
- [x] 参数在 `StartStream` 时固定；v1 运行时调参经 `config set rvc.<会话参数>` 写持久基线，由 daemon 编排应用（见 [SPEC-04](SPEC-04-daemon.md)），`[rvc.realtime]` 四键同口径走 Stop→Start；模型切换时 `[rvc]` 会话参数由该模型的预设载入（见 [SPEC-11](SPEC-11-preset.md)），`StartStream` 契约不变。实时换模型由 daemon `model use` 断流编排（SPEC-04），rvc 不新增命令。块边界 `SetParams` 仍推迟，见 [FUTURE.md](../FUTURE.md)。
- [x] 输出节拍由输入帧驱动：C++ 每收到一帧输入就回一帧输出，输出环不足时补零。启动缓冲留在 `StreamingRvc.startup_delay_out`（默认 400 ms，`startup_delay_ms` 旋钮），音频进程只保留小缓冲吸收送达抖动。大缓冲只有一个主人。
- [ ] 1:1 泵假设采集与播放时钟同源；使用不同声卡时，长期漂移会导致周期性欠载或缓冲增长，需时钟漂移补偿（见 [FUTURE.md](../FUTURE.md)）。推迟项，未实现。
- [x] 板端实测确认小缓冲布局成立：同源 60 s 长跑 49.86 帧/s 1:1、丢弃与欠载零增长，rvc 端到端净滞后约 0.5 s（400 ms 启动缓冲为主）。维持启动缓冲留 C++、音频进程小缓冲的现状；数据见 `reports/realtime-pipeline-20261112/RESULTS.md`。

## 接口约束

- [x] 粗粒度边界：数据面只有“输入帧进、输出帧出”，控制面只有加载、启动、停止、重置和状态查询；不暴露张量、引擎和 stream。
- [x] 数据面沿用 core 的帧契约：16 kHz 输入，一帧一条消息。实时输出按模型原生采样率（32/40/48 kHz）填 `sample_rate` 与 `valid_samples`；文件输出仍固定 48 kHz，离线收尾重采样。字段与偏移见 SPEC-01。（`rvc/src/rvc/audio_worker.cpp:137-139`、`rvc/src/rvc/file_worker.cpp:116-117`）
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
| `StartStream` | Rust 控制命令 | `kind`、`model_handle`、`pitch_shift`、`index_rate`、`rms_mix_rate`、`protect`、`f0_method`；实时模式另带四个 `[rvc.realtime]` 旋钮（可选，缺省用 JSON 默认）；文件模式另带 `total_frames`、可选 `total_samples`，见下文 |
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
| `startup_delay_ms` | 400 | 0..=15000，且 % 20 == 0 | StartStream |
| `skip_silence` | true | bool | StartStream |

`crossfade_ms + sola_search_ms` 锁为定值 60 ms（50 + 10），等于最大 overlap。入口按「各自 ≤ 上限且和 == 60」判定，等价于两键各锁定值，任何偏离都拒绝；拒绝原因写明「引擎形状按最大 overlap 固定，可调 overlap 尚未实现」。定值按 48 kHz 契约换算：`crossfade_max_out 2400 / 48 = 50`、`sola_search_max_out 480 / 48 = 10`、两者之和 `2880 / 48 = 60`；各采样率的 max_out 同比缩放，故 ms 边界与采样率无关。ms 按模型输出采样率换算样本。`startup_delay_ms` 与 `skip_silence` 不参与块几何，保持可调；前者上限 15000 来自 C++ 输出环 16 s @48 kHz 留 1 s 余量（`rvc/src/rvc/streaming_pipeline.cpp:24`）。

锁定原因是分析窗尚未与引擎形状解耦（代码确认）。资产按 `overlap_ref = 2400 + 480 = 2880` 固定，推出 `margin_16k 960`、front 280 帧、decoder 30 帧（`rvc/include/rvc/realtime_geometry.hpp:142-159`）；`StreamingRvc` 的分析窗却按实际旋钮值算（`rvc/src/rvc/streaming_pipeline.cpp:40-43`）。淡化 40 ms + SOLA 10 ms 得分析窗 279 帧，与引擎 280 帧不符，推理层每块抛异常、输出补零（`rvc/src/rvc/streaming_pipeline.cpp:286-296`）。只有默认 50 + 10 = 60 ms 与引擎形状一致；main 上这两个值是写死常量，永远匹配。锁为定值即回到 main 的既有几何行为，音质不退化。解耦做法与解除条件见改进点「淡化与 SOLA 真正可调」。

延迟影响（推断，按代码公式推算，非实测听感）：48 kHz 单块 decoder 输出 = 11520 样本 + 淡化 + SOLA，默认 14400 样本 = 300 ms。淡化调小 10 ms 只省 10 ms 块长，而净滞后约 0.5 s 中 400 ms 是启动缓冲（板端实测见 `reports/realtime-pipeline-20261112/RESULTS.md`）。降延迟的可用旋钮是 `startup_delay_ms`，不是调小淡化。

四键归 R 类运行时键（overlap 两键只接受定值），`config set` 走 Stop→Start：[SPEC-04](SPEC-04-daemon.md) 的编排判据已纳入本段四键（`daemon/src/services.rs:957-965`），复用其失败恢复与降级语义。形状类不做运行时调参。

验收：形状推导 C++/Python 双镜像单测（`tests/test_rvc_window_geometry.cpp` 黄金锚点、`tests/test_rvc_realtime_params.cpp` 读 JSON 对拍并按 32k/40k/48k 重算、`tools/model/test_realtime_chain_params.py`）；旋钮范围、整除与 overlap 定值拒绝由 `validate_realtime_knobs` 与 `core::config` 两侧单测钉住；流式 golden 对拍按变体跑（`run_streaming_reference.py` 24 帧窗参考需泛化到契约块长）；板端按 [RVC-FILE-BENCHMARK](../RVC-FILE-BENCHMARK.md) 四层适配实时（接缝网格 = `output_hop`），记推理耗时/净延迟/接缝伪影比/音分配对差，变体入 `reports/`；非形状类固定形状扫 `startup_delay_ms` 与 `skip_silence`，按延时 + 效果定值，crossfade/sola 扫描待解耦后做。

## 改进点

- [ ] 模型主导参数动态化（多模型适配，总方向）：凡由模型决定的参数（采样率、帧率、hop、特征维度、上采样比、合成帧数 T、音高量化）从模型动态读取，据此推导分窗几何、推理模式（整段/动态/分窗）与重采样，不写死常量。实时形状已改为按 JSON 几何常量加模型采样率与 emb 通道推导，门控不再写字面量（`rvc/src/rvc/pipeline.cpp:211-241`）；`W`/`P` 仍是单 profile 自由参数，未由模型能力推导。前置的跨模型实测已覆盖 40k/48k 共 10 个模型（文件链路）。
- [x] 实时原生多采样率（内部动态采样率）：实时链路内部按模型原生采样率工作（32k/40k/48k）。窗、hop、交叉淡化、SOLA、启动缓冲与输出帧长都由采样率推导，40 kHz 模型不再逐块重采样。播放侧不协商采样率：固定 48 kHz，由 pre 在播放边界重采样（SPEC-02）。帧契约扩 `sample_rate`/`valid_samples`（SPEC-01）。（`rvc/include/rvc/realtime_geometry.hpp:119-201`、`rvc/src/rvc/audio_worker.cpp:64-73,137-139`、`rvc/src/rvc/streaming_pipeline.cpp:31-42`）板端验收见下方板端条目。
- [x] 采样率生态与原生多采样率方案调研：公开资料调研 RVC 常见采样率（32k/40k/48k）、上游实时推理假设、导出与 front/decoder 形状、ALSA/USB 播放采样率约束，结论与实现边界入 `reports/realtime-sample-rate-20261116/RESULTS.md`。
- [x] 实时流中换模型（推理侧）：`LoadModel` 支持活动流期间原子换入 inferencer，空闲切换先释放旧 inferencer 防 OOM。（`rvc/src/rvc/pipeline.cpp:142-160`、`rvc/control/stream_session.cpp:519-540`）实时换模型定为断流切换（SPEC-04 `model use`）：`StartStream` 重建 `StreamingRvc` 并 `reset_realtime`，过去上下文、SOLA 状态与音高缓存不跨模型保留，跨采样率按新采样率重建几何。（`rvc/src/rvc/audio_worker.cpp:62-79`）板端接缝与帧守恒见下方板端条目。
- [x] 文件链路多模型：静态定长、静态整段与动态生成器（40k/48k）共 10 个模型板端完成，输出按输入时长统一收口，`frames_in == frames_out`；空闲模型切换先释放旧 inferencer 并腾出缓存槽位，避免旧模型显存未释放时触发 TensorRT OOM 回退 ONNX；活动流期间保持旧模型直到新模型换入。（`rvc/src/rvc/inferencer.cpp:243-442`、`rvc/src/rvc/pipeline.cpp:136-151`、`rvc/src/rvc/model_loader.cpp:307-342`、`reports/multi-model-file-20261115/RESULTS.md`）
- [ ] 文件分窗几何动态化：398 只作默认参考；窗长、hop、pad 和超 profile 分块由引擎 min/opt/max、T、采样率与输入时长推导；拟增 `[file].chunk_frames`。验收：10/30/120 s 的 F0、log-mel、内存和延迟，22 s 内整段结果不变。（`rvc/src/rvc/inferencer.cpp:26,262-266,336-346`）
- [ ] 文件链路优先大窗资产：编译时为文件链路提供覆盖目标时长的大窗或动态大 profile；加载器区分实时/文件资产，长文件不默认 T=200。验收：同输入的窗数、F0、log-mel、显存和延迟。
- [ ] 文件小窗质量方向：必须小窗时，先做互相关/SOLA 对齐，再用等功率淡化；评估真实前后文、跨窗 F0 平滑、窗级增益匹配和噪声/相位连续性。验收看 F0 P90、log-mel、边界阶跃、显存与延迟。
- [ ] 实时链路单独优化：实时模型可与文件不同，能力区分仅文件/仅实时/两者；按 240 ms 块、overlap 定值 60 ms、400 ms 启动单独调优；40 kHz 见上方实时原生多采样率条。几何与旋钮契约见实时几何与资产参数节。验收：48 kHz 无回归，具备实时资产的新模型起流、延迟 P95 与连续性。
- [ ] 淡化与 SOLA 真正可调（解耦分析窗与淡化量）：分析窗恒按引擎形状喂帧（16 kHz 侧 280 帧，与采样率无关，`margin_16k` 按最大 overlap 算），实际 `crossfade_ms`/`sola_search_ms` 只用于 overlap-add 与 SOLA 搜索，不再决定分析窗长度（耦合点 `rvc/src/rvc/streaming_pipeline.cpp:40-43`，拼接点同文件 `349-360`）。解耦后入口才放开两键范围；现状锁为定值 60 ms，原因见实时几何与资产参数节。现在不做：引擎形状按最大 overlap 固定，解耦要同时改取窗与拼接两处，且淡化量变化的音质与接缝收益须板端声学对拍才有意义，单独开一轮。解除条件：完成解耦，并通过板端 32k/40k/48k 声学对拍与接缝听感验证（按 [RVC-FILE-BENCHMARK](../RVC-FILE-BENCHMARK.md) 四层适配实时），再放开范围校验、恢复 overlap 的 10 ms 整除校验（锁定期间作为不可达守卫已移除），并扫淡化/SOLA 定值。
- [x] 实时几何单一来源落地：`realtime_chain_params.json` 由 `tools/model/realtime_chain_params.py` 读取，导出与编译脚本取默认值并校验形状，C++ 门控与常量镜像在 `realtime_geometry.hpp`（见实时几何与资产参数节）。（`tools/model/export_generator_split_onnx.py`、`export_hubert_onnx.py`、`export_rmvpe_onnx.py`、`build_realtime_engines.py`、`tests/test_rvc_realtime_params.cpp`）
- [x] `[rvc.realtime]` 段：`core::config` 四键 + 范围、整除与 overlap 定值校验 + schema + `StartStream` 携带 + SPEC-04 `config set` Stop/Start 判据纳入本段键。（`core/src/config.rs:140-158,735-765`、`rvc/control/stream_session.cpp:283-327`、`daemon/src/services.rs:189-192,957-965`）
- [x] RMVPE 逐块帧推导复核：帧数由 `derive_realtime_geometry` 推导并进门控，补整引用同一常量；架构文档「音高缓存 1024 帧」已定位 `kRealtimePitchCacheFrames`。（`rvc/include/rvc/realtime_geometry.hpp:166-173`、`rvc/src/rvc/feature_extractor.cpp:525-528`、`rvc/src/rvc/inferencer.cpp:40`）
- [x] 板端多采样率与换模型冒烟（2026-11-17 实测，`tools/jetson-e2e.sh --realtime-mm --file --preset` 全过：实时多模型 9 场景、文件链 9、预设 8）：48 kHz 实时基线帧守恒且无推理错误、overlap 定值偏离在入口被拒且不打断在跑的流、`startup_delay_ms` 可调、48 kHz↔40 kHz 跨采样率 `model use`、切回载入原模型预设、未知模型失败恢复旧模型旧参数、文件占 GPU 时回 Busy、40 kHz 模型文件链输出 48 kHz 单声道、资产树无 presets 残留。套件为 `tools/jetson-realtime-mm.sh`（需两个采样率不同的模型，`MODEL_A`/`MODEL_B` 可指定）。注意采集静音时须置 `skip_silence = false` 才真跑推理，否则块全被跳过、`blocks` 恒 0。
- [x] 32 kHz 模型接入与双链验收（asuka-v2-32k，2026-11-17 实测）：代码已具备 32k 能力（文件链几何与重采样按模型采样率通用推导，实时几何支持集含 32000），本轮补齐资产与验收闭环，未改运行时逻辑。文件链动态生成器 large 档（每帧 320 样本）与实时 front/decoder 定长资产（decoder 输出 9600 样本）导出编译，共享特征引擎以 onnx+engine 符号链接复用（链接 engine 的原因见实测报告）；登记 `[[models]]`。`--file-models` 11 模型全过（asuka 500/500 帧、整段单窗、无 ONNX 回退），`--realtime-mm` 48k↔32k 9 场景全过（32k 实时帧守恒 199=199、blocks>0）。测试补强：几何偶数 T 扫描纳入 32k 与 T=398@32k 锚点、重采样 32k→48k 用例。实测见 `reports/rvc-32k-onboarding-20261117/RESULTS.md`。
- [ ] 板端剩余未验证：延迟 P95/连续性/长跑稳定性、`model use` 切换接缝听感与音质主观评价。阻塞原因：现有套件只断言帧守恒、块计数与 `last_error`，不测延迟统计也不做听感评价。解除条件：延迟与音质按 `reports/` 测量流程单独跑。32 kHz 实时冒烟已由上条覆盖。
- 本机已实测（2026-11-17，overlap 锁定与验收修复后）：`tools/ci-local.sh all` 通过，native 10 个 C++ 用例全过（含 32k/40k/48k 几何对拍与流式用例），Rust 647 项、Python 工具链 7 项全过。几何对拍经两次反证（改歪资产常量、改歪旋钮默认值均转红）。
- [ ] Python 镜像单测接入检查入口：`tools/model/test_realtime_chain_params.py` 覆盖 32k/40k 几何与旋钮边界，但 `tools/ci-local.sh` 与 CTest 都未调用它，目前只能人工运行。解除条件：在检查脚本加一个 Python 单测调用。
- [x] 模型加载移出控制锁。（`rvc/control/stream_session.cpp:152,159`）
- [ ] 长文件改分块，限制内存。（FUTURE）正文已定推迟（分块流式见 FUTURE），本条同步标推迟。
- [x] 配置项必须与实际行为一致。原 LoadModel 入口预检与注册表定稿冲突已修复（见模型注册表第一节）。
- [ ] 离线推理三条路径（动态整段 / 静态整段 / 分窗）各自重复 `apply_rms_mix` → `resample` → `limit_peak`；抽公共收尾，避免改一处漏两处导致不同导出形状听感不一致。（`rvc/src/rvc/inferencer.cpp:280-284,318-322,426-430`）
- [ ] 多说话人模型缺 per-job 选说话人：`spk_id` 只从 config 读，202 个说话人只用到 0。需每任务参数（与 SetParams 协同）。（`rvc/src/rvc/inferencer.cpp:800-801`）
- [ ] 小项：`infer_window` 无条件复制 feats，protect 关闭时白复制（`rvc/src/rvc/inferencer.cpp:703`）；`apply_rms_mix` 逐样本 `std::pow`，可在 RMS 网格算再插值（`rvc/src/rvc/inferencer.cpp:924-930`）。
