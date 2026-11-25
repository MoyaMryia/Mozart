# language：语言服务

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。
`[x]` 条目详情已于 2026-11-24 移入 [archive/SPEC-05-language.md](archive/SPEC-05-language.md)，本文件保留契约、关键决策摘要与活跃待办。

## 方向

旧实现的 Python 服务全部用 Rust 重写，Python 不进入运行时。

- Rust：识别编排、字幕桥、翻译调度、质量检查、翻译播报调度、投递队列、变速、启动监督。
- C++：识别与合成的模型推理，经 sherpa-onnx 的 C/C++ 接口调用；翻译模型仍作为独立进程。
- 翻译播报 v1 继承旧参考音色播报：TTS 合成后直接播放，不走变声；扬声器分时沿用旧实现（播报时静音实时，播完恢复）。经变声链路输出、用同一目标音色推迟，见 FUTURE。

## 已确认决策（摘要）

> 全文条目与证据见 [archive/SPEC-05-language.md](archive/SPEC-05-language.md)。与归档冲突时以归档原文为准并回来修正本节。

- 同音字纠错与旧实现逐条对拍，夹具 `tests/fixtures/preprocess/homophones.json`；质检对拍由 `bridge/src/checks.rs` 内联测试覆盖。
- 重试翻译的进程测试在原 5 s 预算内同时等待翻译与异步播报提交，保留全部状态断言。

## 已确认

### 识别服务

- 识别服务为 Rust 单进程；sherpa-onnx 以库形式链入，经其 C API 调用。复核模型用同进程第二个识别器实例。（`asr/src/`）
- 识别服务内分收包线程与解码线程，中间用 core 环形队列交接；收包线程不分配内存、不阻塞。环形队列容量为新增参数，设备上实测后定。
- 复核独立线程，队列容量 2；整句音频所有权随提交移交复核线程。队满标 skipped 并发事件，不阻塞解码。（`asr/src/review.rs`）
- 句音频缓冲上限 700 帧（14 s）兜底保留，触发即进程退出、由 daemon 重启，不改为强制切句。复核可选；模型加载失败不影响在线识别，每句报 failed 带原因，状态可查。
- 识别服务独立进程，事件经 Unix SEQPACKET 发给字幕桥：识别服务监听，桥连接，一条消息一个 JSON 事件；事件 socket 默认 `asr-events.sock`（run_dir 下）。partial/final/refined 三种类型与现有字段沿用。桥的读循环只入队，不做慢活。（`asr/src/event.rs`、`bridge/src/events.rs`）
- 事件新增识别启动号字段：识别服务每次启动生成一个随机编号，三种事件都携带；现有字段不变。（`asr/src/event.rs`）
- 输出侧由独立输出线程独占事件连接：发送、重连、暂存补发；解码与复核线程只交付事件，不碰 socket。（`asr/src/output.rs`）
- 桥断开期间 final/refined 存进有界内存队列，重连后按序补发；partial 直接丢。队列上限为新增参数，按几十句量级，设备上实测后定；超限丢最旧并计数。（`bridge/tests/process.rs:320`）
- 生命周期归 daemon 监管，跟随 SPEC-04。SIGTERM 退出沿用旧行为：喂 0.3 s 静音切出最后 final，复核队列限时排空、剩余标 skipped。pre 断流由 1 s 空闲兜底切句；识别服务重启重新绑定输入 socket，损失进行中的半句。
- 状态内容：收包数、拒收数、环形队列丢帧数、输出缓冲丢句数、final 数、复核 skipped/failed 数、最后收包时间、桥连接状态。经控制 socket 的 `GetStatus` 上报。（`asr/src/state.rs`）
- 控制面：绑定 `asr.sock`（run_dir 下），daemon 连接；最小命令 `Hello` / `GetStatus`，信封见 SPEC-01。控制独立线程，只读原子计数，不碰解码热路径。配置读 `[asr]` 段，socket 路径相对 run_dir。（命令号 0x04xx）

### 字幕桥

