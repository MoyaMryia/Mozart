# SPEC 索引

状态：讨论中。最后更新：2026-11-12。

## 总体

以旧实现的数据流和模块边界为基线，重写并改进。按模块小步推进，每步独立验收。

跨模块分工：

- 运行时用 Rust；推理层保留 C++。
- 旧实现的 Python 服务纳入 Rust 重写范围；模型推理仍归 C++。
- 模块边界按旧实现的数据流划分。
- 每个接口契约写在提供方模块的规格里，本文件不复述。
- 前端保持 TypeScript。
- 离线模型导出也在讨论范围内；导出核心依赖 PyTorch，替代方案待评估。
- 验收评分标准见 [REQUIREMENTS.md](REQUIREMENTS.md)。
- 第一版之后的提升点见 [FUTURE.md](../FUTURE.md)。

## 模块索引

| 顺序 | 文档 | 对应旧模块 | 状态 |
| --- | --- | --- | --- |
| 1 | [SPEC-01-core.md](SPEC-01-core.md) | IO：契约帧、环形缓冲、传输 | 讨论中 |
| 2 | [SPEC-02-pre.md](SPEC-02-pre.md) | preprocessor：采集、DSP、播放、路由 | 讨论中 |
| 3 | [SPEC-03-rvc.md](SPEC-03-rvc.md) | rvc-backend：推理核心与调度 | 讨论中 |
| 4 | [SPEC-04-daemon.md](SPEC-04-daemon.md) | state、api、monitor：入口与控制面 | 讨论中 |
| 5 | [SPEC-05-language.md](SPEC-05-language.md) | tools：识别、字幕、翻译、翻译播报、投递、变速 | 讨论中 |
| 6 | [SPEC-06-web.md](SPEC-06-web.md) | frontend | 后续 |
| 7 | [SPEC-07-device.md](SPEC-07-device.md) | usb-audio、usb-gadget | 待定 |
| 8 | [SPEC-08-verification.md](SPEC-08-verification.md) | rvc-golden、reports | 讨论中 |
| 9 | [SPEC-09-pipeline.md](SPEC-09-pipeline.md) | 开发与部署流水线 | 讨论中 |

阅读顺序按数据流：core → pre → rvc → daemon。语言、前端、设备、验证按需插入。

## 讨论与记录方式

- 一次只讨论一个点，确认后再进下一个。
- 只记录边界、已确认决定、改进点和待讨论项，不搬运旧实现细节。
- 需要对照旧实现时按需查阅，不整段复制到规格或对话里。
- 改进点记在对应模块文件中，不集中堆一张大表。
- 每个细节只写在一处：跨模块信息放本文件，模块行为放模块文件，接口契约放提供方模块。

## 变更记录

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
