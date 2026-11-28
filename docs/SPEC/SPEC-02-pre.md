# pre：音频前端

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。
`[x]` 条目详情已于 2026-11-24 移入 [archive/SPEC-02-pre.md](archive/SPEC-02-pre.md)，本文件保留契约、关键决策摘要与活跃待办。

## 边界

- 采集：只做设备 I/O 或测试文件注入。
- 预处理：DSP 与降采样到 16 kHz。
- 播放与路由：直出、变声、翻译播报共用扬声器。

## 已确认决策（摘要）

> 全文条目与证据见 [archive/SPEC-02-pre.md](archive/SPEC-02-pre.md)。与归档冲突时以归档原文为准并回来修正本节。

- UL-UNAS 单测覆盖 STFT 恒等、重打包、首尾、时间戳、切换、重连、背压、文件接管与旧后端回归；板端在隔离目录验证真实模型与 RVC／ASR 分发。
- AEC 探针已在 Jetson 空闲 ALSA 回环子设备验证，不占用现役设备和服务；覆盖率与对齐误差见归档内报告链接。
- 禁用降噪跳过全部计算、pre 与 dsp 拆分 crate 均已实现；行为契约见「已确认」节。

## 已确认

- 采集输出原生格式块；DSP 归预处理。
- 采集到 DSP 使用 core 的环形队列。
- 处理阶段顺序沿用旧实现；禁用阶段直接跳过。（禁用降噪跳过全部计算：`dsp/src/lib.rs:88-93`）
- 直出与变声切换用显式状态机；参数入口校验一次。（`pre/src/route.rs`）
- DSP 用 Rust 重写，逐阶段与旧输出对拍。第一版覆盖全部阶段：80 Hz 高通、WOLA 谱降噪、gate、AGC、limiter、FIR 3:1 降采样与 VAD；对拍夹具由旧 C 核心离线生成，小型 PCM 放 `tests/fixtures/` 并在 `.gitignore` 白名单。（`dsp/src/golden.rs:128-344`，夹具 `tests/fixtures/dsp/` 共 11 项）
- DSP 抽成独立共享 crate：pre 实时链路与 daemon 离线文件降噪共用同一实现，不做两套；逐阶段对拍口径不变。（`pre/Cargo.toml:9`、`daemon/Cargo.toml:11`）
- 采集与播放设备名、数据面 socket 路径等部署配置经 `--config` 读取；daemon 拉起 pre 时传同一配置路径。
- `[pre]` 配置段键：`capture_device`、`playback_device`（默认 `default`）、`capture_queue_frames`（采集→DSP 环容量，默认 8，1..=1024）、`capture_gain_db`（软件前级数字增益，默认 `0.0`，`-24.0..=24.0` dB，R 类运行时键）、`capture_file`（测试采集文件路径，默认不设置）、`consumers`（消费者列表，见下条分发；未设置回退 rvc+asr）。（`core/src/config.rs` PreConfig）
- 文件采集源（测试注入）：`capture_file` 设置后，采集线程读 WAV 文件替代 ALSA 采集，播放仍走 ALSA；用于无麦克风环境的离线对拍，下游 DSP 与分发链路不变。文件契约：PCM16（format tag 1，不含 WAVE_FORMAT_EXTENSIBLE）、48000 Hz、至少 1 声道，取首声道（与设备采集取 FL 一致）；启动时入口校验一次，不合契约报错退出，由 daemon 退避重启。按实时节拍每 20 ms 发射一帧，文件播完循环；`frame_idx` 与 `pts_ns` 语义与设备采集一致。`GetStatus` 的采集设备名报告文件路径。换文件与换设备一样由重启生效，不做在线命令。（`pre/src/file_capture.rs`）
- `[pre.dsp]` 配置段键：`denoise`（降噪总开关，默认 `true`，沿用旧 main.c 代码默认）、`backend`（可选降噪后端，缺省有 `[enhance]` 时取 `ulunas`、否则 `wola`；通过 `core/src/denoise.rs` 目录注册扩展）、`gate`、`agc`、`limiter`（整形级开关，默认 `false`）。禁用降噪时跳过降噪计算，48 kHz 主干无发射延迟、置信度恒 0 且 16 kHz 打包由统一 `resolve_vad` 按主干可闻度驱动能量型 VAD（修复此前 `conf = 0` 导致 `vad_flag` 恒 0 的问题）；禁用整形级时逐位恒等跳过。（`core/src/config.rs`、`core/src/denoise.rs`、`dsp/src/lib.rs`）
- WOLA 谱降噪流式合同（旧实现实测口径）：960 样本定块首块发射 896、其后每块 960；发射流滞后输入流 64 样本，处理级对首块缺口补零保持 20 ms 帧节拍（旧 dsp.c wrapper 语义）。旧注释的“192 样本发射延迟”指 64 样本 hop 的完整重建跨度（frame 128 + hop 64）。（`dsp/src/lib.rs:84-107`、`dsp/src/noise_suppressor.rs:96-114`）
- 整形级作用于 WOLA 发射样本（旧 dsp.c enhance 次序：NS 发射后、补零前）；参数固定为旧实现出厂默认，无配置键。
- 16 kHz 打包：主干 960 对齐帧经 33 抽头 FIR 3:1 降采样为 320 样本契约帧，FIR 历史跨帧连续；InputFrame 的 meta 时间轴与主干帧同窗。`pts_ns` 取采集时间戳（frame.h 语义）；`frame_idx` 为打包序号（旧 `++frame_idx`，从 1 起；旧 main.c 打包时刻改写 pts 的做法不沿用）。（`dsp/src/decimator.rs:9-58`）
- AI 支路发送：处理线程对每帧主干恒定发送 InputFrame 到 rvc `input_socket`，不随路由停发（旧 main.c「AI 路径永不停」）。唯一例外是文件接管：`SetFileMode{active:true}` 期间 rvc 支路停发，把 `input_socket` 让给 daemon 的文件数据面（SPEC-03 复用 + 状态门控交接），避免实时帧污染整段攒帧；`active:false` 恢复恒发。asr 识别支路不受文件接管影响，始终恒发。rvc AI 支路（发送 + 绑 `output_socket`）受 daemon 权威断言门控（SPEC-04 单一权威）：pre 启动/重启后未断言则静默，收到首个 `SetFileMode` 才激活，不单边绑定/恒发，杜绝重启无条件 reclaim 孤儿化 daemon 绑定；断言先置 `file_mode` 再置断言标志，避免激活窗口抢发实时帧。非阻塞发送，过载与对端不在一律丢弃计数；pre 对 input_socket 惰性建链——对端后起不阻塞 pre 启动，未建链期发送丢弃计数，对端就绪后自动接通。（`pre/src/ai.rs:19,57-108,170-217`）
- AI 支路接收：pre 经门控断言后惰性绑定 `output_socket` 收 OutputFrame，应用层接收环 8 帧（4–8 确认范围取上沿）满丢最旧计数；播放线程 rvc 路由取该环、缺帧补零续流，direct 路由逐帧排空该环并按帧计数。文件接管期间接收线程释放 `output_socket` 绑定（daemon 接管同一 socket），交还时重绑恢复接收；释放/重绑经握手标志确认，控制命令在释放完成后才应答（见 SPEC-03「切换时交接绑定」）。（`pre/src/ai.rs:177,196-208`、`pre/src/state.rs:247-265`）
- `consumer_drops` 口径：键随 `[[pre.consumers]]` 列表动态生成，每个消费者报告输入发送丢弃；名为 `rvc` 的消费者额外聚合输出链路——`rvc` = 输入发送丢弃 + 输出坏包丢弃 + 接收环满丢弃（链路异常计数，恒发下应接近 0），`rvc.skipped` = direct 路由期间丢弃的输出帧（设计内丢弃，对应旧 `skipped_replies`，direct 下随回显持续增长属正常）；`tts` 为播报链路固定键。尺寸不符计数是坏包丢弃的子集，控制线程首次发现时把原因写进 `last_error`（守卫定义见 SPEC-01）。（`pre/src/state.rs` Shared::status、`pre/src/control.rs:75-91`）
- 路由与静音淡变为 10 ms（480 样本）线性增益；direct↔rvc 两个方向对称：先淡出完成再切源，帧内过零保持零增益，下一帧新源从零淡入，消除切换单样本跳变；静音为淡到零并保持。（`pre/src/route.rs:62-78`、`pre/tests/route.rs:234-249`）
- 分发：音频进程按配置消费者列表逐个 Unix socket 发送；一个消费者一个 socket，背压互不影响。`[[pre.consumers]]` 声明 name/socket/file_mode_affected（缺省 false 恒发）；未设置回退 rvc+asr 两支路，socket 沿用 `[rvc]/[asr]` 的 input_socket（rvc 受文件接管影响、asr 不受）。入口校验名字唯一、socket 唯一且纯文件名，"tts" 与含点名为保留键。（`core/src/config.rs` PreConsumerConfig/Config::pre_consumers、`pre/src/ai.rs` Sender、`pre/src/main.rs` process_thread）
- 扬声器仲裁的执行归音频进程（淡变、设备写入）；策略（何时切、切到哪、静音与否）由 daemon 下发。直出、变声、翻译播报共用播放设备。
- 音频进程不做控制决策，不管理其他进程生命周期；只执行 daemon 下发的路由与模式指令。
- 直出分支取在降采样前，用 48 kHz 主干；AI 支路降采样到 16 kHz。
- 数据面过载：非阻塞发送，丢弃计数；对端排队上限由发送方 connect 侧 `SO_SNDBUF` 按 4–8 帧执行——Linux AF_UNIX 数据报流控只按发送方发送缓冲记账，接收方 `SO_RCVBUF` 不约束发送方（bind 侧 `SO_RCVBUF` 保留设置但不参与流控）。（`core/src/dgram.rs:174-182`、`pre/src/ai.rs:26`）
- 数据面 socket 命名与方向：`input_socket`（默认 `rvc-in.sock`）由 rvc 绑定收输入帧；`output_socket`（默认 `rvc-out.sock`）由 pre 绑定收输出帧；配置键在 `[rvc]`，路径相对 run_dir。（`core/src/config.rs`）
- Socket 路径在 `/run/mozart/` 下；对端重启后重连（惰性建链）。（`pre/src/ai.rs`）
- 锁文件防重复实例：`run_dir/pre.lock` 独占锁，与 daemon 的 `daemon.lock` 同一模式（std 文件锁）；配置解析后、socket 绑定与 ALSA 打开前拿锁，拿不到报错退出，锁持有到进程退出。（`pre/src/main.rs`）
- 控制面（daemon → pre）：Unix SEQPACKET 定长头 + JSON。命令 `Hello`（版本握手）、`SetRoute{route}`（切 direct/rvc）、`SetMute{mute}`（静音开关）、`SetFileMode{active}`（文件接管开关：暂停/恢复 rvc AI 发送并交接 output_socket 绑定，asr 不受影响）、`SetDsp{denoise,gate,agc,limiter}`（运行中切换 DSP 级）、`GetStatus`。路由与静音由 pre 执行淡变，策略归 daemon。（`pre/src/control.rs`）
- `SetDsp{denoise,gate,agc,limiter}`（运行中切换 DSP 级）：命令号 0x0207（`core/include/mozart/ctl.h`、`core/src/ctl.rs`），请求体 schema::DspSetRequest 四键必需、拒未知键；pre 校验后写目标原子，处理线程逐帧应用，应答回显四个开关。（`pre/src/control.rs` dispatch、`pre/src/main.rs` process_thread）
- 状态：`GetStatus` 返回运行状态、路由、静音、文件接管状态（file_mode）、采集/播放设备名、采集/播放/丢弃帧计数、队列深度、各消费者丢弃计数、最近错误、版本。daemon 每秒轮询，pre 不主动推；旧 `pre-status.json` 废弃。（`pre/src/state.rs`）
- 修改持久基线（`config set`）仍由 daemon 重启 pre 生效；运行时临时换设备走 `PRE_SET_DEVICE`（`0x0208`）。设备枚举归 daemon（SPEC-04），pre 只打开指定设备、热切失败保留旧设备；启动阶段若 `capture_device` 或 `playback_device` 打开失败，不阻塞启动，记录 `last_error` 并将对应槽位标为待机（`DeviceSwitchSlot::mark_standby`，允许后续对同设备名发起 `PRE_SET_DEVICE` 重试），待机期间 `playback` 不虚计实播帧并在双侧恢复活跃后自动清除启动打开报错（`pre/tests/set_device.rs` 主机测试已通过；未验证项：Jetson 板端真实无外设启动与实插拔恢复实测，阻塞原因：尚未部署本次改动至设备，解除条件：部署后在缺外设与重接后热切场景验证 `pre` 运行态与通音）。（`pre/src/main.rs`、`pre/src/state.rs`、`pre/src/capture.rs`、`pre/src/playback.rs`、`pre/src/control.rs`）
- DSP 开关经 `SetDsp` 运行时生效；denoise 重新启用时重置 WOLA 流式状态（噪声估计、频谱队列、增益回到初始，重新攒初始化帧，首块同启动的 64 样本补零），避免陈旧状态伪影；gate/agc/limiter 直接切布尔。（`dsp/src/lib.rs` Dsp::set_switches、`dsp/src/noise_suppressor.rs` NoiseSuppressor::reset）

