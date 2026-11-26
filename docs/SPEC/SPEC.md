# SPEC 索引

状态：讨论中。最后更新：2026-11-26。

## 总体

以旧实现的数据流和模块边界为基线，重写并改进。按模块小步推进，每步独立验收。

跨模块分工：

- 运行时用 Rust；推理层保留 C++。
- 旧实现的 Python 服务纳入 Rust 重写范围；模型推理仍归 C++。
- 模块边界按旧实现的数据流划分。
- 每个接口契约写在提供方模块的规格里，本文件不复述。
- 前端保持 TypeScript。
- 离线模型导出也在讨论范围内；导出核心依赖 PyTorch，替代方案待评估。
- 验收评分标准见 [REQUIREMENTS.md](../REQUIREMENTS.md)。
- 第一版之后的提升点见 [FUTURE.md](../FUTURE.md)。

## 模块索引

| 顺序 | 文档 | 对应旧模块 | 状态 |
| --- | --- | --- | --- |
| 1 | [SPEC-01-core.md](SPEC-01-core.md) | IO：契约帧、环形缓冲、传输 | 讨论中 |
| 2 | [SPEC-02-pre.md](SPEC-02-pre.md) | preprocessor：采集、DSP、播放、路由 | 讨论中 |
| 3 | [SPEC-03-rvc.md](SPEC-03-rvc.md) | rvc-backend：推理核心与调度 | 讨论中 |
| 4 | [SPEC-04-daemon.md](SPEC-04-daemon.md) | state、api、monitor：入口与控制面 | 讨论中 |
| 5 | [SPEC-05-language.md](SPEC-05-language.md) | tools：识别、字幕、翻译、翻译播报、投递、变速 | 讨论中 |
| 6 | [SPEC-06-web.md](SPEC-06-web.md) | frontend、HTTP/SSE 适配器 | 首批接口与部署入口已实现，板端局部联调通过，完整验收保留 |
| 7 | [SPEC-07-device.md](SPEC-07-device.md) | usb-audio、usb-gadget | 待定 |
| 8 | [SPEC-08-verification.md](SPEC-08-verification.md) | rvc-golden、reports | 讨论中 |
| 9 | [SPEC-09-pipeline.md](SPEC-09-pipeline.md) | 开发与部署流水线 | 讨论中 |
| 10 | [SPEC-10-env.md](SPEC-10-env.md) | 模型资产与工具链（环境管理） | 讨论中 |
| 11 | [SPEC-11-preset.md](SPEC-11-preset.md) | 模型预设（按模型持久化会话参数） | 已实现 |
| 12 | [SPEC-12-file-chain-params.md](SPEC-12-file-chain-params.md) | 文件链参数与档位单一来源 | 讨论中 |

阅读顺序按数据流：core → pre → rvc → daemon。语言、前端、设备、验证按需插入。

## 待办清单记法

本文件与各模块规格的待办清单是当前工作的唯一事实源，完成一项勾选 `[x]`。
`FUTURE.md` 只是暂存区，不是待办；从中取出条目先讨论确认，再写入对应模块待办。

各模块规格已于 2026-11-15 按规格-代码对照审查改为待办清单：

- `[x]` 已实现；`[ ]` 未实现或待定。未实现项附证据（文件:行号，审查时点快照，动手前先复核）。
- 标注"待决策"：规格之间或规格与实现不一致，先定方向再动手。
- 标注"存疑"：审查中不确定，需人工核实。
- 标注"（文档）"：只改文档或注释，不改行为。
- 标注 FUTURE / 条件项：有意推迟，不算欠账。

跨模块的待决策项集中在本文件"跨模块冲突"一节；模块内事项在各模块文件。
勾选后的归档时机与条目分类见下方「归档」一节。

## 归档

已完成 `[x]` 条目的细节手动归档到 `archive/`，无脚本、无 hook，不做即时归档。

- 触发：模块状态转"已实现"，或 `[x]` 条目明显妨碍阅读。攒一批、按模块做一次，一次一个模块。
- 移什么：`[x]` 条目全文（含证据与细节）整体移入 `archive/SPEC-XX-<模块名>.md`，按归档日期分节，append-only，不改写历史；归档内相对链接按目录深度调整。
- 源文件留什么：状态、目的、边界原样保留（它们是活的接口契约）；关键决策在源文件压成"已确认决策（摘要）"一节，一行一条并标注指向归档；未完成、待决策、存疑项全部留在原地。与归档冲突时以归档原文为准，回来修正摘要。
- 阅读规则：起步只读本文件；动某模块前只读该模块规格；`archive/` 只在追溯决策细节时定点读，不全量读 `docs/SPEC/`。

2026-11-24 全量落地：除 SPEC-07（无已完成项）外，各模块均已完成首次归档，见 `archive/`。契约类条款去勾选后留在源文件，决策与验收记录入归档。

## 跨模块冲突