- 字幕桥为单个 Rust 进程：收字、写字幕、翻译调度、播报投递都在进程内，分线程执行。字幕记录表是进程内共享状态，不跨进程同步；旧桥拉起识别子进程的职责归 daemon。（`bridge/src/main.rs`）
- 字幕发布沿用文件：桥追加写 subtitles.jsonl，文件是发布点与存档，记录格式与 revision 覆盖语义沿用；内存记录表管修订合并。桥不对网页提供连接，推流方读文件，留到前端模块定。（`bridge/src/subtitles.rs`）
- 字幕文件放持久本地存储，重启保留，不放 tmpfs；默认 `~/.local/state/mozart/subtitles.jsonl`（XDG 状态目录，普通用户可写），经 `[bridge]` 的 `subtitles_path` 配置。旧环境变量 `MOZART_SUBTITLES_JSONL` 废弃。v1 不轮转，留实测后定。（`core/src/config.rs`）
- 字幕编号 utterance_id 用识别启动号:句号，不用桥会话号；识别或桥单独重启都不撞号，落盘队列里的老编号也能对上。
- 控制面：绑定 `bridge.sock`（run_dir 下），daemon 连接；命令 `Hello` / `GetStatus` 加字幕开关命令，信封见 SPEC-01。配置读 `[bridge]` 段。（命令号 0x05xx）
- 字幕链路命令（v1）：`translate on|off`、`speech on|off`、`speech stop`，经 daemon 转发到桥执行。`translate off` 桥只发布原文字幕、不调 llama-server；`speech off` 桥停新投递、在途自然播完；`speech stop` 关播报并清空待播报队列（在途丢弃）；三者都不影响字幕落盘。`glossary` 编辑与 `subtitle clear` 缓（数据管理留前端适配器，clear 破坏存档）；识别开关不做，停识别用 daemon 停进程。（`bridge/src/control.rs`、`bridge/src/speech.rs`）
- 开关持久性（生效语义）：`[bridge]` 的 `translate_enabled` / `speech_enabled` 是 TOML 基线；经 daemon 转发的运行时命令改桥内临时开关并持久记录，重启恢复、不回落基线（直连 `bridge.sock` 仅改桥内存活状态，不写盘）。桥是开关的活权威。运行时开关命令（translate/speech）与 `config set` 改 TOML 基线、`config save` 批量提升分开（见 SPEC-04）。（`bridge/src/state.rs`）
- 开关持久性（存档归属）：经 daemon 转发的改动记入 daemon 的运行时状态文件 `state.json`（见 SPEC-04），桥只改内存开关并由 daemon 在启动与桥重启时恢复，不再自写 `bridge-state.json`。（`bridge/src/state.rs`、`daemon/src/poller.rs`）
- 桥 `GetStatus` 字段分四组：翻译（`translate_enabled`、待翻译队列深度、已翻译/失败/重试/质检拒绝数、llama-server 连接状态、最近成功失败时间与原因）；播报（`speech_enabled`、待播报队列深度、已投递/队满丢弃数、TTS 连接状态、受理计数 submitted/skipped/failed）；字幕（已发布句数、记录表条数、文件路径、最近写入时间）；降级原因列表（可查询，带原因与时间）。（`bridge/src/state.rs`）

- 桥 SIGTERM 收尾：停止收新事件，放弃在途翻译/播报请求，丢弃在途队列后退出；字幕存档逐行 flush 已落盘，不受影响。退出须在 daemon 的 2 s 宽限内完成。（`bridge/tests/process.rs`）

### 状态归属

- 每块状态只有一个主人，命令到主人那里改；daemon 的整机 `status` 是轮询来的派生读模型，不是第二份权威。跨进程交换状态只有两条路：`GetStatus` 拉只读快照、字幕文件发布；无跨进程共享可变状态。
- 桥：开关、待翻译/待播报队列、翻译/投递计数、llama-server 与 TTS 连接状态、字幕记录表与文件。识别服务：识别计数、复核状态、桥连接状态。TTS：合成计数、模型加载状态、任务队列。daemon：生命周期、资源仲裁、整机汇总。

### 翻译

