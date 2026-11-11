# realtime pipeline 端到端实测：Jetson

日期：2026-11-12。对象：Task 1–6 交付的三进程实时链（daemon + pre + rvc C++）。
设备：Jetson Orin Nano Super 8 GB，JetPack 6，qiqi-zh 模型（TensorRT engine）。
验收口径遵循 [TARGET.md](../../TARGET.md)：延迟以物理回录计时为准，软件时间戳只作拆解。

## 测量方法与配置

- 拓扑（snd-aloop，SUBDEV 0/1 被 root 遗留进程占用，用空闲的 2/3）：
  `aplay 测试信号 → Loopback DEV0,SUB2 → pre 采集 DEV1,SUB2；pre 播放 DEV0,SUB3 → arecord DEV1,SUB3 回录`。
- 配置：`tools/jetson-measure.sh` 生成（键值与 `tools/jetson-smoke.sh` 一致），
  `denoise = true`，`f0_method = "rmvpe"`，模型 `models_dir = ~/Mozart-Old/rvc-golden/qiqi-zh-run`。
- 回录链与延迟场景共用 aplay/arecord 参数（4096 帧缓冲、1024 帧周期，48 kHz S16 双声道）。
- 地板校准（无 pre 直连回录）：504.29 ms，其中 500.0 ms 为 arecord 提前 0.5 s 启动的坐标偏移，
  净通路延迟 4.3 ms。三次运行 504.29/508.29/504.29 ms，跨运行存在 192 样本（4 ms）对齐量子；
  单次运行内 6 个点击样本级一致（零抖动）。净延迟 = 实测滞后 − 地板，含 ±4 ms 对齐量子。
- 每场景回录起点为 aplay 启动前 0.5 s；本文回录时间均按该坐标约定换算。
- 证据目录：Jetson 本机 `~/mozart-t7/`，各场景一个带时刻后缀的目录，正文按文件名引用。

## 端到端延迟（物理回录）

脉冲串法：0.3 s、220 Hz、幅度 0.73 满幅的短脉冲，回录起振沿对信号内脉冲起点。
统计范围：direct 首段 8 个、rvc 冷激活段 5 个、切回 direct 段 2 个、rvc 热切换段 3 个（`latency-1794428614559`）。

| 路径 | 净延迟 | 样本 |
| --- | --- | --- |
| direct（首段） | 44.67 ms，8/8 全一致 | 8 |
| direct（rvc 往返后） | 64.67 ms，2/2 全一致 | 2 |
| rvc（冷激活后稳态） | 493.5–507.5 ms（均值 498.3） | 5 |
| rvc（热切换后稳态） | 500.3–505.2 ms（均值 502.8） | 3 |

结论：

- direct 净延迟约 45 ms，由 pre 采集/播放 ALSA 缓冲与 DSP 流水构成；rvc 往返后增加 20 ms（一帧），可复现。
- rvc 稳态净滞后约 0.5 s，与 SPEC-03 的 400 ms 启动缓冲加块化/排队构成一致，属设计值而非缺陷。
- 路由切换命令耗时（`switches.jsonl`）：切 rvc 冷 698 ms（含 LoadModel + StartStream），
  切 direct 20 ms，rvc 热切换 8 ms。
- 热切换后新流的第一个有声窗会回放切换前约 0.2 s 的音频残尾（实测滞后 1.26 s 一次），
  随后进入稳态节拍，无累积错位。

## 连续性（60 s 长跑）

连续 220 Hz 正弦经 rvc 路由 60 s（`longrun-1794428672211`），status 每 2 s 轮询。

- 1:1 泵：稳态窗口 [6.0, 62.3] s 内 pre 采集速率 49.86 帧/s，rvc 输入速率 49.86 帧/s，帧数守恒。
- 丢弃：pre `frames_dropped` 0、rvc `frames_dropped` 0、稳态窗口 `consumer_drops.rvc` 增量 0
  （启动期累计 288 帧，为 rvc 冷激活前发送缓冲溢出，激活后不再增长）。
- 欠载：pre 播放 underrun 全程 1 次（启动抖动），无累积（`st-final.json` 的 `last_error`）。
- 回录掉帧：包络检测 20 ms 以上凹陷为零，仅启动 1.0 s 与收尾 1.76 s 的预期静默。
- 队列深度：pre 队列最大 0，rvc 队列最大 16 帧（块化前蓄积，符合 240 ms 块节拍）。

## 切换淡变（无爆音检查）