1. **播报链路文档残留**（已实现）：SPEC-03 边界已指向 SPEC-05 扬声器分时直播，文档与实现一致。
2. **运行时状态归属**（已实现）：daemon 统一管运行时状态文件 `state.json`，桥不再自写 `bridge-state.json`。
3. **对外控制面**（已定）：Rust 独立适配器进程，全包静态文件 + REST + SSE + 文件上传下载。进程生命周期已按方案 1 归 `daemon` 监管（与业务子进程同级），控制面角色与 `mozart-ctl` 同级——一切控制读写均经 `daemon.sock` 单一权威，不直控业务子进程（仅字幕流只读 `subtitles.jsonl`）。循环等待已破。
4. **部署布局**（已定）：固定运行位 `~/mozart-runtime/bin` 为唯一部署目录，子进程同目录解析不变。releases 版本目录 + 原子切换为后续增强（SPEC-09 待办），不破坏同目录前提。
5. **pre 分发消费者**（已定）：配置化，允许多支路，按统一规范声明消费者。待实现。
6. **控制信封对齐**（已实现）：TTS 任务应答改标准信封，type 回显请求号、体 `status`/`code`/`reason`/`data`，`utterance_id` 放 `data`。

### 输出模式转换研究

- [x] 编制 [daemon／pre／ASR／bridge／TTS 输出模式转移表](../MODE-TRANSITIONS.md)，区分可逆配置、需确认的资源归还、不可撤回的外部效果与需要收尾的句子。以规格原有字幕常驻要求为准，不把语言链编排并入 RVC 资源枚举。

## 讨论与记录方式

- 一次只讨论一个点，确认后再进下一个。
- 只记录边界、已确认决定、改进点和待讨论项，不搬运旧实现细节。
- 需要对照旧实现时按需查阅，不整段复制到规格或对话里。
- 改进点记在对应模块文件中，不集中堆一张大表。
- 每个细节只写在一处：跨模块信息放本文件，模块行为放模块文件，接口契约放提供方模块。

## 变更记录

- 2026-11-26：将默认启动模式收敛为 `direct`（`[bridge].speech_enabled = false`，`translate_enabled = true` 跨模式保持持久），新增保持 `speech_enabled` 不变与托管模式的清队列命令 `DAEMON_SPEECH_CLEAR`（`0x0109`）/ `BRIDGE_SPEECH_CLEAR`（`0x0506`）/ `POST /api/v1/speech/clear` / `mozart-ctl speech clear`；Web 控制台路由卡片接入 `PUT /api/v1/mode` 三态互斥切换并于底部提供独立「翻译」开关，移除左侧独立「翻译播报」卡片，仅在翻译播报开启时于右侧条件展开带交互动效的「播报设置」卡片（含真实队列指标与「清除播报队列」操作，SPEC-04、SPEC-05、SPEC-06、`docs/config.md`、`docs/MODE-TRANSITIONS.md`）。
- 2026-11-26：将声学回声消除（AEC）从 `FUTURE.md` 迁入 `SPEC-02` 分 4 步推进：确立以 WebRTC AEC3 为首选自适应底座、位序位于 `80 Hz` 高通后与降噪前；先以多房间真实麦克风／扬声器实录与离线对比拍板处理采样率、双声卡时钟漂移补偿与封装形态，再依次落地在线参考环、AEC3 控制面接入及多房间整机验收（SPEC-02、FUTURE）。
- 2026-11-25：完成语言链路测试验收加固与可扩展多翻译模型架构切换（SPEC-05 步骤 1–2、`docs/config.md`）：新增 `[bridge].llama_model_id`、`glossary_path` 与 `[[bridge.models]]` 多模型配置（支持 `qwen_chat` / `hy_mt` / `custom` 提示词策略，兼容旧单键 `llama_model`），`daemon` 按选中模型动态传递 `llama-server` 参数，`bridge` 接入术语表与滑动上下文并修复口语省位数字对拍、`max_tokens=256` 长句防截断、虚词重复误杀（保留连续 `>= 3` 次拦截）、纯英文复核直通与有效质检重试；加固 `tools/jetson-language-test.sh` 为 `broadcast` 独占播报加全链路排空并新增 `tools/verify-language-run.py` 自动化核验工具；Jetson 正式运行时已切至 `Hy-MT3-1.8B`（`Q4_K_M`，`-c 1024 --cache-ram 64`），7 服务与流式 RVC 并发稳定，`MemAvailable = 1069 MiB`，`jetson-e2e.sh --language` 9/9 必需门限通过。
- 2026-11-25：新增 Jetson 开机定频与 `systemd` 自启维护及部署适配（SPEC-09、SETUP）：仓库新增独立模板 `deploy/systemd/mozart-clocks.service.in` 与 `deploy/systemd/mozart.service.in` 及安装脚本 `tools/install-jetson-systemd.sh`（默认保留自动调速风扇，支持 `--restart on-failure|no`、`--fan`、`--uninstall` 与最小权限 sudoers）；`tools/deploy-jetson.sh` 与 `tools/jetson-e2e.sh` 适配 `mozart.service` 临时停启并补 3 s 收割等待；同步加固 `TrtEngine` 锁页缓冲（单张量 4 MiB 上限、申请失败回退普通拷贝、卸载前同步流）。
- 2026-11-25：完成实时变声衔接自然度与降延迟优化（SPEC-03）：补齐静音跳过尾音淡出与历史音高缓存推进（步骤 1）、放宽 `frame_idx` 连续时的正向 `pts_ns` 误重置（步骤 2）、补齐实时 `protect` / 120 ms 边缘宽度归一化 `rms_mix_rate` / `0.99` 峰值限幅（步骤 4）、实时音高关闭无声插值恢复声码器清浊音开关（步骤 5），并优化 `IndexSearch`（NEON 向量化 + 质心分块 + 多线程并行）与 `TrtEngine` CUDA 锁页缓冲（配合板端 `jetson_clocks` 定频，240 ms 切块 FP16 单块推理降至 `29.9 ms`、`d=260` 零缺帧净延迟 `398~405 ms`）；四相位锁死消融检出步骤 3 绝对网格初相/噪声与 `3e-3` SOLA 门限存在真实退化与 `+4.9 ms` 开销并已回滚；将默认 `startup_delay_ms` 降至 `340`、默认实时引擎编译 `compile.fp16` 设为 `true`，并归档含 `160 ms` 短块实验在内的实测报告（`reports/realtime-quality-latency-20261125/RESULTS.md`）。