- 翻译模型用 llama-server（llama.cpp 的 C++ 服务程序）独立进程，不链库进 Rust，以隔离崩溃域、解耦构建；模型与启动参数经 `[bridge]` 的 `llama_model_id` 与 `[[bridge.models]]` 统一配置（兼容旧单键 `llama_model`），由 `Config::active_llm_model` 同时供给 daemon 启动参数与 bridge 提示词策略。llama-server 由 daemon 监管，跟随 SPEC-04。（`core/src/config.rs`、`daemon/src/supervisor.rs`、`bridge/src/http.rs`）
- 传输用 Unix socket（llama-server 的 --host 填 .sock 路径），协议沿用其原生 HTTP（OpenAI /v1/chat/completions）。这是系统唯一保留 HTTP 协议之处：不重写第三方推理服务，传输层仍统一 Unix socket。板端需确认 llama-server 支持 --host .sock，不支持则重建 llama.cpp 或暂退 TCP 本机回环。（板端确认仍未做，保留退路）
- Rust 桥当 HTTP 客户端（经 Unix socket 连接），负责推理之外的全部编排：预处理（同音字纠正、术语表、数字归一化）、质检、失败重试（最多 2 次）、上下文管理、落字幕、投递。llama-server 只做中文→英文纯推理。（`bridge/src/http.rs`、`bridge/src/translate.rs`、`bridge/src/quality.rs`）
- 同音字纠错（显式表）：显式纠错表搬成 Rust，有行为测试。（`bridge/src/preprocess.rs:13-26`、`bridge/tests/process.rs:395-427`）
- 同音字纠错（术语同音索引，拼音匹配、词频过滤）：术语表生成无声调拼音索引，识别文本中与术语同音、写法不同的 2–4 字中文片段换成术语写法；词频不低于 10 的真词不替换。拼音用 pinyin crate 逐字 plain 读音（与 pypinyin 同源，ü 归一化为 v），词频表内嵌 jieba 的 2–4 字高频子集。先显式表、后拼音索引，接入翻译预处理与字幕原文纠错。（`bridge/src/preprocess.rs`、`bridge/data/word_freq.txt`）
- 第一版整句才翻译（未开复核在 `final` 触发，开启复核待 `refined` 触发）；提前断句（stable_clauses）与可选断句模型推迟到第一版之后，见 FUTURE。

### 播报投递

- [x] 投递队列为有界内存队列，不落盘；队满丢最旧并计数，容量为新增参数、板端实测后定。桥里在途队列（待翻译、待播报）均放内存，落盘的是字幕存档与运行时状态文件（见 SPEC-04）。桥崩溃重启丢失在途播报，不补播旧句；译文存档不受影响（见字幕桥，先于投递写入 subtitles.jsonl）。（`bridge/tests/process.rs:572`）
- [x] 播报 TTS 为独立 Rust 进程，链 sherpa-onnx 的 TTS 库（C/C++ 接口），不链进桥，以隔离崩溃域、解耦构建；由 daemon 监管，跟随 SPEC-04。桥把合格译文发给 TTS 进程，TTS 用参考音色合成，流式喂给 pre 播放（合成一段播一段，沿用旧低延迟路径），不走变声。播报经变声用同一目标音色见 FUTURE。进程形态、不走变声与流式直播均已落地：生成回调每解码一段 PCM 即经有界通道交输出线程按固定单调网格推进 20 ms 节拍发帧（`tts/src/synth.rs`、`tts/src/ffi.rs`、`tts/src/output.rs`），流式合成句内跨段保持单调不增的归一化增益以消除段间音量跳升。首段音频仍须等整句 LM 阶段完成（PocketTTS v1.13.6：LM 整句批处理、解码逐段回调），解码阶段与播放重叠。
- [x] TTS 逐段重采样保留跨回调插值状态，只有整句结束才延展最后一采样；整句时长按整数样本计。该契约针对重采样，不改变句内增益策略。
- [x] 翻译质检按字母与数字计字符比，单字符原文允许短词组扩展。测试同时覆盖正常短句、标点变化和过长拒绝；其余质检不变。
- [x] 播报音频采样率：v1 不走变声，TTS 合成后重采样到播放采样率直接喂 pre 播；经变声输出（FUTURE）时在 TTS 进程内重采样到 16 kHz 契约帧喂 RVC，RVC 输入统一 16 kHz。重采样为纯算术、逐块进行，延迟可忽略；算法沿用旧实现。（v1 路径 24k→48k；16 kHz 分支为 FUTURE 死代码 `tts/src/resample.rs:32-33`）
- [x] 桥到 TTS 接口走 Unix socket + 自有协议（定长头 + JSON），不用 HTTP：TTS 为自有 Rust 进程，区别于第三方 llama-server。任务通道默认 `tts-jobs.sock`（run_dir 下），TTS 绑定、桥连接。命令号 0x06xx（任务 0x0610/停止 0x0612）。应答已改标准信封：type 回显请求号、体 status/code/reason/data、utterance_id 放 data（见 SPEC-01）。
- [ ] 桥发参考音色参数：桥只发 text + utterance_id（`bridge/src/speech.rs:119-123`）；`voice_params` 仅校验不生效（`tts/src/jobs.rs:6`），音色走配置 reference_wav。注：按句切换音色取决于 TTS 模型能力（当前 sherpa-onnx 固定参考音频），非纯接线问题；v1 单音色不受影响，通道预留待模型支持时接通。
- TTS 控制面：绑定 `tts.sock`（run_dir 下），daemon 连接；命令 `Hello` / `GetStatus`，信封见 SPEC-01。`GetStatus` 带合成成功/失败数、是否合成中、模型加载状态、任务队列深度、最近错误。配置读 `[tts]` 段，socket 路径相对 run_dir。（`tts/src/control.rs`、`tts/src/state.rs`）
- 第一版播报原速播放；灵活变速（按时长比例与积压反馈选 1.0–1.5 倍速、保留音高）推迟见 FUTURE。拉伸 DSP 用现成 C 库 libmozart-tempo，控制逻辑与时长模型待启用时搬 Rust。