## UL-UNAS 可选后端

- 新增 `enhance/` C++ 工作进程，以 ONNX Runtime CPU、单线程运行作者预训练权重；禁止随包复制模型，不依赖 PyTorch。
- `[enhance]` 配置选定服务端 16 kHz 增强后端；未设置 `pre.dsp.backend` 且存在 `[enhance]` 时默认启用 `ulunas`，否则默认 `wola`，也可在 `[pre.dsp].backend` 或运行时通过 `PRE_SET_DENOISE` 在同层模块间热切换，WOLA 与 UL-UNAS 不叠加。切换、禁用或帧不连续时重置对应后端会话。
- 48 kHz 直通保留高通及 gate／AGC／limiter，UL-UNAS 仅处理 FIR 后的共享 16 kHz 输入。普通工作线程通过有界队列调用，采集／播放不等待推理。
- 增强进程由 daemon 启停、退避重启并轮询状态；控制命令 Hello=0x0701、GetStatus=0x0702，使用既有 JSON 信封。pre 不生成子进程。
- 音频通过独立 Unix SEQPACKET 会话传输：请求为输入 magic 加 InputFrame；应答为增强 magic、ready 和 InputFrame。首两次 ready=0，此后每请求输出对应两帧前的 320 样本；元数据保留源 frame_idx／pts_ns，不伪造新采集时刻。契约以 core C 头为准。
- 模型使用 512 点周期 Hann、256 点步进和连续缓存。首部反射，固定积攒两帧后输出；结束排空并保留全部样本。算法积攒 640 个 16 kHz 样本，不当作 ADC／DAC 延迟。真实权重对拍固定排除末尾 512 个采样点（离线参考反射边界、流式结束零排空），其余区间误差 SNR ≥ 60 dB；全部首尾另经恒等模型逐样本重建验证。
- 重新计算增强后能量与能量型 VAD，不将模型谱增益冒充语音概率；复用 DSP 的滞回。文件接管的 RVC 门控同时检查源帧采集时和发送时状态，ASR 不受影响。
- 队列溢出、超时、断连、坏应答必须计数并暴露原因；失败不静默回退未增强音频，不重送未知结果。丢帧后重建会话，避免旧缓存跨缺口。

