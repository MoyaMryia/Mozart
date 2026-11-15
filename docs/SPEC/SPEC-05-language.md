# language：语言服务

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。

## 方向

旧实现的 Python 服务全部用 Rust 重写，Python 不进入运行时。

- [x] Rust：识别编排、字幕桥、翻译调度、质量检查、翻译播报调度、投递队列、变速、启动监督。
- [x] C++：识别与合成的模型推理，经 sherpa-onnx 的 C/C++ 接口调用；翻译模型仍作为独立进程。
- [x] 翻译播报 v1 继承旧参考音色播报：TTS 合成后直接播放，不走变声；扬声器分时沿用旧实现（播报时静音实时，播完恢复）。经变声链路输出、用同一目标音色推迟，见 FUTURE。

## 已确认

### 识别服务

- [x] 识别服务为 Rust 单进程；sherpa-onnx 以库形式链入，经其 C API 调用。复核模型用同进程第二个识别器实例。（`asr/src/`）
- [x] 识别服务内分收包线程与解码线程，中间用 core 环形队列交接；收包线程不分配内存、不阻塞。环形队列容量为新增参数，设备上实测后定。
- [x] 复核独立线程，队列容量 2；整句音频所有权随提交移交复核线程。队满标 skipped 并发事件，不阻塞解码。（`asr/src/review.rs`）
- [x] 句音频缓冲上限 700 帧（14 s）兜底保留，触发即进程退出、由 daemon 重启，不改为强制切句。复核可选；模型加载失败不影响在线识别，每句报 failed 带原因，状态可查。
- [x] 识别服务独立进程，事件经 Unix SEQPACKET 发给字幕桥：识别服务监听，桥连接，一条消息一个 JSON 事件；事件 socket 默认 `asr-events.sock`（run_dir 下）。partial/final/refined 三种类型与现有字段沿用。桥的读循环只入队，不做慢活。（`asr/src/event.rs`、`bridge/src/events.rs`）
- [x] 事件新增识别启动号字段：识别服务每次启动生成一个随机编号，三种事件都携带；现有字段不变。（`asr/src/event.rs`）
- [x] 输出侧由独立输出线程独占事件连接：发送、重连、暂存补发；解码与复核线程只交付事件，不碰 socket。（`asr/src/output.rs`）
- [x] 桥断开期间 final/refined 存进有界内存队列，重连后按序补发；partial 直接丢。队列上限为新增参数，按几十句量级，设备上实测后定；超限丢最旧并计数。（`bridge/tests/process.rs:320`）
- [x] 生命周期归 daemon 监管，跟随 SPEC-04。SIGTERM 退出沿用旧行为：喂 0.3 s 静音切出最后 final，复核队列限时排空、剩余标 skipped。pre 断流由 1 s 空闲兜底切句；识别服务重启重新绑定输入 socket，损失进行中的半句。
- [x] 状态内容：收包数、拒收数、环形队列丢帧数、输出缓冲丢句数、final 数、复核 skipped/failed 数、最后收包时间、桥连接状态。经控制 socket 的 `GetStatus` 上报。（`asr/src/state.rs`）
- [x] 控制面：绑定 `asr.sock`（run_dir 下），daemon 连接；最小命令 `Hello` / `GetStatus`，信封见 SPEC-01。控制独立线程，只读原子计数，不碰解码热路径。配置读 `[asr]` 段，socket 路径相对 run_dir。（命令号 0x04xx）

### 字幕桥

