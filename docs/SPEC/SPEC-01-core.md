# core：契约与传输

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。
`[x]` 条目详情已于 2026-11-24 移入 [archive/SPEC-01-core.md](archive/SPEC-01-core.md)，本文件保留契约、关键决策摘要与活跃待办。

## 职责

帧格式、环形缓冲、本机传输。

## 已确认决策（摘要）

> 全文条目与证据见 [archive/SPEC-01-core.md](archive/SPEC-01-core.md)。与归档冲突时以归档原文为准并回来修正本节。

- RAW 48 kHz 采集帧契约属一致扩展，已回写「已确认」节（仅文档修正，不改行为）。

## 已确认

- 帧契约：20 ms；输入 16 kHz 单声道 f32（320 点）。输出单声道 f32，缓冲上限 960 点，采样率随模型（32/40/48 kHz），有效样本数 = 采样率 / 50。（`frame.h:26-38`、`core/src/lib.rs:11-18`）
- 进程内另有 48 kHz RAW 采集帧（960 点），不跨进程边界。（`frame.h:54-62`、`core/src/lib.rs:71-77`）
- 环形队列：满时丢最旧并计数，空时消费者等待。（`core/src/ring.rs:44-84`）
- [ ] 环形队列日志限频：满丢最旧时无日志，仅 `dropped` 计数。（`core/src/ring.rs:93-95`，依赖中亦无日志库）机制：实时热路径不打日志（禁分配与阻塞），由持有方非热路径线程（如 GetStatus 处理侧）发现计数增量时限频记录，带时间与增量。
- 数据面：Unix `SOCK_DGRAM`，一帧一条消息；控制面：Unix `SOCK_SEQPACKET`，定长头 + JSON 体。（`core/src/dgram.rs:136-147`、`core/src/ctl.rs:142-207`）
- 控制信封：16 B 小端定长头（magic `'MCTL'`、version、type、request_id、body_len），体为 JSON，上限 64 KiB；命令号按平面分段：daemon `0x01xx`、pre `0x02xx`、rvc `0x03xx`、asr `0x04xx`、bridge `0x05xx`、tts `0x06xx`。（`core/src/ctl.rs:20,142-207,678-696`）
- 响应契约统一：响应 type 回显、体固定 `status` / `code` / `reason` / `data`。TTS 任务应答已对齐：type 回显请求号（SUBMIT→0x0610、STOP→0x0612），`utterance_id` 放 `data`，受理三态映射 ok/Ok（submitted）、error/Busy（队满 skipped）、error（failed）。（`tts/src/jobs.rs`、`bridge/src/speech.rs`）
- 识别事件流（识别服务 → 字幕桥）：Unix `SOCK_SEQPACKET`，一条消息一个 JSON 事件，无定长头；socket 默认 `asr-events.sock`（run_dir 下）；事件契约见 SPEC-05。（`core/src/ctl.rs:426-462`、`core/src/config.rs:207,464-466`）
- 线包：4 B 魔数 + 16 B 元数据 + PCM。输入 1300 B；输出 3868 B，元数据后插两个 u32 字段。（`core/include/mozart/frame.h:86-91`、`core/src/lib.rs:83-84` 编译期断言）
- 输出帧新增字段：`sample_rate` 为模型原生采样率（32000 / 40000 / 48000），`valid_samples` 为 pcm 有效样本数（采样率 / 50，即 640 / 800 / 960），其后 pcm 为补零。结构体内偏移 16 与 20，线包偏移 20 与 24（前置 4 B 魔数）。（`frame.h:74-84`、`core/src/lib.rs:94-97`）
- 帧契约以 C 头文件为单一来源；Rust 用 `#[repr(C)]` 镜像，加编译期大小与偏移断言。（`core/src/lib.rs:79-99`）
- 帧契约偏移断言：`offset_of!` 逐字段钉住元数据、输入帧、输出帧与 RAW 帧；控制头同口径。（`core/src/lib.rs:86-99`、`core/src/ctl.rs:169-174`）
- 新旧进程混跑守卫：数据面无版本协商。接收长度不等于契约线包长即丢帧，热路径只加原子计数（Rust `recv_size_mismatch`、C++ `recv_size_mismatch_`）。控制线程首次出现时记可查询原因，文案要求同代重建 pre/rvc/tts/daemon。结果是全部丢帧，不会错帧。（`core/src/dgram.rs:145-147,391-397`、`rvc/transport/unix_dgram_stream.cpp:230-236,330-338`、`rvc/control/stream_session.cpp:443,484`、`pre/src/control.rs:75-91`）
- 双方用同一组字节夹具对拍：`tests/fixtures/frame/` 放共享二进制夹具（InputFrame、OutputFrame、控制信封各一条），两侧测试读同一文件比对字节；原硬编码字节断言保留为额外校验。`output.bin` 为 3868 B、`sample_rate` 48000、`valid_samples` 960。（`core/examples/gen_frame_fixture.rs`、`core/tests/frame_fixture.rs:82-91`、`tests/test_rvc_transport.cpp:389-437`、`tests/test_control_contract.cpp:58-84`）
- 环形队列用于进程内（采集 → DSP）；Unix 数据报用于进程间（音频 ↔ 推理）。

## 改进点

- socket 路径与地址集中配置。（`core/src/config.rs:544-614`）
- 控制协议的模式名与状态字段统一。
- 实时原生多采样率：输出帧扩为固定缓冲上限 + `sample_rate` + `valid_samples`，由模型采样率驱动；播放侧固定 48 kHz，边界重采样见 SPEC-02，主条见 SPEC-03。（`frame.h:74-84`、`core/src/lib.rs:44-49`、`rvc/src/rvc/audio_worker.cpp:137-139`）