连续音下 direct↔rvc 往返切换（`fade-1794428638476`），相邻样本最大跳变为判据。
direct 稳态理论跳变上限 691（220 Hz、幅度 24000）。

| 段 | 相邻样本最大跳变 |
| --- | --- |
| direct 稳态（两段） | 686（贴近理论值） |
| rvc 稳态（两段） | 2628–2847（变换后谐波内容自身斜率） |
| direct→rvc 切换窗 | 686（与稳态持平，无爆音） |
| rvc→direct 切换窗 | 4494（单样本阶跃后 10 ms 淡入） |

结论：direct→rvc 两个方向（冷、热）均无切换爆音；rvc→direct 依 SPEC-02
"立即切源后淡入"的设计，存在一次受 rvc 输出幅度约束的单样本阶跃（4494，约 14% 满幅），
后接 10 ms 线性淡入。若要求绝对无爆音，需把淡入起点改为当前输出值（改进建议，非实现缺陷）。

## kill -9 自愈

### rvc（`heal-1794428782177`）

- kill -9 后 0.92 s daemon 重启 rvc（新 pid，`restarts = 1`）。
- 回录静默 2.93 s 后恢复出声；3.46 s 状态回 `streaming`。
- 路由保持 rvc；重启后 rvc 帧守恒 1051/1051/0。
- pre `consumer_drops.rvc` 在 rvc 死亡期间累计 317（发送被拒计入），恢复后冻结，符合 SPEC-02 口径。

### pre（`heal-1794429254690`）

- kill -9 后 0.6 s daemon 重启 pre，路由 0.6 s 恢复 rvc。
- snd-aloop 拓扑约束：pre 死亡断开采集缆，写端存活时旧缆对象不释放，重开的 pre 永远采到静默
  （取证运行 `heal-1794428819464`：整条 IPC 链恢复、1:1 泵维持，但 rvc 129 窗全部静默跳过）。
  测量脚本因此在 kill 后立即退掉旧写端（两端齐闭释放缆对象），待 pre-2 就绪后重供信号。
- 信号重供后 533.9 ms 恢复出声，静默单段 2644.9 ms（含上述拓扑必需的重启与重供时间），
  恢复后连续出声到运行结束。
- 重启后 rvc 帧 1888/1888/1，pre 帧 1559/1559/0；rvc 检测 frame_idx 跳变完成全量重锚
  （`resets = 1`，`streaming_pipeline.cpp:22` 的缺口阈值 4）。
- 双向重连均实测成立：pre→rvc 惰性建链（`ai.rs`）、rvc→pre 发送失败重连
  （`unix_dgram_stream.cpp` 的 `try_reconnect`）。

## 模型加载、推理耗时与 GPU

- 进程启动加载实时 HuBERT/RMVPE 引擎约 0.8 s；`LoadModel` 加载生成器三引擎并预热约 0.66 s；
  自愈重启全程 2.93 s（重启 0.92 s + 引擎重载 + 启动缓冲）。
- 推理耗时（60 s 长跑，261 块）：均值 81.1 ms/块，p95 最大 87.0 ms；240 ms 块预算余量约 3 倍。
- GPU 负载（status `hw` 字段，2 s 采样）：均值 32.2%，与推理占空比 81.1/240 = 33.8% 吻合；
  单样本呈 0/满载交替，是 sysfs 负载窗与轮询周期错拍的采样伪影，非真实负载摆动。
- GPU 内存 2589.0–2627.6 MiB，全程波动 39 MiB，无增长趋势（无泄漏迹象）。

## 真语音两路回录（espeak-ng）

espeak-ng 中文合成语音 10 s（确定性语音源），direct 与 rvc 各录一段（`voice-1794428745158`）。

- 连续性：两段活动跨度一致（9.63 s 与 9.62 s），段内低能量时长均为 0（无掉帧、无断句丢失）。
- 变声证据：direct 段频谱以 220/800 Hz 为主（Goertzel 8.97M/7.14M）；
  rvc 段 1000 Hz 成分跃升为主峰（12.0M），220 Hz 衰减至 2.69M，频谱整体上移，音色改变可测。
- 两段 RMS 相当（2997 对 3073），电平未被压垮。

## 遗留项处理结果