- 2026-11-24：SPEC 归档全量落地：11 个模块的已完成条目按契约/决策分类处理——契约类去勾选保留原文，决策与验收记录逐字入 `archive/`，全部未完成待办逐字保留；SPEC-07 无已完成项不归档。逐行核对原文无丢失（全部模块）。

- 2026-11-24：建立 SPEC 归档机制（本文件新增"归档"一节）：已完成 `[x]` 细节移入 `docs/SPEC/archive/`，源文件保留状态、目的、边界、关键决策摘要与活跃待办；Agent 读取改为定点读，禁止全量读 `docs/SPEC/`。首个试点 SPEC-11（preset）完成瘦身（SPEC-11）。

- 2026-11-24：板端 Rust 构建保持 debug（用户决定，release 编译过慢），SPEC-09 相应待办去掉 `--release`。新增部署提速待办：只编部署需要的 C++ 目标与单个 Rust 桩、调试信息降为 line-tables-only、加分段计时并做改动前后对比（SPEC-09）。

- 2026-11-24：落地停服审查修复：停服超时后由 `poller` 补做收尾（清运行时快照与业务生命周期留下的持久降级原因），`services start` 走同一收尾并重置播报停止屏障；停服校验统一读 `transition` 锁内的权威标志，文件提交在停服期统一回 `NotReady`、排队任务保留；业务角色集合改由 `Role::ALL` + `is_business` 单一来源；删除适配器测试里的恒真断言。同步 GUIDE、架构图与 `RestartRequest` 注释到方案 1（监管 8 角色、`services start|stop`、`restart web`）；SPEC-09 拆出方案 1 部署流的板端实测待办，低端口孤儿保护缺口记入 SPEC-06（SPEC-04、SPEC-06、SPEC-09）。

