# 语言链路实现计划

SPEC-05 全落地：ASR + TTS + 字幕桥 + 翻译 + 播报投递 + daemon 监管 + pre 音频接入。

## Global Constraints

- 按 SPEC-05（docs/SPEC/SPEC-05-language.md）和 SPEC-04 语言服务条款实现，不虚捏。
- C 头 core/include/mozart/ 是跨语言契约唯一定义源，改头必须同步 Rust 镜像 + 编译期断言。
- 子进程 CLI 契约：`--socket <path>` + `--config <path>`（supervisor 传入）。
- 控制面协议：16B LE 信封 + JSON body，SEQPACKET，命令号 ASR 0x04xx / TTS 0x05xx / Bridge 0x06xx。
- 数据面：InputFrame 1300B（16 kHz/320 f32），WirePacket trait，receiver binds / sender connects。
- sherpa-onnx 链接用 SHERPA_ONNX_LIB_DIR 环境变量控制；未设置时 build.rs 不链接，推理用 stub。本机 cargo check 可过，完整构建测试在 Jetson。
- 模型路径（Jetson）：ASR ~/models/sherpa-onnx/zipformer-zh-14M，复核 ~/models/sherpa-onnx/clone-test-downloads/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17，TTS ~/models/sherpa-onnx/clone-test-downloads/sherpa-onnx-pocket-tts-int8-2026-01-26，翻译 ~/mozart-archive/qwen35/gguf/qwen35-0.8b-text-Q4_K_M.gguf。
- TTS 原生 24 kHz，进程内重采样到 16 kHz 契约帧。TTS→RVC 喂入机制（资源仲裁）留 FUTURE，v1 TTS 输出到 tts-out.sock 待接。
- llama-server 由 daemon 只管进程存活，不说控制协议；桥经 Unix socket HTTP 调 /v1/chat/completions。
- 提交前跑 tools/ci-local.sh --staged，不用 --no-verify。commit 中文前缀。
- 不碰用户未提交的 docs/SPEC/SPEC-03-rvc.md、SPEC-04-daemon.md、SPEC.md。

## Task 1: Core 契约扩展

扩展 core crate 为语言链路提供类型基础。

文件：core/include/mozart/ctl.h, core/src/ctl.rs, core/src/ctl/schema.rs, core/src/config.rs, core/src/lib.rs, Cargo.toml

内容：
1. ctl.h + ctl.rs 新增命令号：ASR_HELLO 0x0401, ASR_GET_STATUS 0x0402; TTS_HELLO 0x0501, TTS_GET_STATUS 0x0502; BRIDGE_HELLO 0x0601, BRIDGE_GET_STATUS 0x0602, BRIDGE_TRANSLATE_SET 0x0603, BRIDGE_SPEECH_SET 0x0604, BRIDGE_SPEECH_STOP 0x0605。
2. schema.rs：AsrStatus（SPEC-05 状态字段：收包数、拒收数、环形队列丢帧数、输出缓冲丢句数、final 数、复核 skipped/failed 数、最后收包时间、桥连接状态、启动号）；TtsStatus（合成成功/失败数、是否合成中、模型加载状态、任务队列深度、最近错误）；BridgeStatus（四组：翻译、播报、字幕、降级原因）。Processes 加 asr/tts/bridge 字段。Status 加 asr/tts/bridge Option。
3. config.rs：AsrConfig（socket, input_socket, events_socket, model_dir, review_model_dir, threads, ring_capacity, output_queue_capacity）、TtsConfig（socket, jobs_socket, output_socket, model_dir, reference_wav, threads, provider）、BridgeConfig（socket, subtitles_path, translate_enabled, speech_enabled, llama_socket, llama_model, translate_queue_capacity, speech_queue_capacity）。ProcessConfig 加 asr_bin/tts_bin/bridge_bin/llama_bin。Config 加 asr/tts/bridge 段。socket getters、env overrides、validate。
4. Cargo.toml workspace members 加 "asr", "tts", "bridge"。
5. 各新 crate 最小 Cargo.toml + src/main.rs 空壳（fn main 解析 --socket/--config 后退出），确保 workspace cargo check 通过。

验收：cargo check --workspace 通过；新命令号/类型/配置有单元测试。

## Task 2: ASR 进程

新建 asr crate 完整实现。

文件：asr/src/{main,control,receiver,decoder,review,output,ffi}.rs, asr/build.rs