## 改进点

- 在字幕桥为播报许可引入代次。桥首次处理句末 `final` 时绑定播报资格；等待复核、翻译、投递或重连时不重新获得资格。关停后重新开启不得从桥投递旧代任务，旧句仍更新文字字幕。专项回归覆盖待复核、翻译在途和投递重连（`bridge/tests/process.rs`、`bridge/src/translate.rs`、`bridge/src/speech.rs`）。
- 将播报代次扩展到 TTS 任务、音频输出和 pre 准入。使用同机单调时钟令牌 `issued_ns` 标识桥播报代次；停止建立 `cutoff_ns`，小于截止令牌的任务与帧不得通过，即使延迟到达。严格停止应答等待 TTS 安装截止值及 pre 播放线程处理帧边界。已提交给 ALSA 的尾音自然播完，不以应答宣称声学静音。TTS 和 pre 使用 `frame.h` 的同版播报线包，不能混用旧 `OutputFrame` 发送端。实测与范围见[本机联调报告](../../reports/speech-fence-20261123/RESULTS.md)。

- 复核后再翻译播报：开启复核（`refinement_status = pending`）时，`final` 仅落盘原文字幕不入翻译队列；待 `refined`（`completed` / `failed` / `skipped`）到达后才以最终原文触发单次翻译与 TTS 播报（含终态复核状态校验、空 PCM 防挂起与替换时清理旧纠错字段）。复核未开启（`disabled`）时直接在 `final` 触发一次，消除同一句重复翻译播报两次的问题。（`bridge/src/subtitles.rs`、`asr/src/decoder.rs:216`、`bridge/tests/process.rs`）
- 复核丢失后有界收敛：等待到 30 s 单调时限后标记复核跳过，用在线原文继续文字翻译且不补播。桥重启从存档尾部最多 1 MiB、128 条最近 final 按最高修订恢复；仍未决的复核立即标记中断，已完成不重放，迟到复核不覆盖结果。超出读取范围记日志；无法翻译时标明原因。
- 字幕常驻与播报解耦：由 `SetOutputMode` 切换时不关闭识别和翻译字幕；仅 `broadcast` 模式启用 TTS。用户单独关闭翻译是独立设置，不由模式切换改写。`broadcast` 在无播报帧时静音，不回落到直通输出。验收见[模式往返报告](../../reports/speech-fence-20261123/RESULTS.md)。
- [ ] 端到端 STS 选型边界：8 GB Jetson 端侧暂不以本地端到端中译英 STS 替换现有 `ASR + LLM + TTS` 管线（受限于统一内存与流式生态）；后续若引入云端 STS，仅作为翻译播报模式的可选在线后端，本地管线保留为离线默认兜底。
- [x] 语言链路测试脚本与验收门限加固（步骤 1）：已将 `tools/jetson-language-test.sh` 改为 `startup_mode = "broadcast"` 独占播报隔离并校验 `output_mode.applied == "broadcast"`、`phase == "applied"` 与 `pre.playout_policy == "broadcast_only"`；等待就绪状态连续稳定消除启动首秒竞态，播完后补 `2 s` 尾静音并等待全链路队列及字幕完全排空收敛，采用原子快照避免失败破坏文件；新增仓内可复用核验脚本 `tools/verify-language-run.py`（纯标准库可用，含 23 项正反例自测，`--mode smoke|strict` 如实区分健康硬门限与断句/停顿告警），板端 `tools/jetson-e2e.sh --language` 实测 9/9 必需门限通过（`tools/jetson-language-test.sh`、`tools/verify-language-run.py`）。
- [x] 翻译模型切换至特化小模型与质检误杀修复（步骤 2）：在 `core/src/config.rs` 新增可扩展多模型配置 `[[bridge.models]]`、`llama_model_id`、`glossary_path` 与 `PromptProfile`（`qwen_chat` / `hy_mt` / `custom`，兼容旧 `llama_model`），`daemon/src/supervisor.rs` 按选中模型传递 `-m`、`-c`、`--cache-ram` 等参数；`bridge/src/http.rs` 实现按模板构建请求及术语表、滑动上下文注入；同步完成四项修复——`preprocess::eval_number` 与 `checks.rs` 对齐中文口语省位数字、默认 `max_tokens` 放宽至 `256`、`quality::check` 对英文高频虚词豁免总次数同时保留任意词连续 `>= 3` 次复读拦截、`translate.rs` 对纯 ASCII 英文复核原文直通放行并在有实质变化（去上下文或非零温度）时执行 1 次质检重试；已同步 `docs/config.md`。Jetson 板端已将运行配置切换至 `Hy-MT3-1.8B`（`Q4_K_M`，`hy_mt` 模板、`-c 1024 --cache-ram 64`、`context_window = 2`），7 服务含实时流式 RVC 并发运行 `degraded = []`、`MemAvailable = 1069 MiB`，`jetson-e2e.sh --language` 实测 `11/12` 终句通过、零崩溃零回退。（`core/src/config.rs`、`daemon/src/supervisor.rs`、`bridge/src/{http,preprocess,checks,quality,translate,subtitles,main}.rs`、`docs/config.md`）
- [ ] TTS 合成与播放解耦消除句间空白（步骤 3）：拆分 `tts/src/synth.rs` 单线程阻塞等待 `output.join()` 的流程，引入双线程单句预合成槽，在按实时节拍播放当前句的同时提前跑完下一句的 PocketTTS LM 与解码阶段，消除相邻两句间 `1.5–3.8 s` 机械空白；若长句连发仍形成排队积压，再按需启用预留的弹性变速播放（`1.15–1.2x`）消化积压。验收：连续播报下超过 `1.5 s` 的异常空白占比 `≤ 10%`、单次最大空白 `≤ 2.5 s`、`tts.skipped == 0`。（`tts/src/synth.rs:262-306`）
- [ ] 双工断句与长句软切升级（步骤 4）：取消 `asr/src/decoder.rs` 句音频缓冲满 `700` 帧（`14 s`）直接退出重启的做法，改为连续说话超过 `8 s` 后借小换气或声音低谷安全软切并重置解码流；启用板端 `sherpa-onnx` 原生内置的 `Silero VAD v5` 将纯底噪挡在识别缓冲之外；在 `mozart-asr` 接入轻量端侧轮次结束模型 `Smart Turn v3`（约 `8 MB` INT8 ONNX），以短静音（约 `300 ms`）初判加未完结动态延展窗口替代固定 `rule2 = 0.6 s` 单阈值切句；外放双讲场景后续再衔接 `SPEC-02` 的回声消除（WebRTC AEC3）。验收：同音频 3 轮重放句数波动 `≤ 8%`、切分边界重合度 `≥ 85%`、短碎句率与句尾未完切率均 `≤ 15%`、超过 `15 s` 连续语音零重启零丢句、全栈剩余可用内存 `≥ 700 MiB`。（`asr/src/decoder.rs:20,122-140`、`asr/src/ffi.rs:440-442,472-475`）

## 回归检查

- 测试修正记录已移入 [archive/SPEC-05-language.md](archive/SPEC-05-language.md)「回归检查」节。

## 待讨论

- 无。glossary 编辑与 subtitle clear 留前端模块（见 SPEC-06）。
