# pre：音频前端

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。

## 边界

- [x] 采集：只做设备 I/O 或测试文件注入。
- [x] 预处理：DSP 与降采样到 16 kHz。
- [x] 播放与路由：直出、变声、翻译播报共用扬声器。

## 已确认

- [x] 采集输出原生格式块；DSP 归预处理。
- [x] 采集到 DSP 使用 core 的环形队列。
- [x] 处理阶段顺序沿用旧实现；禁用阶段直接跳过。（禁用降噪跳过全部计算：`dsp/src/lib.rs:88-93`）
- [x] 直出与变声切换用显式状态机；参数入口校验一次。（`pre/src/route.rs`）
- [x] DSP 用 Rust 重写，逐阶段与旧输出对拍。第一版覆盖全部阶段：80 Hz 高通、WOLA 谱降噪、gate、AGC、limiter、FIR 3:1 降采样与 VAD；对拍夹具由旧 C 核心离线生成，小型 PCM 放 `tests/fixtures/` 并在 `.gitignore` 白名单。（`dsp/src/golden.rs:128-344`，夹具 `tests/fixtures/dsp/` 共 11 项）
- [x] DSP 抽成独立共享 crate：pre 实时链路与 daemon 离线文件降噪共用同一实现，不做两套；逐阶段对拍口径不变。（`pre/Cargo.toml:9`、`daemon/Cargo.toml:11`）
- [x] 采集与播放设备名、数据面 socket 路径等部署配置经 `--config` 读取；daemon 拉起 pre 时传同一配置路径。
- [x] `[pre]` 配置段键：`capture_device`、`playback_device`（默认 `default`）、`capture_queue_frames`（采集→DSP 环容量，默认 8，1..=1024）、`capture_file`（测试采集文件路径，默认不设置）、`consumers`（消费者列表，见下条分发；未设置回退 rvc+asr）。（`core/src/config.rs` PreConfig）
- [x] 文件采集源（测试注入）：`capture_file` 设置后，采集线程读 WAV 文件替代 ALSA 采集，播放仍走 ALSA；用于无麦克风环境的离线对拍，下游 DSP 与分发链路不变。文件契约：PCM16（format tag 1，不含 WAVE_FORMAT_EXTENSIBLE）、48000 Hz、至少 1 声道，取首声道（与设备采集取 FL 一致）；启动时入口校验一次，不合契约报错退出，由 daemon 退避重启。按实时节拍每 20 ms 发射一帧，文件播完循环；`frame_idx` 与 `pts_ns` 语义与设备采集一致。`GetStatus` 的采集设备名报告文件路径。换文件与换设备一样由重启生效，不做在线命令。（`pre/src/file_capture.rs`）
- [x] `[pre.dsp]` 配置段键：`denoise`（WOLA 谱降噪开关，默认 true，沿用旧 main.c 代码默认）、`gate`、`agc`、`limiter`（整形级开关，默认 false，沿用旧 main.c 代码默认）。禁用降噪时该级全部计算跳过：不走旧实现的旁路 WOLA 重建，无切换收敛期、无发射延迟，置信度恒 0；禁用整形级时逐位恒等跳过。（`core/src/config.rs:60-113`、`dsp/src/lib.rs:88-93`）
- [x] WOLA 谱降噪流式合同（旧实现实测口径）：960 样本定块首块发射 896、其后每块 960；发射流滞后输入流 64 样本，处理级对首块缺口补零保持 20 ms 帧节拍（旧 dsp.c wrapper 语义）。旧注释的“192 样本发射延迟”指 64 样本 hop 的完整重建跨度（frame 128 + hop 64）。（`dsp/src/lib.rs:84-107`、`dsp/src/noise_suppressor.rs:96-114`）
- [x] 整形级作用于 WOLA 发射样本（旧 dsp.c enhance 次序：NS 发射后、补零前）；参数固定为旧实现出厂默认，无配置键。
- [x] 16 kHz 打包：主干 960 对齐帧经 33 抽头 FIR 3:1 降采样为 320 样本契约帧，FIR 历史跨帧连续；InputFrame 的 meta 时间轴与主干帧同窗。`pts_ns` 取采集时间戳（frame.h 语义）；`frame_idx` 为打包序号（旧 `++frame_idx`，从 1 起；旧 main.c 打包时刻改写 pts 的做法不沿用）。（`dsp/src/decimator.rs:9-58`）
- [x] AI 支路发送：处理线程对每帧主干恒定发送 InputFrame 到 rvc `input_socket`，不随路由停发（旧 main.c「AI 路径永不停」）。唯一例外是文件接管：`SetFileMode{active:true}` 期间 rvc 支路停发，把 `input_socket` 让给 daemon 的文件数据面（SPEC-03 复用 + 状态门控交接），避免实时帧污染整段攒帧；`active:false` 恢复恒发。asr 识别支路不受文件接管影响，始终恒发。rvc AI 支路（发送 + 绑 `output_socket`）受 daemon 权威断言门控（SPEC-04 单一权威）：pre 启动/重启后未断言则静默，收到首个 `SetFileMode` 才激活，不单边绑定/恒发，杜绝重启无条件 reclaim 孤儿化 daemon 绑定；断言先置 `file_mode` 再置断言标志，避免激活窗口抢发实时帧。非阻塞发送，过载与对端不在一律丢弃计数；pre 对 input_socket 惰性建链——对端后起不阻塞 pre 启动，未建链期发送丢弃计数，对端就绪后自动接通。（`pre/src/ai.rs:19,57-108,170-217`）
- [x] AI 支路接收：pre 经门控断言后惰性绑定 `output_socket` 收 OutputFrame，应用层接收环 8 帧（4–8 确认范围取上沿）满丢最旧计数；播放线程 rvc 路由取该环、缺帧补零续流，direct 路由逐帧排空该环并按帧计数。文件接管期间接收线程释放 `output_socket` 绑定（daemon 接管同一 socket），交还时重绑恢复接收；释放/重绑经握手标志确认，控制命令在释放完成后才应答（见 SPEC-03「切换时交接绑定」）。（`pre/src/ai.rs:177,196-208`、`pre/src/state.rs:247-265`）
- [x] `consumer_drops` 口径：键随 `[[pre.consumers]]` 列表动态生成，每个消费者报告输入发送丢弃；名为 `rvc` 的消费者额外聚合输出链路——`rvc` = 输入发送丢弃 + 输出坏包丢弃 + 接收环满丢弃（链路异常计数，恒发下应接近 0），`rvc.skipped` = direct 路由期间丢弃的输出帧（设计内丢弃，对应旧 `skipped_replies`，direct 下随回显持续增长属正常）；`tts` 为播报链路固定键。尺寸不符计数是坏包丢弃的子集，控制线程首次发现时把原因写进 `last_error`（守卫定义见 SPEC-01）。（`pre/src/state.rs` Shared::status、`pre/src/control.rs:75-91`）
- [x] 路由与静音淡变为 10 ms（480 样本）线性增益；direct↔rvc 两个方向对称：先淡出完成再切源，帧内过零保持零增益，下一帧新源从零淡入，消除切换单样本跳变；静音为淡到零并保持。（`pre/src/route.rs:62-78`、`pre/tests/route.rs:234-249`）
- [x] 分发：音频进程按配置消费者列表逐个 Unix socket 发送；一个消费者一个 socket，背压互不影响。`[[pre.consumers]]` 声明 name/socket/file_mode_affected（缺省 false 恒发）；未设置回退 rvc+asr 两支路，socket 沿用 `[rvc]/[asr]` 的 input_socket（rvc 受文件接管影响、asr 不受）。入口校验名字唯一、socket 唯一且纯文件名，"tts" 与含点名为保留键。（`core/src/config.rs` PreConsumerConfig/Config::pre_consumers、`pre/src/ai.rs` Sender、`pre/src/main.rs` process_thread）
- [x] 扬声器仲裁的执行归音频进程（淡变、设备写入）；策略（何时切、切到哪、静音与否）由 daemon 下发。直出、变声、翻译播报共用播放设备。
- [x] 音频进程不做控制决策，不管理其他进程生命周期；只执行 daemon 下发的路由与模式指令。
- [x] 直出分支取在降采样前，用 48 kHz 主干；AI 支路降采样到 16 kHz。
- [x] 数据面过载：非阻塞发送，丢弃计数；对端排队上限由发送方 connect 侧 `SO_SNDBUF` 按 4–8 帧执行——Linux AF_UNIX 数据报流控只按发送方发送缓冲记账，接收方 `SO_RCVBUF` 不约束发送方（bind 侧 `SO_RCVBUF` 保留设置但不参与流控）。（`core/src/dgram.rs:174-182`、`pre/src/ai.rs:26`）
- [x] 数据面 socket 命名与方向：`input_socket`（默认 `rvc-in.sock`）由 rvc 绑定收输入帧；`output_socket`（默认 `rvc-out.sock`）由 pre 绑定收输出帧；配置键在 `[rvc]`，路径相对 run_dir。（`core/src/config.rs`）
- [x] Socket 路径在 `/run/mozart/` 下；对端重启后重连（惰性建链）。（`pre/src/ai.rs`）
- [x] 锁文件防重复实例：`run_dir/pre.lock` 独占锁，与 daemon 的 `daemon.lock` 同一模式（std 文件锁）；配置解析后、socket 绑定与 ALSA 打开前拿锁，拿不到报错退出，锁持有到进程退出。（`pre/src/main.rs`）
- [x] 控制面（daemon → pre）：Unix SEQPACKET 定长头 + JSON。命令 `Hello`（版本握手）、`SetRoute{route}`（切 direct/rvc）、`SetMute{mute}`（静音开关）、`SetFileMode{active}`（文件接管开关：暂停/恢复 rvc AI 发送并交接 output_socket 绑定，asr 不受影响）、`SetDsp{denoise,gate,agc,limiter}`（运行中切换 DSP 级）、`GetStatus`。路由与静音由 pre 执行淡变，策略归 daemon。（`pre/src/control.rs`）
- [x] `SetDsp{denoise,gate,agc,limiter}`（运行中切换 DSP 级）：命令号 0x0207（`core/include/mozart/ctl.h`、`core/src/ctl.rs`），请求体 schema::DspSetRequest 四键必需、拒未知键；pre 校验后写目标原子，处理线程逐帧应用，应答回显四个开关。（`pre/src/control.rs` dispatch、`pre/src/main.rs` process_thread）
- [x] 状态：`GetStatus` 返回运行状态、路由、静音、文件接管状态（file_mode）、采集/播放设备名、采集/播放/丢弃帧计数、队列深度、各消费者丢弃计数、最近错误、版本。daemon 每秒轮询，pre 不主动推；旧 `pre-status.json` 废弃。（`pre/src/state.rs`）
- [x] 修改持久基线（`config set`）仍由 daemon 重启 pre 生效；运行时临时换设备走 `PRE_SET_DEVICE`（`0x0208`）。设备枚举归 daemon（SPEC-04），pre 只打开指定设备、打开失败保留旧设备或启动报错。（`pre/src/main.rs`、`pre/src/control.rs`）
- [x] DSP 开关经 `SetDsp` 运行时生效；denoise 重新启用时重置 WOLA 流式状态（噪声估计、频谱队列、增益回到初始，重新攒初始化帧，首块同启动的 64 样本补零），避免陈旧状态伪影；gate/agc/limiter 直接切布尔。（`dsp/src/lib.rs` Dsp::set_switches、`dsp/src/noise_suppressor.rs` NoiseSuppressor::reset）