- 2026-11-24：确认停服与文件任务的边界并登记审查修复待办：`services stop` 只在文件任务正在跑或 GPU 归还中时拒绝（`Busy`），排队任务保留并在恢复服务后继续；停服期间拒绝新的文件提交（`NotReady`），消除带与不带 `model_id` 的两种结果；登记停服超时收尾、停服校验移入 `transition` 锁内、业务角色集合单一来源三项修复。删除 SPEC-04 中「生命周期操作调用可信 CLI」的过期措辞，Web 生命周期以方案 1 为准。低端口（`< 1024`）不做专门适配，默认 `8080`，孤儿保护限制记入文档（SPEC-04、SPEC-06、SPEC-09）。
- 2026-11-24：输出帧固定尺寸不变，显式区分有效样本与补零。实时 RVC 欠载由 pre 淡变，文件末帧不发布填充样本；旧推理代次不能再提交音频或静音。
- 2026-11-24：补齐部署稳定化与停机加固待办（待实现）：板端 Rust 构建改用 `--release`，收紧部署健康检查并核对 `[web].allowed_origins` 缺失告警；本机按需运行 `npm ci`、部署后清理历史备份快照（保留最近 `3` 份）、修正 `deploy.json` 的增量/全量记录；停栈 `pgrep` 增重试缓冲，`poller` 关停快速退出并放宽 `mozart-ctl down` 停机时限（SPEC-04、SPEC-09）。
- 2026-11-24：登记部署与运行侧锁机制、断网恢复及接口收口待办（待实现）：部署锁改为绑定会话自动释放的 `flock` 且仅源码回退必要时清缓存；`core` 长超时表补齐 `DAEMON_MODEL_USE` 与 `DAEMON_FILE_SUBMIT`、`daemon.sock` 队列扩至 `64`；Web 侧补齐 `mode`/`device use` 映射、配置缓存与前端按需刷新；修复前端写锁 SSE 竞态并将幂等写操作改为刷新后自动解锁、SSE 改指数退避、`accept()` 去 `panic!`、字幕优化回放并补齐上传超时额度回收路径（SPEC-04、SPEC-06、SPEC-09）。
- 2026-11-24：按方案 1 落地 Web 生命周期收口：`mozart-web-adapter` 已纳入 `daemon` `Supervisor`（`Role::Web`）统一监管并移除独立 `tools/web-service.sh`，新增 `DAEMON_SERVICES_START` / `DAEMON_SERVICES_STOP`（`0x0123` / `0x0124`）控制音频与 AI 业务子服务组启停（保持 `daemon` 与网页在线），整机完全关停保留给 `mozart-ctl down`；`web-adapter` 同步移除 `--ctl-bin` / `--ready-file` 与 `mozart-ctl` 子进程调起，`deploy-jetson.sh` 合入单一 `ctl down`/`up` 部署流（SPEC-04、SPEC-06、SPEC-09）。
- 2026-11-24：Web 默认监听改为 `0.0.0.0:8080`（默认 `allowed_origins` 同步带 `:8080`），保留手动低端口 `cap_net_bind_service` 兼容；部署启动运行时不再额外注入 shell `LD_LIBRARY_PATH`，运行期环境变量收口至 `[process.env]`；`run_dir/logs` 由 `ctl` 与 `daemon` 统一收敛为 `0700`；`upload_dir` 首次创建规范为 `0700`，拒绝不安全现有目录时报错打印具体路径与权限/原因（SPEC-04、SPEC-06、SPEC-09）。

- 2026-11-23：将已下发直通与实时 RVC 租约归还分开；实时停流未确认时保留租约。新增统一输出模式命令、播报停止屏障与 ASR 复核有界收尾；本机证据见输出模式转移表。文件终态与租约归还分开，pre 按模式独占选源；配置了模型时默认启动实时变声。

- 2026-11-22：登记实时变声衔接自然度分步优化与延迟拆解待办：将实时变声的停顿/开口咔嗒声、偶发断音、持续说话字间轻微颤动与字头吐字失真拆为 4 步可独立验收待办，并补充实时延迟构成拆解与分阶段降延迟待办；从 `FUTURE.md` 迁出 `protect 与 rms_mix_rate 实时对齐` 入正式待办跟踪（SPEC-02、SPEC-03、FUTURE）。
- 2026-11-22：确认并登记本机↔Jetson 跨平台 SSH 音频桥接工具（`tools/jetson-audio.py`）：单文件 Python（PEP 723 + `sounddevice`）直连本机系统默认扬声器与默认麦克风，不建系统虚拟麦克风；纯 SSH 原始 PCM 管道（48 kHz / `S16_LE` / 2 ch / 20 ms），支持 `play`/`mic`/`duplex`、同步 WAV 录制、有界缓冲防漂移、板端 `Loopback` 复用或 `device use` 智能热切与退出还原、故障显式报错退出（SPEC-09）。
- 2026-11-22：确认「复核后再翻译播报」与「三主模式互斥 + 常驻字幕」边界：开启复核时等 `refined` 后仅触发单次翻译与播报；音频主输出收敛为实时变声（默认启动）/翻译播报（说话间隙静音并停释 RVC）/原声直出三主模式，识别与翻译字幕常驻；明确 8 GB 端侧保留本地语言管线、云端 STS 仅作翻译模式可选在线后端（SPEC-02、SPEC-04、SPEC-05）。
- 2026-11-22：确认并登记 FUTURE「音频设备热切换」：新增控制命令 `device use <capture|playback> <设备名>`（daemon `0x0121` / pre `0x0208`），只做临时实时切换、不写 `mozart.toml` 或 `state.json`；`config set pre.capture_device/playback_device` 保持 S 类。daemon 内存保留生效设备供 `pre` 崩溃自愈恢复，显式 `restart pre` 或 `down`/`up` 回到基线。`pre` 先开新再关旧、失败保留旧设备，切 `capture` 重置 WOLA 状态并拒绝 `capture_file` 模式，切 `playback` 首帧 10 ms 淡入，不做物理拔线自动自愈（SPEC-02、SPEC-04、FUTURE）。
- 2026-11-17：用户确认接入 UL-UNAS。新增 `enhance/` C++ 推理工作进程，归 SPEC-02；daemon 统一管理生命周期，pre 通过有界队列在普通线程调用。增强 RVC／ASR 共用的 16 kHz 支路，不降低 48 kHz 直通带宽。初版为可选后端，旧 WOLA 默认不变。隔离开发目录为 `~/MozartMryia`，只读引用既有模型，不复制模型或覆盖正式部署。
- [x] 完成 UL-UNAS 跨模块接入：帧协议、配置、daemon 监管、pre 分发、构建检查和隔离板端测试；对应模块验收见 SPEC-02，整机验收范围见[接入报告](../../reports/ulunas-integration-20261117/RESULTS.md)。
- 2026-11-17：实现首批独立 HTTP/SSE 适配器；完整命令面及产品验收见 SPEC-06、SPEC-08。
- 2026-11-16：定义 v1 HTTP/SSE 适配器契约，创建 Vue 控制台。