### 接入审查修正

- 预设提交保留生效 DSP 开关，不吸收此前下发失败的新基线；严格校验 `PRE_SET_DSP` 成功应答回显四键。

- 接通 daemon 的 `config set pre.dsp.*` 到 `PRE_SET_DSP`；成功才替换生效配置。自动重启通过 `--effective-dsp` 保留上次成功开关，失败的新基线不得混入恢复。（`daemon/tests/dsp.rs`）

- 自动重启使用生效的增强配置，不吸收 TOML 中仍待重启的后端或 socket。
- 文件接管应答等待 RVC 在途发送归零，使用发送许可与代次隔离，禁止旧帧跨接管恢复回灌。
- daemon 汇总增强数据面降级；主动关闭不报故障，真实成功帧后清除当前故障，保留丢弃计数。
- 控制连接按统一空闲时限回收；离线输出拒绝与模型同 inode；仅按生产两帧排空规则验证短文件与尾样本。


实现、板端功能证据与整机验收边界见[接入报告](../../reports/ulunas-integration-20261117/RESULTS.md)。

改进待办：

- [x] 同层降噪模块封装与运行时热切换：将 `WolaDenoiser` 与 `TrunkProcessor` 解耦出 `Dsp` 条件绑定（保留 `Dsp` 门面供文件离线与黄金对拍），在 `core/src/denoise.rs` 建立单点降噪模型目录与能力元数据生成，在 `pre/src/denoise/`（`mod.rs` / `wola.rs` / `ulunas.rs`）以 `TrunkDenoiser` + 异步 IPC 对等承载本地 48 kHz 与远程 16 kHz 模型并支撑后续模型平滑扩展；修复 `denoise = false` 时 `pack_frame` 因硬编码 `conf = 0.0` 走谱门限导致 `vad_flag = 0` 的静音漏洞（`Option<f32>` 置信度为 `None` 时走能量型 `Vad::update_activity` 且保留 `conf = 0`）；新增 `[pre.dsp].backend`（R 类）及运行时复合键 `pre.dsp.denoise_mode`（`off | <backend>`）、控制命令 `PRE_SET_DENOISE`（`0x020D`）、崩溃恢复参数 `--effective-denoise-backend`，状态上报 `denoise_backend` 与 `denoise_backends` 列表，并在切离 IPC 后端时清零 `enhancement.enabled`；Web 设备面板提供嵌套横向单选折叠框与动态扩展渲染。主机单测与桩集成测试已通过；未验证项：Jetson 板端真实麦克风/音频链路下 `关闭` / `WOLA` / `UL-UNAS` 运行时热切换实听与端到端延迟未验证（阻塞原因：本阶段在开发机完成代码重构与主机模拟验证，尚未部署至 Jetson 硬件与 USB 麦实测环境；解除条件：部署到目标设备后分别在 Direct 与 RVC 路由执行三档热切换实听，确认清辅音不吞字、无爆音且音频持续流畅）。（`core/src/denoise.rs`、`dsp/src/lib.rs`、`pre/src/denoise/`、`pre/src/state.rs`、`daemon/src/{server,services,supervisor}.rs`、`web/src/components/DevicesPanel.vue`）
- [ ] 去伴奏效果不足（用户主观反馈，2026-11-26）：UL-UNAS 对背景伴奏的去除不达预期。先用固定伴奏加人声样本做增强开关对照，客观测量伴奏残留能量与近端语音失真，再决定调参或更换降噪器（候选见 [FUTURE](../FUTURE.md)「降噪器切换」）；不以主观听感直接换模型。