| 遗留项 | 处理结果 |
| --- | --- |
| TRT 引擎同机重建复测（Task 5） | 未执行。阻塞原因：仓库内唯一构建脚本 `Mozart-Old/rvc-golden/build_dynamic_engines.py` 只覆盖文件转换路径三资产（qiqi-dynamic-explicit/rmvpe-file/hubert-file），实时链加载的五个引擎（hubert-realtime、rmvpe-realtime、qiqi-zh、qiqi-zh-front、qiqi-zh-decoder）的原始 trtexec 参数未入库，且大引擎同机构建需小时级并需先备份金标资产（约 1.3 GB）。当前保留 golden 引擎与跨设备警告，实测 81.1 ms/块达标、无死锁。解除条件：补齐实时引擎构建脚本后同机重建，并用 Task 5 探针复测六命令与耗时基线。 |
| 真语音段 smoke（Task 4） | 已完成（见上节）。espeak-ng 确定性语音，direct/rvc 两路回录，频谱迁移与零掉帧为客观证据。 |
| underrun 长跑观察（Task 6） | 已完成。60 s 全程信号下 pre 播放 underrun 仅启动 1 次、无累积；信号提前结束才会累积（Task 6 结论），稳态无此现象。 |
| 冒烟脚本退出码（Task 6） | 已完成。`tools/jetson-smoke.sh` 失败路径齐备（ready 超时、pid 解析失败、进程残留、socket 仍可连接均 `exit 1`），本次实跑 `exit 0` 验证通过。 |

## SPEC-02 已确认清单对照

验证方式缩写：实测 = 本任务回录/status 数据；单测 = 仓库测试套件；对拍 = 旧实现夹具对拍；审查 = 任务期代码审查。

| SPEC-02 条目 | 实现位置 | 验证方式 |
| --- | --- | --- |
| 采集输出原生格式块；DSP 归预处理 | `pre/src/capture.rs`、`pre/src/dsp/mod.rs` | 单测 + 实测（各场景回录非零） |
| 采集到 DSP 使用 core 环形队列 | `core/src/ring.rs:11`、pre 主循环接线 | 单测；`capture_queue_frames = 8` 实测生效 |
| 处理阶段顺序沿用旧实现；禁用跳过 | `pre/src/dsp/mod.rs:3`（阶段次序）、`Dsp` 按开关跳过 | 对拍（Task 3/4 全阶段夹具） |
| 显式状态机；入口校验一次 | `pre/src/route.rs`、`core/src/config.rs:313` | 单测 + 实测切换时间线 |
| DSP Rust 重写逐阶段对拍 | `pre/src/dsp/*.rs`、`tests/fixtures/` | 对拍（80 Hz/WOLA/gate/AGC/limiter/FIR/VAD） |
| 部署配置经 `--config`，daemon 转交 | `daemon/src/supervisor.rs`（start_child） | 单测（argv 断言）+ 实测全部场景 |
| `[pre]` 配置段键 | `core/src/config.rs:40`、`:313` | 单测（越界拒绝 `config.rs:471`）+ 实测 |
| `[pre.dsp]` 配置段键 | `core/src/config.rs:57` | 单测 + 实测 `denoise = true` |
| WOLA 流式合同（896/960、64 滞后） | `pre/src/dsp/noise_suppressor.rs` | 对拍（Task 3 夹具逐块一致） |
| 整形级作用于 WOLA 发射样本 | `pre/src/dsp/stages.rs` | 对拍（Task 4 limiter 热输入） |
| 16 kHz 打包（33 抽头 FIR 3:1） | `pre/src/dsp/decimator.rs` | 对拍（跨帧历史连续） |
| AI 恒发 + 惰性建链 | `pre/src/ai.rs:44` | 单测 + 实测（heal 后自动接通） |
| 接收环 8 帧、满丢最旧、缺帧补零 | `pre/src/ai.rs:18`、`recv_loop:79` | 单测 + 实测（长跑零掉帧） |
| `consumer_drops` 口径 | `pre/src/ai.rs`、`core/src/ctl/schema.rs:129` | 实测（cdrop 317 冻结、skipped 仅 direct 期增长） |
| 10 ms 线性淡变与切源次序 | `pre/src/route.rs:15` | 实测（fade 场景跳变判据） |
| 分发按消费者列表 | `pre/src/ai.rs` | 单测；v1 实装单消费者 rvc，列表扩展随翻译播报 |
| 仲裁执行归 pre、策略归 daemon | `pre/src/route.rs`、`daemon/src/state.rs:100` | 实测（route desired/active 分离、arbiter.gpu.owner） |
| pre 不做控制决策 | `pre/src/control.rs:110` | 审查 + 实测（命令只读原子执行） |
| 直出取 48 kHz 主干 | `pre/src/route.rs` | 实测（direct 段纯音、无降采样镜像） |
| 过载非阻塞丢弃；接收缓冲 4–8 帧 | `core/src/dgram.rs:201`（丢弃计数）、`:149`（缓冲） | 单测（EAGAIN/ENOENT/ECONNREFUSED）+ 实测零异常丢弃 |
| socket 命名与方向 | `core/src/config.rs:93`、`rvc/transport/unix_dgram_stream.cpp:150` | 单测（线包对拍）+ 实测数据面 |
| 路径、锁文件、对端重启重连 | `daemon/src/server.rs:71`（daemon.lock）、`core/src/dgram.rs:228`、`unix_dgram_stream.cpp:274` | 实测（heal 双向重连）；`/run/mozart/` 生产布局见适用范围 |
| 控制面 SEQPACKET 定长头 + JSON | `core/src/ctl.rs:28`（命令号）、`:74`（头）、`pre/src/control.rs:110` | 单测 + 实测（ctl 全程） |
| GetStatus 字段与每秒轮询 | `core/src/ctl/schema.rs:119`、`daemon/src/poller.rs:25` | 实测（status.jsonl 逐字段；轮询间隔按测量脚本设定采样） |
| 换设备由 daemon 重启 pre | `daemon/src/supervisor.rs:67` | 审查（无在线设备命令）+ 实测（supervisor 重启路径） |