- [x] 字幕桥为单个 Rust 进程：收字、写字幕、翻译调度、播报投递都在进程内，分线程执行。字幕记录表是进程内共享状态，不跨进程同步；旧桥拉起识别子进程的职责归 daemon。（`bridge/src/main.rs`）
- [x] 字幕发布沿用文件：桥追加写 subtitles.jsonl，文件是发布点与存档，记录格式与 revision 覆盖语义沿用；内存记录表管修订合并。桥不对网页提供连接，推流方读文件，留到前端模块定。（`bridge/src/subtitles.rs`）
- [x] 字幕文件放持久本地存储，重启保留，不放 tmpfs；默认 `~/.local/state/mozart/subtitles.jsonl`（XDG 状态目录，普通用户可写），经 `[bridge]` 的 `subtitles_path` 配置。旧环境变量 `MOZART_SUBTITLES_JSONL` 废弃。v1 不轮转，留实测后定。（`core/src/config.rs`）
- [x] 字幕编号 utterance_id 用识别启动号:句号，不用桥会话号；识别或桥单独重启都不撞号，落盘队列里的老编号也能对上。
- [x] 控制面：绑定 `bridge.sock`（run_dir 下），daemon 连接；命令 `Hello` / `GetStatus` 加字幕开关命令，信封见 SPEC-01。配置读 `[bridge]` 段。（命令号 0x05xx）
- [x] 字幕链路命令（v1）：`translate on|off`、`speech on|off`、`speech stop`，经 daemon 转发到桥执行。`translate off` 桥只发布原文字幕、不调 llama-server；`speech off` 桥停新投递、在途自然播完；`speech stop` 关播报并清空待播报队列（在途丢弃）；三者都不影响字幕落盘。`glossary` 编辑与 `subtitle clear` 缓（数据管理留前端适配器，clear 破坏存档）；识别开关不做，停识别用 daemon 停进程。（`bridge/src/control.rs`、`bridge/src/speech.rs`）
- [x] 开关持久性（生效语义）：`[bridge]` 的 `translate_enabled` / `speech_enabled` 是 TOML 基线；运行时命令改桥内临时开关，重启恢复、不回落基线。桥是开关的活权威。运行时开关命令（translate/speech）与 `config set` 改 TOML 基线、`config save` 批量提升分开（见 SPEC-04）。（`bridge/src/state.rs`）
- [ ] 开关持久性（存档归属）：改动记入 daemon 的运行时状态文件（见 SPEC-04）。已定：daemon 统一管，桥不再自写 `bridge-state.json`，改为经 daemon 写状态文件。未实现（daemon 状态文件与桥迁移均待做）。
- [x] 桥 `GetStatus` 字段分四组：翻译（`translate_enabled`、待翻译队列深度、已翻译/失败/重试/质检拒绝数、llama-server 连接状态、最近成功失败时间与原因）；播报（`speech_enabled`、待播报队列深度、已投递/队满丢弃数、TTS 连接状态、受理计数 submitted/skipped/failed）；字幕（已发布句数、记录表条数、文件路径、最近写入时间）；降级原因列表（可查询，带原因与时间）。（`bridge/src/state.rs`）

- [x] 桥 SIGTERM 收尾：停止收新事件，放弃在途翻译/播报请求，丢弃在途队列后退出；字幕存档逐行 flush 已落盘，不受影响。退出须在 daemon 的 2 s 宽限内完成。（`bridge/tests/process.rs`）

### 状态归属

- [x] 每块状态只有一个主人，命令到主人那里改；daemon 的整机 `status` 是轮询来的派生读模型，不是第二份权威。跨进程交换状态只有两条路：`GetStatus` 拉只读快照、字幕文件发布；无跨进程共享可变状态。
- [x] 桥：开关、待翻译/待播报队列、翻译/投递计数、llama-server 与 TTS 连接状态、字幕记录表与文件。识别服务：识别计数、复核状态、桥连接状态。TTS：合成计数、模型加载状态、任务队列。daemon：生命周期、资源仲裁、整机汇总。

### 翻译

- [x] 翻译模型用 llama-server（llama.cpp 的 C++ 服务程序）独立进程，不链库进 Rust，以隔离崩溃域、解耦构建；模型与启动参数沿用旧实现（演示固定 Qwen3.5 0.8B）。llama-server 由 daemon 监管，跟随 SPEC-04。（`daemon/src/supervisor.rs:256-257`）
- [x] 传输用 Unix socket（llama-server 的 --host 填 .sock 路径），协议沿用其原生 HTTP（OpenAI /v1/chat/completions）。这是系统唯一保留 HTTP 协议之处：不重写第三方推理服务，传输层仍统一 Unix socket。板端需确认 llama-server 支持 --host .sock，不支持则重建 llama.cpp 或暂退 TCP 本机回环。（板端确认仍未做，保留退路）
- [x] Rust 桥当 HTTP 客户端（经 Unix socket 连接），负责推理之外的全部编排：预处理（同音字纠正、术语表、数字归一化）、质检、失败重试（最多 2 次）、上下文管理、落字幕、投递。llama-server 只做中文→英文纯推理。（`bridge/src/http.rs`、`bridge/src/translate.rs`、`bridge/src/quality.rs`）
- [x] 同音字纠错（显式表）：显式纠错表搬成 Rust，有行为测试。（`bridge/src/preprocess.rs:13-26`、`bridge/tests/process.rs:395-427`）
- [ ] 同音字纠错（术语同音索引，拼音匹配、词频过滤）：代码标注留 FUTURE（`bridge/src/preprocess.rs:2-4`），v1 只显式表替换。已定：保留为待实现，不降级。
- [ ] 同音字纠错与旧实现逐条对拍：对拍证据在仓外 Mozart-Old（`bridge/src/checks.rs:1-4` 自述对拍旧 `test_translation_checks.py`），对拍夹具未入本仓 `tests/fixtures/`。
- [x] 第一版整句 final 才翻译；提前断句（stable_clauses）与可选断句模型推迟到第一版之后，见 FUTURE。