## 声学回声消除（AEC）

目标：适应不同房间混响、音箱摆放与外放设备，防止翻译播报自激与实时变声回环。首选底座为 WebRTC AEC3；处理位序在 `80 Hz` 高通之后、降噪（WOLA／UL-UNAS）与整形级之前。

三项边界已由步骤 2–3 的实现拍板（2026-11-26）：处理采样率取 `48 kHz` 主干（960 样本，AEC3 内部按 10 ms 子块处理）；集成形态为薄 C 封装（`aec3/`，CMake `mozart_aec3`）直链 `mozart-pre`；参考侧时钟漂移以参考环积压修剪加逐帧携带的延时估计吸收。真实双声卡漂移与物理声学延迟的核验、调参仍归步骤 1 实测。

已确认契约（步骤 2–3 已实现）：

- 参考抓取：播放线程仅在 AEC 启用时于 `fade_in` 与 `mute` 之后、ALSA 写成功后（遇欠载先推进 `discontinuity` 再推送新帧），把最终单声道参考帧经定长无锁 SPSC 环（`rtrb`，容量 16 帧）送入处理线程；零分配、非阻塞，环满丢弃并计数。首帧开启或播放待机恢复时将积压修剪到目标 1 帧；稳态超出 2 帧（仅 1 帧漂移）时温和抽丢最旧 1 帧而不重置，突发积压 >= 2 帧则回退到目标 1 帧并重置 AEC。（`pre/src/aec.rs` RefSender/RefReceiver/AecProcessor、`pre/src/playback.rs`）
- 延时估计：AEC 启用时，播放线程以 `snd_pcm_delay`（`runtime_render_delay_ms`，静态开口估算 `buffer_delay_ms` 兜底 25 ms）推算参考首样本抵达 DAC 的单调时刻 `render_at_ns`，采集线程以采读耗时与 `snd_pcm_delay` 推算 `cap_start_ns`，处理线程配对 `(render_at_ns - cap_start_ns) / 1e6`（夹到 `0..=500 ms`）作为动态 `stream_delay_ms` 传入 `ProcessStream`。（`pre/src/playback.rs`、`pre/src/capture.rs`、`pre/src/aec.rs`）
- 因果重置：启用瞬间、采集 `capture_epoch` 跳变（含 ALSA 溢出丢弃残余半帧并复位与处理前环丢帧）、播放 `playback_epoch` 跳变、播放待机恢复、连续 >= 3 帧参考欠载后恢复、突发积压丢帧 >= 2，以及软件前级增益累计偏离基线 `>= 6 dB` 或硬件混音器累计音量偏离 `>= 15%`/AGC 切换均触发 `aec_gain_epoch` / AEC3 重置并计数；播放缺设备待机期间不虚计 `ref_underruns`，短暂参考欠载沿用最近一次配对延时；`capture_clips` 综合统计软件增益及 80 Hz 高通前后的 `|样本| >= 0.99` 削波。（`pre/src/aec.rs`、`pre/src/capture.rs`、`pre/src/control.rs`、`pre/src/main.rs`、`pre/src/playback.rs`、`pre/src/state.rs`）
- 处理位序：`Dsp::process_frame_with_hook` 在 `TrunkProcessor` 的 80 Hz 高通之后、降噪与整形级之前调用 AEC，同时关闭 WebRTC APM 内重复的 80 Hz 高通滤波；禁用时排空参考环、主干逐位直通。（`aec3/src/mozart_aec3.cpp`、`dsp/src/lib.rs`、`pre/src/main.rs` process_thread）
- 配置与控制面：`[pre.aec].enabled`（bool，默认 false，R 类）；`PRE_SET_AEC`（`0x020A`，体 `{"enabled": bool}`）运行时启停，应答回显请求；`config set pre.aec.enabled` 由 daemon 下发，失败保留 TOML 基线与生效值不变。崩溃自愈经 `--effective-aec` 沿用生效开关。（`core/src/config.rs` PreAecConfig、`core/include/mozart/ctl.h`、`pre/src/control.rs`、`daemon/src/services.rs` set_aec、`daemon/src/server.rs` apply_runtime_key）
- 可观测：`GetStatus` 的 `aec` 字段报告 enabled/backend/active/delay_ms/erle_db/erl_db/divergent_fraction/residual_echo_likelihood/delay_median_ms/delay_std_ms/stream_delay_ms/ref_dropped/ref_underruns/resets/capture_clips/last_error；遥测每 25 帧同步一次，热路径只更新原子，AEC 故障只记 `aec.last_error`（禁停/恢复即清零，不污染全局 `pre.last_error`），启用状态下由 `daemon` 汇入 `degraded`，Web 设备面板展示 ERLE 与发散提示。后端不可用时显式记录原因，不修改采集。（`aec3/`、`core/src/ctl/schema.rs`、`daemon/src/services.rs`、`pre/src/aec.rs`、`web/src/contracts.ts`、`web/src/components/DevicesPanel.vue`）
- 构建链接：`aec3/` 经 pkg-config 链接 `webrtc-audio-processing-1`（搜索 `MOZART_ASSETS_DIR` 与 `~/mozart-assets` 的 pkgconfig 路径），缺库时跳过该目标。`pre/build.rs` 在 `MOZART_AEC3_LIB_DIR` 设置时链接 `libmozart_aec3` 并置 `has_aec3`，否则用未链接桩，本机 cargo 检查不依赖外部 C++ 库；`tools/ci-local.sh`、`tools/deploy-jetson.sh` 与 `tools/jetson-unit-test.sh` 在 `libmozart_aec3.so` 存在时自动导出该变量。CTest `aec3_wrapper` 覆盖封装层。（`aec3/CMakeLists.txt`、`pre/build.rs`、`tools/ci-local.sh`、`tools/deploy-jetson.sh`、`tools/jetson-unit-test.sh`、`tests/test_aec3_wrapper.cpp`）
- 离线单测：环满发送侧丢最新、首帧与突发积压修剪重置 vs 单帧漂移温和修剪、禁用直通与清空遥测、待机免虚计欠载及恢复重置、时间戳配对延时与增益跳变/前级削波联动、连续欠载恢复重置、不可用后端报错、`pre_aec_degraded` 汇总、控制面增益跳变阈值；链接真实库时跑 65 ms + 100 ppm 漂移及 45 ms→95 ms 延时跳变恢复测试。（`pre/src/aec.rs`、`pre/src/control.rs`、`daemon/src/services.rs`）

