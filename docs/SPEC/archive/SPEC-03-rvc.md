# SPEC-03 归档（rvc）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-03-rvc.md](../SPEC-03-rvc.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 全量归档

范围：「已确认」3 项 `[x]`（进程形态决策 2 项、板端小缓冲实测 1 项）、「改进点」12 项 `[x]`（多采样率、换模型、几何单一来源、SetParams、板端冒烟等实现与验收记录）。契约类 `[x]`（边界、接口约束、参数边界、控制消息、模型注册表）去勾选后保留在源文件，不归档；标注 FUTURE 的 `filter_radius` 条按待办记法逐字留在源文件。链接已按归档目录调整深度。

### 已确认

- [x] 推理核心保持独立进程。
- [x] 非推理逻辑归 Rust。
- [x] 板端实测确认小缓冲布局成立：同源 60 s 长跑 49.86 帧/s 1:1、丢弃与欠载零增长，rvc 端到端净滞后约 0.5 s（400 ms 启动缓冲为主）。维持启动缓冲留 C++、音频进程小缓冲的现状；数据见 `reports/realtime-pipeline-20261112/RESULTS.md`。

### 改进点

- [x] 实时原生多采样率（内部动态采样率）：实时链路内部按模型原生采样率工作（32k/40k/48k）。窗、hop、交叉淡化、SOLA、启动缓冲与输出帧长都由采样率推导，40 kHz 模型不再逐块重采样。播放侧不协商采样率：固定 48 kHz，由 pre 在播放边界重采样（SPEC-02）。帧契约扩 `sample_rate`/`valid_samples`（SPEC-01）。（`rvc/include/rvc/realtime_geometry.hpp:119-201`、`rvc/src/rvc/audio_worker.cpp:64-73,137-139`、`rvc/src/rvc/streaming_pipeline.cpp:31-42`）板端验收见下方板端条目。
- [x] 采样率生态与原生多采样率方案调研：公开资料调研 RVC 常见采样率（32k/40k/48k）、上游实时推理假设、导出与 front/decoder 形状、ALSA/USB 播放采样率约束，结论与实现边界入 `reports/realtime-sample-rate-20261116/RESULTS.md`。
- [x] 实时流中换模型（推理侧）：`LoadModel` 支持活动流期间原子换入 inferencer，空闲切换先释放旧 inferencer 防 OOM。（`rvc/src/rvc/pipeline.cpp:142-160`、`rvc/control/stream_session.cpp:519-540`）实时换模型定为断流切换（SPEC-04 `model use`）：`StartStream` 重建 `StreamingRvc` 并 `reset_realtime`，过去上下文、SOLA 状态与音高缓存不跨模型保留，跨采样率按新采样率重建几何。（`rvc/src/rvc/audio_worker.cpp:62-79`）板端接缝与帧守恒见下方板端条目。
- [x] 文件链路多模型：静态定长、静态整段与动态生成器（40k/48k）共 10 个模型板端完成，输出按输入时长统一收口，`frames_in == frames_out`；空闲模型切换先释放旧 inferencer 并腾出缓存槽位，避免旧模型显存未释放时触发 TensorRT OOM 回退 ONNX；活动流期间保持旧模型直到新模型换入。（`rvc/src/rvc/inferencer.cpp:243-442`、`rvc/src/rvc/pipeline.cpp:136-151`、`rvc/src/rvc/model_loader.cpp:307-342`、`reports/multi-model-file-20261115/RESULTS.md`）
- [x] 实时几何单一来源落地：`realtime_chain_params.json` 由 `tools/model/realtime_chain_params.py` 读取，导出与编译脚本取默认值并校验形状，C++ 门控与常量镜像在 `realtime_geometry.hpp`（见实时几何与资产参数节）。（`tools/model/export_generator_split_onnx.py`、`export_hubert_onnx.py`、`export_rmvpe_onnx.py`、`build_realtime_engines.py`、`tests/test_rvc_realtime_params.cpp`）
- [x] `[rvc.realtime]` 段：`core::config` 四键 + 范围、整除与 overlap 定值校验 + schema + `StartStream` 携带 + SPEC-04 `config set` Stop/Start 判据纳入本段键。（`core/src/config.rs:140-158,735-765`、`rvc/control/stream_session.cpp:283-327`、`daemon/src/services.rs:189-192,957-965`）
- [x] RMVPE 逐块帧推导复核：帧数由 `derive_realtime_geometry` 推导并进门控，补整引用同一常量；架构文档「音高缓存 1024 帧」已定位 `kRealtimePitchCacheFrames`。（`rvc/include/rvc/realtime_geometry.hpp:166-173`、`rvc/src/rvc/feature_extractor.cpp:525-528`、`rvc/src/rvc/inferencer.cpp:40`）
- [x] 板端多采样率与换模型冒烟（2026-11-17 实测，`tools/jetson-e2e.sh --realtime-mm --file --preset` 全过：实时多模型 9 场景、文件链 9、预设 8）：48 kHz 实时基线帧守恒且无推理错误、overlap 定值偏离在入口被拒且不打断在跑的流、`startup_delay_ms` 可调、48 kHz↔40 kHz 跨采样率 `model use`、切回载入原模型预设、未知模型失败恢复旧模型旧参数、文件占 GPU 时回 Busy、40 kHz 模型文件链输出 48 kHz 单声道、资产树无 presets 残留。套件为 `tools/jetson-realtime-mm.sh`（需两个采样率不同的模型，`MODEL_A`/`MODEL_B` 可指定）。注意采集静音时须置 `skip_silence = false` 才真跑推理，否则块全被跳过、`blocks` 恒 0。
- [x] 32 kHz 模型接入与双链验收（asuka-v2-32k，2026-11-17 实测）：代码已具备 32k 能力（文件链几何与重采样按模型采样率通用推导，实时几何支持集含 32000），本轮补齐资产与验收闭环，未改运行时逻辑。文件链动态生成器 large 档（每帧 320 样本）与实时 front/decoder 定长资产（decoder 输出 9600 样本）导出编译，共享特征引擎以 onnx+engine 符号链接复用（链接 engine 的原因见实测报告）；登记 `[[models]]`。`--file-models` 11 模型全过（asuka 500/500 帧、整段单窗、无 ONNX 回退），`--realtime-mm` 48k↔32k 9 场景全过（32k 实时帧守恒 199=199、blocks>0）。测试补强：几何偶数 T 扫描纳入 32k 与 T=398@32k 锚点、重采样 32k→48k 用例。实测见 `reports/rvc-32k-onboarding-20261117/RESULTS.md`。
- [x] 在线参数调整（SetParams）：新增 `MOZART_CTL_RVC_SET_PARAMS`（`0x0308`），`RVCInferencer` 在块边界取参数快照、`StreamingRvc` / `AudioWorker` 原子更新 `skip_silence`，不重建推理器与流缓冲。（`rvc/control/stream_session.cpp`、`rvc/src/rvc/inferencer.cpp`、`rvc/src/rvc/streaming_pipeline.cpp`）
- [x] 模型加载移出控制锁。（`rvc/control/stream_session.cpp:152,159`）
- [x] 配置项必须与实际行为一致。原 LoadModel 入口预检与注册表定稿冲突已修复（见模型注册表第一节）。