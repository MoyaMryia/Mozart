# core：契约与传输

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。

## 职责

帧格式、环形缓冲、本机传输。

## 已确认

- [x] 帧契约：20 ms；输入 16 kHz 单声道 f32（320 点），输出 48 kHz 单声道 f32（960 点）。（`frame.h:26-38`、`core/src/lib.rs:10-17`）
- [x] 环形队列：满时丢最旧并计数，空时消费者等待。（`core/src/ring.rs:44-84`）
- [ ] 环形队列日志限频：满丢最旧时无日志，仅 `dropped` 计数。（`core/src/ring.rs:93-95`，依赖中亦无日志库）机制：实时热路径不打日志（禁分配与阻塞），由持有方非热路径线程（如 GetStatus 处理侧）发现计数增量时限频记录，带时间与增量。
- [x] 数据面：Unix `SOCK_DGRAM`，一帧一条消息；控制面：Unix `SOCK_SEQPACKET`，定长头 + JSON 体。（`core/src/dgram.rs:136-147`、`core/src/ctl.rs:142-207`）
- [x] 控制信封：16 B 小端定长头（magic `'MCTL'`、version、type、request_id、body_len），体为 JSON，上限 64 KiB；命令号按平面分段：daemon `0x01xx`、pre `0x02xx`、rvc `0x03xx`、asr `0x04xx`、bridge `0x05xx`、tts `0x06xx`。（`core/src/ctl.rs:20,142-207,678-696`）
- [ ] 响应契约统一：响应 type 回显、体固定 `status` / `code` / `reason` / `data`。TTS 任务应答 `TTS_JOB_REPLY`（0x0611）当前不回显 type、体为 `status`/`utterance_id`/`reason`，与本条不符。（`core/include/mozart/ctl.h:96-97`、`core/src/ctl.rs:98-99`）已定：TTS 改标准信封——type 回显 0x0611，体 `status`/`code`/`reason`/`data`，`utterance_id` 放 `data`。
- [x] 识别事件流（识别服务 → 字幕桥）：Unix `SOCK_SEQPACKET`，一条消息一个 JSON 事件，无定长头；socket 默认 `asr-events.sock`（run_dir 下）；事件契约见 SPEC-05。（`core/src/ctl.rs:426-462`、`core/src/config.rs:207,464-466`）
- [x] 数据包沿用旧格式：4 字节魔数 + 16 字节元数据 + PCM；输入 1300 字节，输出 3860 字节。（`core/src/lib.rs:60-61` 编译期断言）
- [x] 帧契约以 C 头文件为单一来源；Rust 用 `#[repr(C)]` 镜像，加编译期大小断言。（`core/src/lib.rs:56-61`）
- [ ] 帧契约偏移断言：仅有 `size_of`，无字段偏移断言；对照控制头已具备。（`core/src/ctl.rs:153-157`）
- [ ] 双方用同一组字节夹具对拍：`tests/fixtures/` 无帧夹具文件；C++ 与 Rust 各自硬编码字节断言，对拍有效但不共享同一组字节。（`tests/test_rvc_transport.cpp:129-154`、`core/src/dgram.rs:565-627`）已定：`tests/fixtures/frame/` 放共享二进制夹具（InputFrame、OutputFrame、控制信封各一条），两侧测试读同一文件比对字节。
- [x] 环形队列用于进程内（采集 → DSP）；Unix 数据报用于进程间（音频 ↔ 推理）。
- [ ] （文档）代码已有 RAW 48 kHz 帧（`frame.h:54-62`、`core/src/lib.rs:51-54`），本规格未列；属一致扩展，回写。

## 改进点

- [x] socket 路径与地址集中配置。（`core/src/config.rs:436-486`）
- [x] 控制协议的模式名与状态字段统一。