顺序说明：步骤 1 原定先实测定参再实现，步骤 2–3 已先行落地（2026-11-26 提交）；步骤 1 的剩余职责是核验上述已定边界并调参，步骤 4 整机验收不变。

已确认探针契约：

- `--aec-probe <新目录>` 仅对 ALSA 采集启用有界参考抓取。记录每次实际读写的首声道样本、单调时钟／ALSA 延迟、样本序号和恢复代次。I/O 线程不等待诊断队列或写盘；停用时不查询 ALSA 时间戳，不改变原信号路径。
- 离线按采播采样位置估算相对漂移并对齐。不得跨 xrun、时间戳无效或队列丢失的区间插值。两次隔离 ALSA 回环的覆盖率与误差见[报告](../../reports/aec-reference-20261123/RESULTS.md)。

分步待办：

- [ ] 步骤 1（多房间真实声学实录与离线定参）：独占真实麦克风与扬声器窗口，覆盖至少 2–3 种房间／摆放条件（小房间近场、较空旷混响环境、中途挪动位置），用 `--aec-probe` 录制翻译播报打断与同源 RVC 双讲（每组不超过 `60 s`），同步核验播放 ALSA 欠载归零；用 `pre/aec-probe/align.py` 检查真实双硬件时钟漂移、物理声学延迟与对齐覆盖率；离线对比 WebRTC AEC3（`48 kHz`／`16 kHz`）与 SpeexDSP，验收回声衰减（ERLE，消除前后回声能量比）、双讲近端失真、ASR 误识别、RVC 听感、板端单线程 CPU 耗时与内存占用，结果写入 `reports/`，核验已定边界（48 kHz 主干、薄封装直链、积压修剪加延时估计）并调参。环回不替代声学验收。
- [x] 步骤 2（在线参考流与因果对齐）：已实现，契约与证据见上节。（`pre/src/aec.rs`、`pre/src/playback.rs`、`dsp/src/lib.rs` 钩子单测）
- [x] 步骤 3（AEC3 引擎接入与控制面暴露）：已实现，契约与证据见上节；`[pre.aec]` 已同步 [config.md](../config.md)，网页设备面板提供开关与状态。（`aec3/`、`pre/src/aec.rs`、`daemon/src/services.rs`、`web/src/components/DevicesPanel.vue`）
- [ ] 步骤 4（多房间整机验收与回归）：先跑 `tools/jetson-e2e.sh` 确认关闭态零回归；再在多房间真实外放环境验收换房免调参收敛、翻译播报零自激且支持人声打断（衔接 [SPEC-05](SPEC-05-language.md) 的 `Silero VAD v5` 与双工断句）、实时 RVC 外放无啸叫且近端人声不明显发闷，记录端到端延迟增量与 `8 GB` 内存余量。