## SPEC-03 已确认清单对照

| SPEC-03 条目 | 实现位置 | 验证方式 |
| --- | --- | --- |
| 推理核心独立进程 | `rvc/CMakeLists.txt`（mozart-rvc） | 实测（三进程拓扑） |
| 非推理逻辑归 Rust | `daemon/`、`ctl/` | 审查 + 实测 |
| C++ 只保留推理（StreamSession 封装） | `rvc/control/stream_session.cpp:150/229` | 六命令对拍（Task 5）+ 实测 |
| 静音和旁路归音频进程 | `pre/src/route.rs`；rvc 无此概念 | 审查 + 实测（淡变在 pre 侧可测） |
| 参数 StartStream 固定；SetParams 推迟 | `rvc/control/stream_session.cpp:163` | 六命令对拍；命令表无 SetParams（`core/src/ctl.rs:33`） |
| 1:1 泵 + 400 ms 启动缓冲 | `rvc/src/rvc/audio_worker.cpp:71`、`streaming_pipeline.cpp:62` | 实测（49.86 帧/s 守恒、rvc 净滞后约 0.5 s） |
| 1:1 泵时钟同源假设；漂移补偿推迟 | `streaming_pipeline.cpp`（泵） | 实测（同源 60 s 零累积）；异源场景见适用范围 |

## 适用范围

- 配置：Jetson Orin Nano Super 8 GB、JetPack 6、TensorRT 10（libnvinfer）、qiqi-zh 单模型
  （golden 引擎，跨设备构建警告存在，见遗留项）；snd-aloop SUBDEV 2/3；48 kHz S16 双声道；
  `denoise = true`，其余 DSP 整形级关闭。
- 样本数：地板 3 次 × 6 点击；延迟 18 脉冲；长跑 60 s 单轮；fade 21 s 单轮；
  语音 10 s × 2 段；自愈各 1 轮（pre 另有 2 轮取证迭代）；冒烟 1 轮。
- 统计范围：净延迟为脉冲回录起振沿对信号起点、扣地板；推理耗时为 status 报告口径（块粒度）；
  GPU 为 sysfs 采样（含错拍伪影说明）。
- 未验证项 1：真实麦克风/扬声器声学验收。阻塞原因：无 USB 麦克风与扬声器；
  解除条件：插麦后按本报告同流程复测（延迟、连续性、语音质量、变声主观听感）。
- 未验证项 2：TRT 引擎同机重建后的性能复测（见遗留项表）。
- 未验证项 3：异源声卡（不同 capture/playback 时钟）下的 1:1 泵长期漂移。阻塞原因：单板只有
  snd-aloop 同源时钟可测；解除条件：接 USB 声卡后长跑观察欠载/缓冲增长，必要时按 FUTURE 补偿。
- 未验证项 4：`/run/mozart/` 生产运行目录布局与 systemd 部署。阻塞原因：部署任务未启动，
  本次 run_dir 为测量目录；解除条件：部署接线任务按 SPEC-04 验证锁文件与 socket 路径。
- 拓扑限制：snd-aloop 采集端随 pre 进程死亡断缆且不可自愈（写端存活时旧缆不释放），
  heal-pre 须退写端、重开、再重供信号；真实声卡无此耦合。此为测量拓扑属性，非产品缺陷。