### 播报投递

- [x] 投递队列为有界内存队列，不落盘；队满丢最旧并计数，容量为新增参数、板端实测后定。桥里在途队列（待翻译、待播报）均放内存，落盘的是字幕存档与运行时状态文件（见 SPEC-04）。桥崩溃重启丢失在途播报，不补播旧句；译文存档不受影响（见字幕桥，先于投递写入 subtitles.jsonl）。（`bridge/tests/process.rs:572`）
- [ ] 播报 TTS 为独立 Rust 进程，链 sherpa-onnx 的 TTS 库（C/C++ 接口），不链进桥，以隔离崩溃域、解耦构建；由 daemon 监管，跟随 SPEC-04。桥把合格译文发给 TTS 进程，TTS 用参考音色合成，流式喂给 pre 播放（合成一段播一段，沿用旧低延迟路径），不走变声。播报经变声用同一目标音色见 FUTURE。进程形态与不走变声已落地；「流式直播、合成一段播一段」未实现——现为整句合成完成后按 20 ms 节拍逐帧发送（`tts/src/synth.rs:249-259`、`tts/src/ffi.rs:327`，进度回调仅用于中止）。另（文档）`tts/src/output.rs:3` 注释与实现同样不符。
- [x] 播报音频采样率：v1 不走变声，TTS 合成后重采样到播放采样率直接喂 pre 播；经变声输出（FUTURE）时在 TTS 进程内重采样到 16 kHz 契约帧喂 RVC，RVC 输入统一 16 kHz。重采样为纯算术、逐块进行，延迟可忽略；算法沿用旧实现。（v1 路径 24k→48k；16 kHz 分支为 FUTURE 死代码 `tts/src/resample.rs:32-33`）
- [x] 桥到 TTS 接口走 Unix socket + 自有协议（定长头 + JSON），不用 HTTP：TTS 为自有 Rust 进程，区别于第三方 llama-server。任务通道默认 `tts-jobs.sock`（run_dir 下），TTS 绑定、桥连接。命令号 0x06xx（任务 0x0610/应答 0x0611）。应答信封已定改标准格式（见 SPEC-01），待实现。
- [ ] 桥发参考音色参数：桥只发 text + utterance_id（`bridge/src/speech.rs:119-123`）；`voice_params` 仅校验不生效（`tts/src/jobs.rs:6`），音色走配置 reference_wav。注：按句切换音色取决于 TTS 模型能力（当前 sherpa-onnx 固定参考音频），非纯接线问题；v1 单音色不受影响，通道预留待模型支持时接通。
- [x] TTS 控制面：绑定 `tts.sock`（run_dir 下），daemon 连接；命令 `Hello` / `GetStatus`，信封见 SPEC-01。`GetStatus` 带合成成功/失败数、是否合成中、模型加载状态、任务队列深度、最近错误。配置读 `[tts]` 段，socket 路径相对 run_dir。（`tts/src/control.rs`、`tts/src/state.rs`）
- [x] 第一版播报原速播放；灵活变速（按时长比例与积压反馈选 1.0–1.5 倍速、保留音高）推迟见 FUTURE。拉伸 DSP 用现成 C 库 libmozart-tempo，控制逻辑与时长模型待启用时搬 Rust。

## 待讨论

- 无。glossary 编辑与 subtitle clear 留前端模块（见 SPEC-06）。