改进待办：

- [ ] AEC 效果不足（用户主观反馈，2026-11-26）：真实外放音量开大后残余回声仍存在，而且场景识别慢。先完成步骤 1 实测取得 ERLE、延时、漂移与场景切换收敛时间数据，定位根因（声学延时估计偏差、双声卡漂移、音乐类回声、收敛速度等）再调参；不以主观听感直接改滤波器。
- [x] AEC 客观对齐、联动与诊断加固：移除 WebRTC APM 内重复的 80 Hz 高通滤波；`playback` 在 AEC 启用时经 ALSA 运行时延迟（`snd_pcm_delay`）与单调时间戳推算参考渲染时刻，与 `capture` 采帧起始时刻配对计算动态 `stream_delay_ms`，并标记播放待机态（缺设备待机不虚计 `ref_underruns`）；处理线程优化参考环溢出/断流重置与采集溢出/丢帧复位（`capture_epoch`），将削波检测前移到软件前级增益与高通之前，大幅增益突变（软件增益 `>= 6 dB` 或硬件混音大幅改动）触发 AEC 重置；补齐 `erl_db`、`divergent_fraction`、`residual_echo_likelihood`、`delay_median_ms`、`delay_std_ms` 与 `stream_delay_ms` 至 `PreAecStatus`（`daemon` 汇总 `aec.last_error` 至 `degraded`，Web 面板补 ERLE/发散显示）；`ci-local.sh` 与 `jetson-unit-test.sh` 自动探测并链接 `libmozart_aec3.so`。（`aec3/`、`tests/test_aec3_wrapper.cpp`、`core/src/ctl/schema.rs`、`daemon/src/services.rs`、`pre/src/{aec,capture,control,main,playback,state}.rs`、`tools/{ci-local,jetson-unit-test}.sh`、`web/src/`；本机完成全量 `ci-local.sh all`；Jetson 部署后通过 `13/13` CTest、`mozart-pre` 120 单元 + 8 集成（含 `has_aec3`）、`smoke` / `language` 套件，实机双 USB 声卡验证 `stream_delay_ms = 42..43 ms` 与 `-6.5 dB` 增益重置联动，ALSA Loopback TTS 回声实测 4–11 s 纯 AEC3 ERLE = `32.27 dB` 且 `--aec-probe` `align.py` 残差 `0.00 ms`，待机 `ref_underruns = 0`、热接入复位置 `2` 及 `-12 dB` 前级满幅削波 `51` 帧均已验证；真实物理扬声器对麦克风的桌面声学外放录制与 `xrun` 现场尚未验证——阻塞原因：当前未做物理扬声器声学录制与人为注错 `xrun`，解除条件：按步骤 1 实录真实桌面外放回声并用 `align.py` 核验）
- [x] 补延时跳变离线单测（步骤 2 遗留验收项）：补齐稳态积压单帧漂移 vs 突发丢帧、连续参考欠载恢复、45 ms→95 ms 真实 AEC3 中途延时阶跃跳变恢复单测。（`pre/src/aec.rs` `steady_state_backlog_and_prolonged_underrun_reset_policy`、`real_aec3_recovers_after_midstream_delay_jump`；本机与 Jetson ARM64 已链接 `libmozart_aec3.so` 跑通）