## UL-UNAS 可选后端

- [x] 新增 `enhance/` C++ 工作进程，以 ONNX Runtime CPU、单线程运行作者预训练权重；禁止随包复制模型，不依赖 PyTorch。
- [x] 可选 `[enhance]` 配置选定后端；默认不配置，沿用 WOLA。`pre.dsp.denoise` 控制选定后端，UL-UNAS 不与旧 WOLA 叠加。禁用时直接旁路且不调用网络，重新启用或帧不连续时重置会话。
- [x] 48 kHz 直通保留高通及 gate／AGC／limiter，UL-UNAS 仅处理 FIR 后的共享 16 kHz 输入。普通工作线程通过有界队列调用，采集／播放不等待推理。
- [x] 增强进程由 daemon 启停、退避重启并轮询状态；控制命令 Hello=0x0701、GetStatus=0x0702，使用既有 JSON 信封。pre 不生成子进程。
- [x] 音频通过独立 Unix SEQPACKET 会话传输：请求为输入 magic 加 InputFrame；应答为增强 magic、ready 和 InputFrame。首两次 ready=0，此后每请求输出对应两帧前的 320 样本；元数据保留源 frame_idx／pts_ns，不伪造新采集时刻。契约以 core C 头为准。
- [x] 模型使用 512 点周期 Hann、256 点步进和连续缓存。首部反射，固定积攒两帧后输出；结束排空并保留全部样本。算法积攒 640 个 16 kHz 样本，不当作 ADC／DAC 延迟。真实权重对拍固定排除末尾 512 个采样点（离线参考反射边界、流式结束零排空），其余区间误差 SNR ≥ 60 dB；全部首尾另经恒等模型逐样本重建验证。
- [x] 重新计算增强后能量与能量型 VAD，不将模型谱增益冒充语音概率；复用 DSP 的滞回。文件接管的 RVC 门控同时检查源帧采集时和发送时状态，ASR 不受影响。
- [x] 队列溢出、超时、断连、坏应答必须计数并暴露原因；失败不静默回退未增强音频，不重送未知结果。丢帧后重建会话，避免旧缓存跨缺口。
- [x] 单测覆盖 STFT 恒等、跨 20 ms／16 ms 重打包、首尾、时间戳、切换、重连、背压、文件接管和旧后端回归；板端使用 `~/MozartMryia` 独立源码／运行目录验证真实模型、状态与 RVC／ASR 分发。

