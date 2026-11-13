# pre：音频前端

状态：讨论中。

## 边界

- 采集：只做设备 I/O 或测试文件注入。
- 预处理：DSP 与降采样到 16 kHz。
- 播放与路由：直出、变声、翻译播报共用扬声器。

## 已确认

- 采集输出原生格式块；DSP 归预处理。
- 采集到 DSP 使用 core 的环形队列。
- 处理阶段顺序沿用旧实现；禁用阶段直接跳过。
- 直出与变声切换用显式状态机；参数入口校验一次。
- DSP 用 Rust 重写，逐阶段与旧输出对拍。第一版覆盖全部阶段：80 Hz 高通、WOLA 谱降噪、gate、AGC、limiter、FIR 3:1 降采样与 VAD；对拍夹具由旧 C 核心离线生成，小型 PCM 放 `tests/fixtures/` 并在 `.gitignore` 白名单。
- DSP 抽成独立共享 crate：pre 实时链路与 daemon 离线文件降噪共用同一实现，不做两套；逐阶段对拍口径不变。
- 采集与播放设备名、数据面 socket 路径等部署配置经 `--config` 读取；daemon 拉起 pre 时传同一配置路径。
- `[pre]` 配置段键：`capture_device`、`playback_device`（默认 `default`）、`capture_queue_frames`（采集→DSP 环容量，默认 8，1..=1024）、`capture_file`（测试采集文件路径，默认不设置）。
- 文件采集源（测试注入）：`capture_file` 设置后，采集线程读 WAV 文件替代 ALSA 采集，播放仍走 ALSA；用于无麦克风环境的离线对拍，下游 DSP 与分发链路不变。文件契约：PCM16、48000 Hz、至少 1 声道，取首声道（与设备采集取 FL 一致）；启动时入口校验一次，不合契约报错退出，由 daemon 退避重启。按实时节拍每 20 ms 发射一帧，文件播完循环；`frame_idx` 与 `pts_ns` 语义与设备采集一致。`GetStatus` 的采集设备名报告文件路径。换文件与换设备一样由重启生效，不做在线命令。
- `[pre.dsp]` 配置段键：`denoise`（WOLA 谱降噪开关，默认 true，沿用旧 main.c 代码默认）、`gate`、`agc`、`limiter`（整形级开关，默认 false，沿用旧 main.c 代码默认）。禁用降噪时该级全部计算跳过：不走旧实现的旁路 WOLA 重建，无切换收敛期、无发射延迟，置信度恒 0；禁用整形级时逐位恒等跳过。
- WOLA 谱降噪流式合同（旧实现实测口径）：960 样本定块首块发射 896、其后每块 960；发射流滞后输入流 64 样本，处理级对首块缺口补零保持 20 ms 帧节拍（旧 dsp.c wrapper 语义）。旧注释的“192 样本发射延迟”指 64 样本 hop 的完整重建跨度（frame 128 + hop 64）。
- 整形级作用于 WOLA 发射样本（旧 dsp.c enhance 次序：NS 发射后、补零前）；参数固定为旧实现出厂默认，无配置键。
- 16 kHz 打包：主干 960 对齐帧经 33 抽头 FIR 3:1 降采样为 320 样本契约帧，FIR 历史跨帧连续；InputFrame 的 meta 时间轴与主干帧同窗。`pts_ns` 取采集时间戳（frame.h 语义）；`frame_idx` 为打包序号（旧 `++frame_idx`，从 1 起；旧 main.c 打包时刻改写 pts 的做法不沿用）。
- AI 支路发送：处理线程对每帧主干恒定发送 InputFrame 到 rvc `input_socket`，不随路由停发（旧 main.c「AI 路径永不停」）。唯一例外是文件接管：`SetFileMode{active:true}` 期间 rvc 支路停发，把 `input_socket` 让给 daemon 的文件数据面（SPEC-03 复用 + 状态门控交接），避免实时帧污染整段攒帧；`active:false` 恢复恒发。asr 识别支路不受文件接管影响，始终恒发。非阻塞发送，过载与对端不在一律丢弃计数；pre 对 input_socket 惰性建链——对端后起不阻塞 pre 启动，未建链期发送丢弃计数，对端就绪后自动接通。
- AI 支路接收：pre 绑定 `output_socket` 收 OutputFrame，应用层接收环 8 帧（4–8 确认范围取上沿）满丢最旧计数；播放线程 rvc 路由取该环、缺帧补零续流，direct 路由逐帧排空该环并按帧计数。文件接管期间接收线程释放 `output_socket` 绑定（daemon 接管同一 socket），交还时重绑恢复接收；释放/重绑经握手标志确认，控制命令在释放完成后才应答（见 SPEC-03「切换时交接绑定」）。
- `consumer_drops` 口径：`rvc` = 输入发送丢弃 + 输出坏包丢弃 + 接收环满丢弃（链路异常计数，恒发下应接近 0）；`rvc.skipped` = direct 路由期间丢弃的输出帧（设计内丢弃，对应旧 `skipped_replies`，direct 下随回显持续增长属正常）。
- 路由与静音淡变为 10 ms（480 样本）线性增益；direct↔rvc 两个方向对称：先淡出完成再切源，帧内过零保持零增益，下一帧新源从零淡入，消除切换单样本跳变；静音为淡到零并保持。
- 分发：音频进程按配置消费者列表逐个 Unix socket 发送；一个消费者一个 socket，背压互不影响。
- 扬声器仲裁的执行归音频进程（淡变、设备写入）；策略（何时切、切到哪、静音与否）由 daemon 下发。直出、变声、翻译播报共用播放设备。
- 音频进程不做控制决策，不管理其他进程生命周期；只执行 daemon 下发的路由与模式指令。
- 直出分支取在降采样前，用 48 kHz 主干；AI 支路降采样到 16 kHz。
- 数据面过载：非阻塞发送，丢弃计数；对端排队上限由发送方 connect 侧 `SO_SNDBUF` 按 4–8 帧执行——Linux AF_UNIX 数据报流控只按发送方发送缓冲记账，接收方 `SO_RCVBUF` 不约束发送方（bind 侧 `SO_RCVBUF` 保留设置但不参与流控）。
- 数据面 socket 命名与方向：`input_socket`（默认 `rvc-in.sock`）由 rvc 绑定收输入帧；`output_socket`（默认 `rvc-out.sock`）由 pre 绑定收输出帧；配置键在 `[rvc]`，路径相对 run_dir。
- Socket 路径在 `/run/mozart/` 下；锁文件防重复实例；对端重启后重连。
- 控制面（daemon → pre）：Unix SEQPACKET 定长头 + JSON。命令 `Hello`（版本握手）、`SetRoute{route}`（切 direct/rvc）、`SetMute{mute}`（静音开关）、`SetDsp{denoise,gate,agc,limiter}`（运行中切换 DSP 级）、`SetFileMode{active}`（文件接管开关：暂停/恢复 rvc AI 发送并交接 output_socket 绑定，asr 不受影响）、`GetStatus`。路由与静音由 pre 执行淡变，策略归 daemon。
- 状态：`GetStatus` 返回运行状态、路由、静音、文件接管状态（file_mode）、采集/播放设备名、采集/播放/丢弃帧计数、队列深度、各消费者丢弃计数、最近错误、版本。daemon 每秒轮询，pre 不主动推；旧 `pre-status.json` 废弃。
- 换设备不做在线命令，由 daemon 重启 pre 代替。设备枚举归 daemon（SPEC-04），pre 只打开配置的设备、打开失败报错。
- DSP 开关经 `SetDsp` 运行时生效；denoise 运行中切换需处理 WOLA 滤波状态避免伪影，gate/agc/limiter 较简单。

## 改进点

- 禁用降噪时跳过全部计算。
- 淡变逻辑加重入保护，参数做边界校验。
- 拆开旧单体主程序。