## 改进点

- [x] 麦克风增益与调校：`[pre].capture_gain_db` 经 `PRE_SET_CAPTURE_GAIN`（`0x020B`）运行时切换并在 `process_thread` 入口（高通与 AEC 之前）就地缩放且钳位 `±1.0`（`0.0 dB` 逐位直通），daemon 重启自愈通过 `--effective-capture-gain-db` 沿用；同时 `pre/src/mixer.rs` 在采集卡打开/热切换及 `PRE_SET_MIC_MIXER`（`0x020C`，体 `{"volume_percent"?:0..=100,"agc_enabled"?:bool}`）控制路径按需读写 ALSA 硬件 `Capture` 增益与 `Auto Gain Control` 开关（不进入实时采集线程，虚拟/无混音器卡报 `supported=false`）；`GetStatus` 同步上报 `dsp`、`capture_gain_db` 与 `mic_mixer` 快照。主机单测覆盖配置校验、控制协议与 `mixer` 解析；板端真机 USB 麦削波抑制与硬件混音调节未验证（阻塞原因：本阶段尚未部署至 Jetson 硬件实测环境；解除条件：部署后在目标设备网页「麦克风调校」折叠栏调低物理采集卡硬件增益，确认 `capture_clips` 不再递增且近端人声/RVC 输出正常）。（`pre/src/mixer.rs`、`pre/src/main.rs`、`pre/src/control.rs`、`pre/src/state.rs`）
- [x] 受监管 pre 静音启动，重接 `PRE_SET_PLAYOUT` 在同一请求携带路由、静音和播放策略。实时播放确认前选定正确路由；后台未就绪时保持静音，daemon 报告 `reattaching_playout`。
- [x] 零有效样本的 48 kHz 帧进入欠载淡出，不索引 PCM。 见[本机回归](../../reports/postprocessing-stability-20261124/RESULTS.md)。