### 接入审查修正

- [x] 预设提交保留生效 DSP 开关，不吸收此前下发失败的新基线；严格校验 `PRE_SET_DSP` 成功应答回显四键。

- [x] 接通 daemon 的 `config set pre.dsp.*` 到 `PRE_SET_DSP`；成功才替换生效配置。自动重启通过 `--effective-dsp` 保留上次成功开关，失败的新基线不得混入恢复。（`daemon/tests/dsp.rs`）

- [x] 自动重启使用生效的增强配置，不吸收 TOML 中仍待重启的后端或 socket。
- [x] 文件接管应答等待 RVC 在途发送归零，使用发送许可与代次隔离，禁止旧帧跨接管恢复回灌。
- [x] daemon 汇总增强数据面降级；主动关闭不报故障，真实成功帧后清除当前故障，保留丢弃计数。
- [x] 控制连接按统一空闲时限回收；离线输出拒绝与模型同 inode；仅按生产两帧排空规则验证短文件与尾样本。


实现、板端功能证据与整机验收边界见[接入报告](../../reports/ulunas-integration-20261117/RESULTS.md)。

## 改进点

- [x] 禁用降噪时跳过全部计算。（`dsp/src/lib.rs:88-93`）
- [x] 淡变逻辑加重入保护，参数做边界校验。（`pre/src/route.rs:72-75`）
- [x] 拆开旧单体主程序。（pre 与 dsp 分 crate）
- [x] 实时原生多采样率（播放端）：播放采样率固定 48 kHz，不按流协商。`BoundaryResampler` 在播放线程入口把模型原生帧重采样到 48 kHz，只取 `valid_samples` 有效段；`sample_rate` 48000 且 `valid_samples` 960 时直通。覆盖 32k/40k/48k 与运行中切率。主条见 SPEC-03。（`pre/src/playback.rs:85-137,198-209`，单测 `:400-478`）
- [x] `BoundaryResampler` 实时线程约束：栈数组进出，无锁、无阻塞、无分配，播放循环前预创建。采样率变化时以新帧首样本重置插值历史；缺帧与切回 direct 时 reset 清零。（`pre/src/playback.rs:103-136,155,195,205`）
- [x] 播放设备必须给到 48 kHz：`set_rate_near` 返回值不等于 48000 即报错退出，由 daemon 退避重启。设备不支持 48 kHz 时不在播放侧改率。（`pre/src/alsa_stream.rs:46-51`）
- [x] 播报分时取帧与切换淡变：`Speaker::Broadcast` 连续发帧期以 4 ms 微抖动等待吸收 TTS 跨进程唤醒抖动，进入 `holdoff` 空窗与实时态立即退回零等待，由 ALSA 写定拍以防抽干硬件缓冲；`Realtime ↔ Broadcast` 切换做 10 ms（`FADE_SAMPLES`）线性淡变；播报期隔离 `Router` 淡变并重置 `BoundaryResampler` 插值历史。（`pre/src/playback.rs`）
- [x] 音频设备热切换（`PRE_SET_DEVICE` `0x0208`）：采集与播放线程在循环边界支持热替换 ALSA PCM 句柄，先试打开新设备再关旧句柄，失败保留旧设备继续运行并回错；切 `capture` 时清空半帧采集缓冲、触发处理线程重置 WOLA 降噪状态（UL-UNAS 同步随 `capture_epoch` 重置）并拒绝 `capture_file` 模式；切 `playback` 时首帧执行 10 ms 线性淡入消杂音；支持 `--effective-capture-device` / `--effective-playback-device` 供崩溃自愈沿用临时设备；物理拔线仍按 `Fatal` 退出。（`pre/src/state.rs`、`pre/src/control.rs`、`pre/src/capture.rs`、`pre/src/playback.rs`、`pre/tests/set_device.rs`；板端通过 `snd-aloop` 跨子设备通音及 USB 麦打开验证，真实扬声器切口主观听感未验证——阻塞原因：无物理扬声器实听环境，解除条件：接物理扬声器热切 `playback` 并人工试听切口有无爆音）
- [x] 扬声器主模式门控：`PRE_SET_PLAYOUT` 在播放帧边界确认策略。托管播报模式只输出 TTS，空窗补零；托管实时变声与直出模式不播放 TTS。pre 受监管重启时静音待断言，旧命令退出托管后恢复原分时。
- [ ] 实时播放缓冲余量与欠载平滑（衔接 SPEC-03 实时衔接步骤 2）：当前播放 ALSA 硬件缓冲为 `max(period × 4, period + 960) = 1024` 样本（`21.3 ms`），仅比单帧 `960` 样本（`20 ms`）多 `64` 样本（`1.33 ms`）；当 RVC 接收环等待或调度微抖动时极易触发硬件 `EPIPE` underrun（实测 30 s 出现 6 次），且 `rvc.pop` 超时直接写入 20 ms 全零并重置重采样器会产生方波缺口咔嗒声。优化：适度增大播放侧 ALSA 硬件缓冲周期余量（避免单帧写完仅剩 1.33 ms），并在 RVC 接收环短时欠载时对上一帧末尾做短淡出归零，恢复来帧时短淡入。验收：单测覆盖欠载过渡无单样本满幅阶跃；板端 30 s 实时变声 `pre.log` 播放 underrun 计数为 0。