内容：
1. main：解析 --socket/--config，Config::load 读 [asr]，生成随机启动号，启动各线程，SIGTERM 收尾（喂 0.3s 静音、复核限时排空、剩余标 skipped）。
2. receiver 线程：FrameSocket::<InputFrame>::bind(input_socket, ring_capacity)，收包不分配不阻塞，push 到 SPSC Ring。统计收包数/拒收数。1s 空闲兜底切句。
3. decoder 线程：从 Ring pop，喂 sherpa-onnx OnlineRecognizer（FFI），partial/final 事件交付 output。句音频缓冲 700 帧上限，触发即进程退出。endpoint 检测参数沿用旧实现（rule1=1.2, rule2=0.6, rule3=12.0）。
4. review 线程：独立队列容量 2，整句音频所有权移交。OfflineRecognizer SenseVoice 复核。队满标 skipped 发事件。模型加载失败不影响在线识别，每句报 failed。
5. output 线程：独占 asr-events.sock SEQPACKET 连接（桥连接、ASR 监听）。发送 partial/final/refined JSON 事件（含启动号）。断连时 final/refined 存有界队列（output_queue_capacity），重连补发；partial 丢。超限丢最旧计数。
6. control 线程：asr.sock 绑定，Hello/GetStatus。只读原子计数。
7. ffi.rs：sherpa-onnx C API 声明（OnlineRecognizer transducer + OfflineRecognizer sense_voice）。build.rs 用 SHERPA_ONNX_LIB_DIR 链接；未设置时编译 stub（返回空结果）。
8. 状态计数全部 AtomicU64/AtomicBool，control 只读。

验收：cargo check 通过；stub 模式下单元测试覆盖线程交接、队列满丢弃、事件格式、SIGTERM 收尾。

## Task 3: TTS 进程

新建 tts crate 完整实现。

文件：tts/src/{main,control,jobs,synth,resample,output,ffi}.rs, tts/build.rs

内容：
1. main：--socket/--config，Config::load 读 [tts]，启动线程。
2. control：tts.sock，Hello/GetStatus（合成成功/失败数、是否合成中、模型加载状态、任务队列深度、最近错误）。
3. jobs 线程：tts-jobs.sock 绑定（定长头 + JSON 自有协议），接收桥发来的译文 + utterance_id + 音色参数。回受理状态 submitted/skipped/failed。有界任务队列。
4. synth：sherpa-onnx OfflineTts PocketTTS FFI。reference_audio 从配置 WAV 加载。GenerationConfig：num_steps=5, speed=1, seed=42。文本预处理（标点归一化）。
5. resample：24 kHz → 16 kHz 线性插值，逐块纯算术。输出 InputFrame（320 samples/帧）。
6. output：FrameSocket::<InputFrame> connect 到 tts-out.sock（receiver binds 留 FUTURE 接 RVC）。非阻塞发送 + 丢弃计数。
7. ffi.rs + build.rs：同 ASR 模式，SHERPA_ONNX_LIB_DIR 控制链接。
8. SIGTERM：放弃在途合成，退出。

验收：cargo check 通过；resample 单元测试（已知输入→期望输出）；jobs 协议解析测试；stub 模式集成测试。

## Task 4: 字幕桥核心

新建 bridge crate：控制面 + ASR 事件接收 + 字幕落盘 + 开关。

文件：bridge/src/{main,control,events,subtitles,state}.rs

内容：
1. main：--socket/--config，Config::load 读 [bridge]，启动线程。
2. events 线程：连接 asr-events.sock（SEQPACKET），读循环只入队不做慢活。解析 partial/final/refined JSON。启动号校验。
3. subtitles：内存记录表（utterance_id → record），追加写 subtitles.jsonl（XDG 路径 ~/.local/state/mozart/subtitles.jsonl）。revision 覆盖语义沿用旧实现。逐行 flush。
4. control：bridge.sock，Hello/GetStatus（四组字段）+ translate on|off / speech on|off / speech stop。开关改桥内临时状态 + 写 daemon 运行时状态文件。
5. state：开关持久性（translate_enabled/speech_enabled 基线从 TOML，运行时命令覆盖，重启从状态文件恢复）。
6. SIGTERM：停收新事件，放弃在途，丢弃队列，2s 内退出。

验收：cargo check 通过；字幕记录表合并/revision 测试；事件解析测试；开关命令测试。

## Task 5: 桥翻译与播报

桥内翻译调度 + 播报投递。

文件：bridge/src/{translate,preprocess,quality,speech,http}.rs