- 2026-11-10：建立模块化规格。
- 2026-11-10：确认方向、采集边界、队列语义、RVC 进程形态；规格内容按模块归位。
- 2026-11-10：模块规格文件名加编号。
- 2026-11-10：旧 Python 服务用 Rust 重写；取消独立 overview，总览并入本文件。
- 2026-11-10：赛题评分标准迁入 `docs/REQUIREMENTS.md`，验收规格引用该文件。
- 2026-11-10：功能验收写入 `SPEC-08-verification.md`；翻译播报替代参考音色播报。
- 2026-11-10：一致性核对：术语统一、门槛来源标注、待讨论项归位。
- 2026-11-10：新增开发与部署流水线规格，建立一键本地入口的讨论框架。
- 2026-11-11：补充推理层接口约束草案。
- 2026-11-11：新增第一版之后的提升点清单。
- 2026-11-11：确认同机传输与控制消息格式。
- 2026-11-11：确认输出缓冲归属，大缓冲留在 C++。
- 2026-11-11：补写 pre 分发、仲裁与直出决定；确认帧契约 C 单一来源；记录 1:1 泵时钟耦合假设。
- 2026-11-11：控制面归独立 daemon；基础链路定为三进程，daemon 不在数据路径上。
- 2026-11-11：补记识别服务复核、生命周期、状态内容与桥断线补发决定；SPEC-01 补识别事件流通道。
- 2026-11-11：字幕桥定为一个进程；字幕发布沿用文件，推流方留到前端模块定。
- 2026-11-11：字幕编号改用识别启动号:句号，事件新增启动号字段。
- 2026-11-11：控制面形式定为 daemon 单一权威 + CLI 瘦客户端 + 控制 socket；CLI 为统一启动入口。
- 2026-11-11：确认 CLI 设计范式（名词-动词、--json 稳定契约、瘦客户端、clap）。
- 2026-11-11：启动方式定为 CLI spawn；模式建模定为资源仲裁，v1 先做 GPU 单资源，扬声器与播报仲裁入 FUTURE。
- 2026-11-11：pre 控制面定稿（Hello/SetRoute/SetMute/GetStatus，每秒轮询，换设备用重启）。
- 2026-11-11：监控归 daemon（进程监管 + 业务指标轮询 + 硬件指标采集 + 日志），不独立进程。
- 2026-11-11：翻译运行方式定稿：llama-server 独立进程，Unix socket 传输 + 原生 HTTP，Rust 桥做编排，不链库。
- 2026-11-11：确认控制信封格式（16 B 头 + JSON 体）与命令号分段。
- 2026-11-11：确认 daemon 控制命令、socket 布局与配置来源。
- 2026-11-11：确认进程监管退避与子进程日志位置。
- 2026-11-11：确认路由切换顺序与轮询对账策略。
- 2026-11-11：确认 CLI 命令面与退出码。
- 2026-11-11：确认控制连接线程化、空闲断开与子进程终止宽限。
- 2026-11-11：播报投递队列定为有界内存、不落盘；在途队列均内存，译文存档仍走 subtitles.jsonl。
- 2026-11-11：播报 TTS 定为独立 Rust 进程（链 sherpa TTS 库），合成后喂 RVC 链路；接入细节入 FUTURE。
- 2026-11-11：桥到 TTS 接口定为 Unix socket + 自有协议（区别于 llama-server 的 HTTP）。
- 2026-11-11：同音字纠错确认保留，Rust 复现并与旧实现逐条对拍。
- 2026-11-11：提前断句（stable_clauses）与断句模型推迟到 FUTURE，第一版整句翻译。
- 2026-11-11：确认 RPC 超时覆盖 connect/send/recv、PDEATHSIG 孤儿防护与关停幂等。
- 2026-11-11：灵活变速推迟到 FUTURE，第一版播报原速；拉伸 C 库 libmozart-tempo 保留备用。
- 2026-11-11：filter_radius 定为保留 FUTURE 提升项、推迟实现，v1 不暴露死参数。
- 2026-11-11：播报音频由 TTS 进程内重采样到 16 kHz 契约帧，RVC 输入统一 16 kHz；rvc 待讨论清空。
- 2026-11-11：确认数据面 socket 命名与方向：`rvc-in.sock` 由 rvc 绑定收输入帧，`rvc-out.sock` 由 pre 绑定收输出帧。
- 2026-11-12：控制面命令覆盖字幕链路；语言服务接线定稿（各绑控制 socket，命令号 asr `0x04xx` / bridge `0x05xx` / tts `0x06xx`，llama-server 可达性由桥带报）。字幕文件改放持久本地存储，默认路径与轮转待确认。
- 2026-11-12：字幕文件默认 `~/.local/state/mozart/subtitles.jsonl`（XDG 状态目录）；v1 不轮转，留实测后定。
- 2026-11-12：字幕链路控制定稿（v1 命令 translate/speech on/off、状态归属各归其主、桥/TTS 状态字段）；持久化定为 TOML 只读基线 + 运行时状态文件，每次改动原子写、重启恢复、不回写 TOML。关机缺口（daemon 信号处理、桥 SIGTERM 收尾）入待讨论。
- 2026-11-12：收尾三开口：speech stop 纳入 v1；daemon 捕获 SIGTERM/SIGINT 走优雅关停；桥 SIGTERM 丢弃在途、字幕存档不受影响。字幕文件与控制面交接定稿。
- 2026-11-12：确认 `[pre]` 配置段设备与环容量键，以及路由/静音淡变时长与切换次序（SPEC-02）。
- 2026-11-12：确认 `[pre.dsp]` 配置键（denoise，默认沿用旧代码默认 true；禁用整级跳过）与 WOLA 流式合同（首块 896、稳态 960、发射滞后 64、缺口补零保持 20 ms 节拍），对拍夹具入 `tests/fixtures/dsp/`（SPEC-02）。
- 2026-11-12：pre AI 支路定稿：整形级配置键（gate/AGC/limiter 默认关）、16 kHz 打包 meta 语义（pts 取采集时间戳、frame_idx 为打包序号）、恒定发送与惰性建链、接收环 8 帧、direct 排空计数与 consumer_drops 口径（rvc / rvc.skipped）（SPEC-02）。
- 2026-11-12：rvc C++ 实现定稿：六命令按 SPEC-03 落地（StartStream 参数此时固定、StopStream 等当前块返回、GetStatus 14 字段）；`[rvc]` 进程启动键（models_dir、device、half、cache_limit、HuBERT/RMVPE/实时资产路径）与 socket 键入部署 TOML；退出用 SIGTERM，第一版无 UnloadModel/Shutdown。
- 2026-11-12：daemon 接线定稿：daemon 把自己解析出的 TOML 路径随 spawn 以 `--config` 转交子进程，内置默认启动时无文件不传，子进程按同一顺序得到同一默认值；部署布局定 cargo 产物目录为唯一部署目录，rvc CMake 产物复制进去或用 `MOZART_RVC_BIN` 指绝对路径（SPEC-04）。
- 2026-11-12：板端实测定稿：同源 60 s 长跑小缓冲零掉帧、49.86 帧/s 1:1，维持大缓冲留 C++；rvc 端到端净滞后约 0.5 s（SPEC-03 补实测确认行）。实测记录入 `reports/realtime-pipeline-20261112`。
- 2026-11-12：路由切换淡变双向对称化：rvc→direct 不再立即切源，与 direct→rvc 一致先淡出到零、帧内零保持、下一帧新源淡入，消除约 14% 满幅的单样本跳变（SPEC-02）。
- 2026-11-12：数据面缓冲措辞修正：4–8 帧上限由发送方 connect 侧 `SO_SNDBUF` 执行，Linux AF_UNIX 数据报流控不按接收方 `SO_RCVBUF` 记账；非阻塞发送与丢弃计数口径不变（SPEC-02）。
- 2026-11-12：配置写回定稿（取代同日"不回写 TOML"）：模型甲保留（TOML 基线 + 状态文件覆盖）；新增 `config set` 单键写 TOML、`config save` 批量提升状态→TOML；键分运行时/重启两类，重启键响应带 `restart_required`+`affected` 稳定契约、`status` 显示待重启；重启支持单服务与整栈两粒度；手改 TOML 随意不防冲突。TOML 规范文件后续（SPEC-04）。
- 2026-11-12：文件任务队列 v1 有界内存不落盘、重启丢待转任务，落盘持久化入 FUTURE；澄清 RVC 模式为资源仲裁派生状态（不入配置状态文件），会话参数实时/文件共用一份配置覆盖（SPEC-04）。
- 2026-11-12：文件转换入口定稿：v1 用 CLI `file submit` 提交本地路径，daemon 复制进受管目录后入队；校验沿用扩展名白名单、100 MiB 预检、300 s 时长上限（SPEC-04）。
- 2026-11-12：文件链路冲突裁决定稿：等待能解决则排队/挂起（文件入队等 GPU、GPU 空闲自动转、切实时挂单等待槽），等待不能解决则拒绝；替换原“拒绝或挂等待槽”未定措辞（SPEC-04）。
- 2026-11-12：文件队列暂停/恢复保留（`file pause`/`file resume`，暂停只停新任务、当前任务跑完）（SPEC-04）。
- 2026-11-12：文件任务按任务选模型保留（`file submit --model`，缺省用当前模型）；模型存在性改执行时 `LoadModel` 验证，失败标 failed、队列继续（SPEC-04）。第二点队列与仲裁 2.1–2.5 全部定稿。
- 2026-11-12：文件链路补 model 查询：rvc 增 `ListModels` 命令、CLI `model list`；模型存在性改回入队时校验、不存在当场拒收（取代同日 2.5 的执行时验证）（SPEC-03、SPEC-04）。
- 2026-11-12：文件数据面定稿：daemon 作 Rust 中间层，C++ 只收/发帧不碰文件；复用帧契约，文件发送不丢（阻塞/流控），输出 socket 由对端绑定（实时 pre/文件 daemon）、切换交接（SPEC-03、SPEC-04）。
- 2026-11-12：离线推理定稿：v1 整段批处理、质量参数沿用、内存由 300 s 兜底，分块流式入 FUTURE；文件降噪保留，Rust DSP 抽共享 crate 供 pre 实时与 daemon 离线共用（SPEC-02、SPEC-03、SPEC-04）。
- 2026-11-12：整段 F0 耗时与取消等待的板端实测推迟，作为是否分块 F0 的依据，并入 FUTURE“离线推理分块流式”前置。
- 2026-11-12：文件取消定稿：daemon 编排（排队移除删源 / 执行中 StopStream），临时文件归 daemon 清理，StopStream 粒度按 kind 区分（实时 240 ms / 文件下一检查点），CLI `file cancel`（SPEC-03、SPEC-04）。
- 2026-11-12：文件结果生命周期定稿：存储目录默认 `~/.local/state/mozart/files/`（持久、TOML 可配），原子发布沿用，驱逐沿用 1000 MiB/80%/护未完成（已完成可删），取结果 CLI `file fetch`（SPEC-04）。
- 2026-11-12：文件状态与进度定稿：六态状态字段 + 结果可取标记（去模式枚举），进度粗阶段 + 推理段用 rvc 块计数细化，CLI `file status` / `file list`；rvc 文件模式 blocks 按窗计数（SPEC-03、SPEC-04）。文件转换链路 7 点全部定稿。
- 2026-11-12：配置分类定稿为机制 + 切换成本、能即时生效就运行时（取代“仅 bridge 两开关”）：daemon 超时 / [file] 阈值 / RVC 会话参数（Stop→Start）/ pre DSP（新增 SetDsp）均运行时，固化进结构者重启；新增 [file] 段；schema 落 docs/config.md（SPEC-02、SPEC-04）。
- 2026-11-12：播报定稿修正：v1 继承旧参考音色播报（TTS 流式直播、扬声器分时），撤销 11-10“走变声替代参考音色”，经变声同音色入 FUTURE；文件队列改落盘续转、SPEC-08 文件验收改 CLI、TARGET 运行时语言改 Rust+C++（SPEC-04、SPEC-05、SPEC-08、TARGET、FUTURE）。
- 2026-11-13：音频设备识别定稿：枚举归 daemon（ALSA API、按需不缓存、id+标签、配置校验），CLI `device list`；pre 只用设备不枚举；设备热切换入 FUTURE（SPEC-02、SPEC-04）。
- 2026-11-13：新增 `[process.env]` 段，daemon spawn 子进程时注入 TOML 定义的环境变量（SPEC-04、config.md）。
- 2026-11-13：模型注册表定稿：TOML 顶层 `[[models]]` 数组（id/type/path），LoadModel 和 ListModels 优先从注册表解析路径，注册表为空时回退 models_dir 扫描；特征引擎四路径改为可选，缺省从 models_dir 按约定文件名自动发现（SPEC-03、config.md）。
- 2026-11-16：新增 SPEC-11 模型预设与 SPEC-12 文件链参数索引。
- 2026-11-16：FUTURE 模型相关三条（多模型适配、实时原生多采样率、采样率调研）迁入 SPEC-03 并解除推迟，跨模块链接项落 SPEC-01/02/04；新增实时流中换模型条目（SPEC-03 推理侧、SPEC-04 命令与编排）。
- 2026-11-16：实时几何与资产参数契约入 SPEC-03 新节（单一来源 `realtime_chain_params.json`，非形状类旋钮归 `[rvc.realtime]`，实时独有、不进预设）；SPEC-12 实时形状归属改指 SPEC-03。
- 2026-11-16：待办流程定稿：SPEC 待办清单为事实源，完成后勾选；FUTURE 仅作暂存区，取出条目先讨论再登记。AGENTS.md 增加此约束并移除“待定事项”：已定项删除，文档检查缺口迁入 SPEC-09。
- 2026-11-16：SPEC-11 模型预设实现：`preset save/use/list`、模型切换载入 `default.toml`、文件链 RVC 参数快照后移到交接后、板端 preset 套件。
- 2026-11-16：SPEC-11 修订：载入拆为读校验与写基线两步、`LoadModel` 夹在中间，加载失败不留参数错位；模型存在性归推理层；损坏预设回 Internal；`preset save` 校验参数来源；CLI 三个 preset 命令统一长超时、`config set` 接受负值（SPEC-04、SPEC-11）。
- 2026-11-16：待重启显示扩到键级：`GetStatus` 与 `config get` 应答带 `pending_restart_keys`，CLI 在值后提示尚未生效的键；键按受影响服务集合清除（SPEC-04、config.md）。
- 2026-11-17：输出帧契约扩为 3868 B，新增 `sample_rate` 与 `valid_samples`（线包偏移 20 与 24），共享夹具更新为 48000/960（SPEC-01）。
- 2026-11-17：实时链路原生多采样率（32k/40k/48k），几何按模型采样率推导；播放侧固定 48 kHz，pre 新增播放边界重采样，不做播放采样率协商（SPEC-02、SPEC-03）。
- 2026-11-17：新增 `DAEMON_MODEL_USE` 0x0120 与 CLI `model use <id>`，定为断流切换（Stop→LoadModel→预设提交→Start），避免 8 GB 设备双引擎驻留 OOM，支持跨采样率重建 `StreamingRvc`（SPEC-03、SPEC-04）。
- 2026-11-17：边界变更（用户确认）：`model use` 允许把 `[rvc].model_id` 写回 TOML 基线，写回只在 `LoadModel` 成功后的提交临界区、失败整体回退；预设载入路径仍不碰模型身份，`config set rvc.model_id` 仍拒绝（SPEC-11、SPEC-04、config.md）。
- 2026-11-17：`[rvc.realtime]` 四旋钮热更新纳入 `config set` 的 Stop/Start 判据；新增整除约束（crossfade+sola 为 10 ms 倍数、startup 为 20 ms 倍数）与上限，违反在入口回 InvalidArgument（SPEC-03、SPEC-04）。
- 2026-11-17：实时几何单一来源扩到旋钮默认值、上限与整除模数，C++ 集中在 `realtime_geometry.hpp` 并由 `validate_realtime_knobs` 单点判定，新增读 JSON 的对拍测试（SPEC-03）。
- 2026-11-17：新旧进程混跑诊断：数据面不做版本协商，尺寸不符只在热路径计数，控制线程首次出现时记可查询原因，要求同代重建 pre/rvc/tts/daemon（SPEC-01、SPEC-02）。
- 2026-11-17：边界变更（用户确认，取代同日 overlap 范围可调部分）：`crossfade_ms + sola_search_ms` 锁为定值 60 ms（50 + 10），入口拒绝偏离，原因是引擎形状按最大 overlap 固定、分析窗尚未与之解耦；`startup_delay_ms` 与 `skip_silence` 仍可调；淡化/SOLA 解耦推迟，待办落 SPEC-03 改进点（SPEC-03、config.md、FUTURE）。
- 2026-11-17：新增板端套件 `tools/jetson-realtime-mm.sh`（`--realtime-mm`）：跨采样率 `model use`、overlap 定值拒绝、`startup_delay` 可调、失败恢复、40 kHz 文件链与资产隔离；板端实测 48 kHz↔40 kHz 冒烟通过，32 kHz 与延迟/听感仍未验证（SPEC-03、SPEC-11、SETUP）。
- 2026-11-17：FUTURE 清理：删「淡化与 SOLA 可调」「多预设与 last-used」两行，待办已分别由 SPEC-03、SPEC-11 完整跟踪；听感基准行删模型覆盖表述，文档不记模型数量；SPEC-11 交叉引用同步（FUTURE、SPEC-11）。
- 2026-11-18：播报链路四项修复：桥移除自写 `bridge-state.json`、统一由 daemon `state.json` 持久化并在桥重启后重下发；修复 TTS 发帧节拍正漂移、句内跨段增益跳变与 pre 播报取帧等待/切换淡变/重采样器重置（SPEC-02、SPEC-04、SPEC-05）。
- 2026-11-18：实现 FUTURE「在线参数调整（SetParams）」：`core` 新增 `MOZART_CTL_RVC_SET_PARAMS`（`0x0308`），`RVCInferencer` 在块边界取参数快照、`StreamingRvc` 原子更新 `skip_silence` 而不重建推理器或流缓冲；`daemon` 对 `pitch_shift`/`index_rate`/`f0_method`/`realtime.skip_silence` 改走 `SetParams` 无中断生效，`startup_delay_ms` 维持 `StopStream`→`StartStream`（SPEC-03、SPEC-04、SPEC-11、FUTURE、config.md）。