- [x] 播放连续性：按有效样本数区别后端补零，缺样淡出、恢复淡入；短帧保持原采样率比例。变声与直出取帧等待上限 20 ms（`FRAME_MS`）；采播缓冲均请求 `period × 4 + 960 × 2` 样本，播放起播阈值为两帧契约帧（上限夹到实际缓冲）。`rvc_playout_gap_frames` 与 ALSA 的 `playback_underruns` 分开报告。单测覆盖完整缺帧、恢复、短帧与抖动等待。

- [x] 禁用降噪时跳过全部计算。（`dsp/src/lib.rs:88-93`）
- [x] 淡变逻辑加重入保护，参数做边界校验。（`pre/src/route.rs:72-75`）
- [x] 拆开旧单体主程序。（pre 与 dsp 分 crate）
- [x] 实时原生多采样率（播放端）：播放采样率固定 48 kHz，不按流协商。`BoundaryResampler` 在播放线程入口把模型原生帧重采样到 48 kHz，只取 `valid_samples` 有效段；`sample_rate` 48000 且 `valid_samples` 960 时直通。覆盖 32k/40k/48k 与运行中切率。主条见 SPEC-03。（`pre/src/playback.rs:85-137,198-209`，单测 `:400-478`）
- [x] `BoundaryResampler` 实时线程约束：栈数组进出，无锁、无阻塞、无分配，播放循环前预创建。采样率变化时以新帧首样本重置插值历史；缺帧与切回 direct 时 reset 清零。（`pre/src/playback.rs:103-136,155,195,205`）
- [x] 播放设备必须给到 48 kHz：`set_rate_near` 返回值不等于 48000 即报错退出，由 daemon 退避重启。设备不支持 48 kHz 时不在播放侧改率。（`pre/src/alsa_stream.rs:46-51`）
- [x] 播报分时取帧与切换淡变：`Speaker::Broadcast` 连续发帧期以 4 ms 微抖动等待吸收 TTS 跨进程唤醒抖动，进入 `holdoff` 空窗与实时态立即退回零等待，由 ALSA 写定拍以防抽干硬件缓冲；`Realtime ↔ Broadcast` 切换做 10 ms（`FADE_SAMPLES`）线性淡变；播报期隔离 `Router` 淡变并重置 `BoundaryResampler` 插值历史。（`pre/src/playback.rs`）
- [x] 音频设备热切换（`PRE_SET_DEVICE` `0x0208`）：采集与播放线程在循环边界支持热替换 ALSA PCM 句柄，先试打开新设备再关旧句柄，失败保留旧设备继续运行并回错；切 `capture` 时清空半帧采集缓冲、触发处理线程重置 WOLA 降噪状态（UL-UNAS 同步随 `capture_epoch` 重置）并拒绝 `capture_file` 模式；切 `playback` 时首帧执行 10 ms 线性淡入消杂音；支持 `--effective-capture-device` / `--effective-playback-device` 供崩溃自愈沿用临时设备；物理拔线仍按 `Fatal` 退出。（`pre/src/state.rs`、`pre/src/control.rs`、`pre/src/capture.rs`、`pre/src/playback.rs`、`pre/tests/set_device.rs`；板端通过 `snd-aloop` 跨子设备通音及 USB 麦打开验证，真实扬声器切口主观听感未验证——阻塞原因：无物理扬声器实听环境，解除条件：接物理扬声器热切 `playback` 并人工试听切口有无爆音）
- [x] 扬声器主模式门控：`PRE_SET_PLAYOUT` 在播放帧边界确认策略。托管播报模式只输出 TTS，空窗补零；托管实时变声与直出模式不播放 TTS。pre 受监管重启时静音待断言，旧命令退出托管后恢复原分时。
- [ ] 板端验收播放缓冲余量（衔接 SPEC-03 实时衔接步骤 2）：旧配置的 30 s 实时变声记录有 6 次 ALSA 播放欠载。需要 Jetson 采播设备窗口，使用同一输入重测；验收仍为 30 s 内 ALSA 播放欠载为 0。另记录 RVC 后端欠载和播放缺帧，不能用短淡变掩盖计数。