内容：
1. http：Unix socket HTTP 客户端（llama-server --host .sock），POST /v1/chat/completions，max_tokens=128, temperature=0。连接状态跟踪。
2. translate 线程：从待翻译队列取 final 句，预处理→推理→质检→落字幕→投递。失败重试最多 2 次。translate_enabled=false 时只发布原文不调 llama。
3. preprocess：同音字纠错（显式表 + 术语同音索引）、数字归一化。v1 先搬显式纠错表，拼音匹配留接口。
4. quality：基础质检（空译文、长度异常、重复检测）。拒绝计数。
5. speech 线程：待播报有界队列（speech_queue_capacity），队满丢最旧计数。连接 tts-jobs.sock 发译文 + utterance_id。speech_enabled=false 停新投递；speech stop 清队列。
6. GetStatus 翻译组（队列深度、已翻译/失败/重试/质检拒绝数、llama 连接状态、最近成功失败时间原因）+ 播报组（队列深度、已投递/队满丢弃数、TTS 连接状态、submitted/skipped/failed）。

验收：cargo check 通过；HTTP 客户端 mock 测试；重试逻辑测试；队列满丢弃测试；开关行为测试。

## Task 6: Daemon 监管集成

daemon 监管 asr/tts/bridge/llama-server + 轮询 + CLI。

文件：daemon/src/{state,supervisor,services,server,poller}.rs, ctl/src/main.rs

内容：
1. state.rs：Role 加 Asr/Tts/Bridge/Llama。Processes 加四字段。State 加 asr/tts/bridge Option status。process_mut 扩展。
2. supervisor.rs：role_name/bin/socket 匹配扩展。llama-server 特殊：bin 从 config.process.llama_bin，启动参数 -m <model> -c 2048 -ngl 99 --host <llama.sock> --jinja（无 --socket/--config 契约）。
3. services.rs：Services 加 asr/tts/bridge ServiceLink。poll_asr/poll_tts/poll_bridge。llama 不轮询（无控制协议）。
4. server.rs：supervisor 数组扩展为 6 个（pre/rvc/asr/tts/bridge/llama）。
5. ctl：status 输出加 asr/tts/bridge 进程行 + 状态块。translate/speech 命令经 daemon 转发到 bridge（DAEMON_TRANSLATE_SET 0x0106 / DAEMON_SPEECH_SET 0x0107 / DAEMON_SPEECH_STOP 0x0108）。
6. daemon 转发：收到 translate/speech 命令 → services.bridge.call_ok(BRIDGE_TRANSLATE_SET/...) → 回 ok/reject。

验收：cargo check 通过；daemon 集成测试（stub 子进程）验证监管/轮询/转发；CLI 命令解析测试。

## Task 7: Pre 音频接入 ASR

pre 进程增加 ASR 音频分发。

文件：pre/src/{main,ai}.rs, core/src/config.rs（如需）

内容：
1. ai.rs 新增 AsrSender（同 Sender 模式）：lazy connect 到 config.asr_input_socket()，FrameSocket::<InputFrame>，非阻塞发送 + 丢弃计数。
2. main.rs process_thread：在 ai_sender.send 之后（或并行），asr_sender.send 同一 InputFrame。AI path never stops 原则不变。
3. Shared 加 asr_send_dropped 计数。PreStatus 加 consumer_drops["asr"]。
4. ASR 未启动时 connect 失败 → 计数丢弃，不阻塞 pre。

验收：cargo check 通过；现有 pre 测试不回归；AsrSender 单元测试（connect 失败丢弃、正常发送）。

## Task 8: sherpa-onnx 构建 + Jetson 集成

Jetson 上构建 sherpa-onnx C 库，全链路部署测试。

前置：Jetson 可达（当前 10.0.0.4 超时，需用户确认地址）。

内容：
1. Jetson 上 clone sherpa-onnx v1.13.6，cmake 构建 C API 共享库（-DSHERPA_ONNX_ENABLE_C_API=ON -DBUILD_SHARED_LIBS=ON -DCMAKE_BUILD_TYPE=Release）。
2. 设置 SHERPA_ONNX_LIB_DIR，cargo build --workspace 全量。
3. 配置 mozart.toml 加 [asr]/[tts]/[bridge] 段指向实际模型路径。
4. deploy-jetson.sh --smoke 部署。
5. 冒烟：daemon up → asr/tts/bridge/llama 全部 Running → pre 采集 → ASR 出 partial/final → 桥落字幕 → 翻译 → TTS 合成。
6. 修复集成问题。

验收：全进程 Running；ASR 事件可收；字幕文件有记录；TTS 合成成功计数 > 0。