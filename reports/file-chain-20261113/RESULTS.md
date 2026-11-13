# 文件转换链路板端实测

日期：2026-11-13。设备：Jetson Orin Nano Super 8 GB。分支：`file-chain`。

## 结论

文件转换链路在 3 s 至 295 s 样本上板端验收通过（实测）。
提交、取回、取消、GPU 互斥、重启续转、入口拒绝、时长上限均按 SPEC-04 生效。

近时长上限的 295 s 长文件曾存在数据面卡死缺陷：推理已完成但任务永不结束。
根因已定位并修复（文件输入 socket 与实时共享，pre 恒发实时帧污染整段攒帧），
板端复测 295 s 与 120 s 均 completed、`frames_in == frames_out`、时长一致（实测）。
SPEC-08「长文件不会耗尽内存」的内存与完成部分均通过。详见「长文件数据面卡死（已修复）」。

修复方案已演进：本报告记录的初版修复采用独立文件 socket（D2），后经用户否决
（偏离 SPEC-03 字面交接）回退；现行实现为 Task 9 状态门控交接（`PRE_SET_FILE_MODE`
令 pre 暂停 rvc 发送并释放输出 socket，daemon 复用同一对 socket），见
「Task 9 状态门控交接（取代 D2）」与 `docs/plans/file-chain.md` D2。下文 D2 相关
数值为该历史方案实测，现行方案复测见 Task 9 一节。

## 配置

| 项 | 值 |
| --- | --- |
| 设备 | Jetson Orin Nano Super 8 GB，aarch64，Linux 6.8.12-1021-tegra，6 核，7546 MiB 内存 |
| 二进制 | `target/debug`（cargo debug + cmake `jetson-release` 的 rvc 对齐复制） |
| 模型 | `qiqi-zh`，`MODELS_DIR` 为设备本地 `rvc-golden/qiqi-zh-run` |
| 推理 | `device = cuda`，`half = false`，`f0_method = rmvpe`，`cache_limit = 2` |
| 引擎 | TensorRT（hubert/rmvpe/generator 均命中 `.engine`），rmvpe 另有 ONNX 回退 |
| 文件段 | `denoise = false`，`max_duration_seconds = 300`，`queue_capacity = 50`，`storage_limit_bytes = 1000 MiB`，`ffmpeg_timeout_ms = 120000` |
| 数据面 | `output_socket = rvc-file-out.sock`，缓冲 8 帧；`net.unix.max_dgram_qlen = 512` |
| 音频拓扑 | snd-aloop Loopback，`CAP_SUB = 2`、`PB_SUB = 3`，48 kHz；`[pre.dsp] denoise = true` |
| 工具 | ffmpeg n8.0.1，ffprobe 同版本，python3 + numpy 1.26.4 |
| 控制超时 | `rpc_timeout_ms = 2000`，`start_timeout_ms = 30000`，poller 1 s |

受管存储目录落在测试产物目录内（`storage_dir` 指向 `SMOKE_DIR/files`），不污染用户状态目录。

## 部署与测试门

`tools/deploy-jetson.sh --smoke` 共尝试 4 次，第 4 次成功，耗时 133 s（实测）。

| 次 | 结果 | 原因与处置 |
| --- | --- | --- |
| 1 | 回滚 | ctest `rvc_transport` 30 s 超时。随后 5 次复跑全绿（手动行缓冲 1 次、ctest 单测 3 次、全套件 1 次），单次 0.21 s，全套件 7/7 通过 1.67 s。判定为构建后负载下的偶发停顿，非代码缺陷 |
| 2、3 | 回滚 | cmake configure 的 spdlog `FetchContent` update 步骤联网 `git fetch` 撞 TLS 中断。设备无代理配置，上游可达，缓存源码正在 pinned tag v1.15.1（`f355b3d`）。系统 spdlog 为 1.12，低于 `find_package` 要求的 1.14，故必走 FetchContent |
| 4 | 成功 | 部署脚本改为 `cmake --preset jetson-release -DFETCHCONTENT_UPDATES_DISCONNECTED=ON`，跳过已填充依赖的 update 步骤。首次填充仍照常下载，构建产物不变，未改动任何仓库地址 |

第 4 次测试门：`cargo test --workspace` 全绿；ctest 7/7 通过；三进程冒烟通过
（3 s 就绪，`route set rvc` 生效，`kill -9` rvc 后崩溃自愈并重放 LoadModel/StartStream，
关停后无进程残留、控制 socket 全部失效）。

## 场景结果

权威轮为第 6 轮（`LONG_BUDGET_S=120`），10 PASS / 0 FAIL（实测）。
脚本 `tools/jetson-file-test.sh`，证据目录 `/tmp/mozart-file-test`。

| 场景 | 结果 | 判据与证据 |
| --- | --- | --- |
| a 模型清单 | PASS | `model list` 中 `qiqi-zh` 的 `exists=true`；`models.json` |
| b 短 WAV 转换取回 | PASS | 3 s WAV → completed → fetch；WAV/48 kHz/单声道，时长 3.000 s，RMS 0.11607；`out-b.wav`、`stats-b.json` |
| c 压缩文件解码转换 | PASS | 真实录音截取 20 s m4a（44.1 kHz 立体声 AAC）→ completed → fetch；时长 20.020 s，RMS 0.08209；`out-c.wav`、`stats-c.json` |
| e GPU 互斥与进度细化 | PASS | 任务 processing 中 `route set rvc`：`desired=rvc`、`active=direct`、`gpu.owner=file`；任务完成后 `active=rvc`、`gpu.owner=realtime`；`st-e-defer.json`、`st-e-applied.json` |
| d 取消与临时清理 | PASS | 60 s 任务 infer 阶段 percent=40 时取消 → cancelled；受管目录无该任务任何残留（源副本、`decoded.f32`、`output.f32`、`.wav.part` 全清）；GPU 释放；`cancel-d.json`、`storage-after-cancel.txt` |
| f 重启续转 | PASS | 落盘 `processing` → down → up → 重启瞬间落盘 `queued` → completed；daemon 退出 257 ms；`queue-f-down.json`、`queue-f-up.json`、`out-f.wav` |
| g 入口拒绝 | PASS | 不存在模型与坏扩展名均退出码 4，原因含 `model not found` / `unsupported file extension`，坏扩展名未留副本；`reject-model.err`、`reject-ext.err` |
| h 时长上限 | PASS | 3453 s 整段录音 → failed，原因 `audio exceeds duration limit (300 s)`，GPU 释放；`job-h.json` |
| i 近上限长文件（120 s） | PASS | 120 s → completed，时长 120.000 s，RMS 0.11964，GPU 释放；`out-i.wav`、`stats-i.json` |
| z 残留检查 | PASS | 无 mozart 进程，6 个控制 socket 全部不存在或失效 |

前 5 轮的失败均为脚本时序问题，已修正，非产品缺陷：

- 第 1 轮 f/g/h 失败：取消后未等 rvc 控制面恢复就继续提交。daemon 按 SPEC-04 把
  StopStream 超时记降级并立即落 cancelled，而 rvc 单线程控制循环要等整段推理的
  下一个取消检查点才退出，这期间 ListModels 不可达。脚本改为显式等待并测量恢复窗口。
- 第 2 轮 f 失败：`down` 应答后瞬时 `pgrep` 判残留。`daemon/src/server.rs` 的
  Shutdown 先回 ok，再 join 监管线程/poller/文件 worker 并删 socket（代码确认）。
  脚本改为有界等待并测量退出耗时。

## 客观测量

权威轮实测值。计时与统计口径见表后说明。

| 任务 | 输入时长 | 输出时长 | RMS | 峰值 | blocks | inference_ms | infer_wall | daemon VmHWM | rvc VmHWM |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| b 3 s WAV | 3.000 s | 3.000 s | 0.11607 | 0.34362 | 4 | 3296.33 ms | 3.341 s | 5.1 MiB | 2906.2 MiB |
| c 20 s m4a | 20.016 s | 20.020 s | 0.08209 | 0.78150 | 22 | 10146.77 ms | 10.038 s | 9.5 MiB | 2974.4 MiB |
| e 30 s WAV | 30.000 s | 30.000 s | 0.11900 | 0.37019 | 31 | 14514.71 ms | 14.602 s | 12.2 MiB | 3037.0 MiB |
| f 30 s 续转 | 30.000 s | 30.000 s | 0.12628 | 0.37690 | 31 | 15763.65 ms | 15.613 s | 12.2 MiB | 3042.5 MiB |
| i 120 s WAV | 120.000 s | 120.000 s | 0.11964 | 0.38096 | 121 | 55285.77 ms | 55.647 s | 41.1 MiB | 3606.8 MiB |
| d 60 s 取消 | 60.000 s | 无结果 | — | — | 0 | — | — | 11.9 MiB | 3124.7 MiB |

所有 completed 任务 `frames_in == frames_out`，`frames_dropped = 0`（实测）。
输出容器均为 WAV、`pcm_f32le`、48000 Hz、单声道（实测）。

关键时间窗（实测）：

| 量 | 值 | 起止 |
| --- | --- | --- |
| b submit→completed | 7 s | 提交返回至任务终态 |
| c submit→completed | 15 s | 同上 |
| e processing→completed | 16 s | 进入 processing 至终态 |
| i submit→completed | 61 s | 提交返回至终态 |
| d cancel→cancelled | 5 s | 发出 cancel 至落 cancelled |
| d 控制面恢复 | 4 s | cancel 后至 ListModels 可应答 |
| f daemon 退出 | 257 ms | down 应答后至进程消失 |
| f down→up→completed | 24 s | 含重启与就绪等待 |

系统级已用内存峰值（daemon `hw.memory.used_bytes`，1 s 采样取最大）：
b 3726.5 MiB、c 3717.1 MiB、e 3700.4 MiB、f 3787.4 MiB、i 4257.4 MiB。

### 计时与统计口径

- `inference_ms`：rvc `GetStatus` 的 `inference_ms_last`，C++ 侧 `steady_clock`
  计 `process_cancellable` 整段墙钟，含整段 F0。文件模式下 last/avg/p95 同值，
  是单次整段测量，不是多窗统计（代码确认）。
- `infer_wall`：逐秒采样中首次 `stage=infer` 到首次 `stage=publish` 的时刻差，
  1 s 采样粒度，故有 ±1 s 不确定度。它含回帧与落盘，比 `inference_ms` 略大。
- `blocks`：rvc `offline_window_count`，整段推理的窗计数，批处理开始清零。
- `VmHWM`：`/proc/<pid>/status` 的进程生命周期峰值 RSS，**不随任务重置**。
  表中各值是该任务采样窗内读到的峰值，跨任务单调不减，不能当作该任务的独立增量。
  daemon 与 rvc 分别取各自 pid。
- RMS 与峰值：ffmpeg 把结果整段解到 48 kHz 单声道 f32 后用 numpy 计算，
  统计范围是全部解码样本，非窗口抽样。RMS 为线性值（非 dB），峰值为绝对值最大。
- 时长：输出取解码样本数 / 48000；输入 m4a 取 ffprobe 容器时长。
  c 的 0.004 s 差来自 20 ms 帧边界补零。
- 归属过滤：rvc 派生指标只取 `stream_kind=file` 或 `stage` 已到 infer/publish 的采样。
  GetStatus 在无流时仍报上一条流的最终计数，不过滤会把上一任务的值算到本任务头上
  （实测：h 在 decode 阶段即失败，全程无流，却带着上一任务的 blocks 与 inference_ms）。

## 长文件数据面卡死（已修复）

295 s 样本曾永不结束（第 4 轮，`LONG_BUDGET_S=295`，观察超过 400 s 仍 processing，实测）。
根因已定位并修复，板端复测 295 s 与 120 s 均 completed（实测，见下「修复与复测」）。

### 根因（线程栈 + 代码 + 计数三方印证）

卡死态用 `sudo gdb -p <pid> -batch -ex 'thread apply all bt'` 抓栈（抓栈前不杀进程，
证据留设备 `/tmp/mozart-hang-repro/`）。三个关键线程：

- daemon 文件 worker 线程（`mozart-file-job`）阻塞在**喂帧**：
  `feed`（worker.rs:155）→ `send_blocking_with` → `send_wait_writable` → `wait_writable`
  → `poll(events=POLLOUT)`（dgram.rs:412）。即输入 socket 缓冲满、等可写，从未进入 `receive`。
- rvc FileWorker 批处理线程阻塞在**回帧**：
  `batch_loop` → `WriteFrame` → `send_blocking` → `poll(POLLOUT)`。即输出 socket 缓冲满 8 帧、等可写。
- rvc 控制 serve 线程**空闲**在 `serve` → `CtlConnection::recv` → `poll`，未卡在任何锁或 `handle()`；
  daemon poller 线程在 `hrtimer_nanosleep`（轮询间隔睡眠）。故控制面 RPC 阻塞的假设被否定。

根因是**文件输入 socket 与实时输入 socket 共享 `rvc-in.sock`**：

1. SPEC-02 要求 pre「恒定发送 InputFrame 到 rvc `input_socket`，不随路由停发」
   （`pre/src/main.rs:108` 无条件 `ai_sender.send`，代码确认）。文件期间 route=direct，pre 仍向 `rvc-in.sock` 发实时帧。
2. rvc `FileWorker::batch_loop`（file_worker.cpp:70-78）把 `rvc-in.sock` 上**每条**输入帧都计入 `total_frames`，
   无法区分文件帧与实时帧。
3. 攒满 `total_frames=14750`（文件帧 + 实时帧混合）后 rvc 停读输入、转推理；
   daemon 剩余文件帧（被实时帧挤占名额）送不出，阻塞在 feed 的 POLLOUT，永不排空输出。
4. rvc 推理完成回帧，输出缓冲满 8 帧（daemon 不排空）→ 阻塞在 WriteFrame 的 POLLOUT。两侧互等死锁。

实证计数：卡死态 pre `frames_captured=853`、`consumer_drops.rvc=829` → 成功送达 24 帧实时帧进 `rvc-in.sock`，
被 FileWorker 计入 `total_frames`，正是 daemon 喂帧缺口（进度冻结 `feed 38%`）。
8 帧 == `QUEUE_FRAMES`；内存未耗尽（rvc VmHWM 4637.5 MiB、系统 available 2372 MiB），排除 OOM。
120 s 通过、295 s 卡死：喂帧窗越长，实时帧混入攒帧概率越高，≥1 帧混入即死锁，阈值随时长上升。
旧观测「两侧接收队列均 0」不构成反证：AF_UNIX DGRAM 流控按发送方 `SO_SNDBUF` 记账，
`ss` 的 dgram Recv-Q 不反映发送方 wmem；栈已直接证明两侧分别阻塞在输入/输出 POLLOUT。

### 修复（最小，对称于 D2）

> 已取代：本节描述的独立文件 socket（D2）方案已回退，现行实现为 Task 9 状态门控
> 交接（见「Task 9 状态门控交接（取代 D2）」）。本节保留作历史根因与初版修复记录。

文件输入隔离到独立 `rvc-file-in.sock`，pre 实时路径完全不动（满足 SPEC-02 恒发），
整段批处理 / 无丢帧 / StopStream 语义不变（SPEC-03），daemon 中间层不变（SPEC-04）。
与 D2 已隔离的文件输出 socket 同一模式，补上输入侧：

- `[file]` 增 `input_socket`（默认 `rvc-file-in.sock`）与 `file_input_socket()` getter、入口校验。
- `StartStream` kind=file 增可选 `input_socket`（绝对路径，入口校验一次，对称 `output_socket`）；
  rvc `start_worker` 文件流绑定该输入 socket。
- daemon `feed` 改连 `file_input_socket()`；`rvc_stub` honor 覆盖。

D2 延伸待确认：D2（独立文件输出 socket）是计划文档标注「偏离 SPEC 字面，待确认」的设计决策；
输入侧隔离同性质、同理由（pre 终身持有实时 socket，交接需改已验收实时路径），仍满足 SPEC 核心意图
（C++ 对端无感知、文件不丢、实时/文件互斥）。此延伸与 D2 一样待用户确认；
若坚持 SPEC 字面输入 socket 交接，需改 pre 路由感知绑定（后续局部改动）。

主机回归 `daemon/tests/file_worker.rs::realtime_flood_on_shared_input_does_not_wedge_file_job`：
模拟 pre 恒发线程灌共享 `rvc-in.sock` + 20 s 文件任务。旧代码（共享输入 socket）卡死
`stage=feed percent=29` 超时 FAILED；新代码（独立输入 socket）completed、`frames_in==frames_out==1000`。
`cargo test --workspace` 全绿，ctest 7/7。

### 修复与复测（实测）

`tools/deploy-jetson.sh` 部署成功（129 s），设备测试门 cargo workspace 全绿、ctest 7/7。
验证脚本 `/tmp/mozart-fix-verify.sh`（拓扑同本测试，`[file]` 增 `input_socket`），证据 `/tmp/mozart-fix-verify/`。

| 任务 | 终态 | frames_in | frames_out | dropped | blocks | inference_ms | 输出时长 | RMS | submit→completed 墙钟 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 295 s | completed | 14750 | 14750 | 0 | 296 | 136845 ms | 295.000 s | 0.11597 | 144 s |
| 120 s | completed | 6000 | 6000 | 0 | 121 | 55269 ms | 120.000 s | 0.11600 | 59 s |

两任务 `result_ready=true`、`stage=publish percent=100`、`frames_in==frames_out`、`frames_dropped=0`；
取回 WAV 48 kHz 单声道、时长与输入一致（±0.1 s）。修复前 295 s 卡死（>400 s），修复后 144 s 完成；120 s 无回归。
SPEC-08 内存与完成部分均通过；SPEC-04 的 300 s 时长上限兜底在该区间可用。

## Task 9 状态门控交接（取代 D2）

D2 独立 socket 方案经用户否决回退后，按 SPEC-03 字面「复用帧数据面 / 输出 socket
由对端绑定 / 切换时交接绑定」实现（提交 `c239fd2`、`0af1118`，分支 `file-chain`）。
新增 `PRE_SET_FILE_MODE`（0x0206）：`active=true` 令 pre 暂停 rvc AI 发送并释放
`rvc-out.sock`（接收线程握手确认后才应答），daemon 复用同一对 `rvc-in.sock`/
`rvc-out.sock` 跑文件数据面；收尾 `active=false` 恢复 pre。asr 字幕支路恒发不受影响。
长文件死锁根因（pre 恒发实时帧污染整段攒帧）由状态门控消除，不再靠分离 socket。
设计详见 `docs/plans/file-chain.md` D2。

板端复测（实测，设备与模型同「配置」一节；数据面改为复用 `[rvc]` 默认
`rvc-in.sock`/`rvc-out.sock`，无独立文件 socket，缓冲 8 帧）：部署
`tools/deploy-jetson.sh --smoke` 成功（134 s），设备测试门 `cargo test --workspace`
全绿、ctest（jetson-release）7/7、三进程实时冒烟通过（实时无回归）。
`tools/jetson-file-test.sh` 长文件场景两轮：

| 任务 | 终态 | frames_in | frames_out | dropped | blocks | inference_ms | 输出时长 | RMS | 峰值 | infer 墙钟 | rvc VmHWM |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 295 s | completed | 14750 | 14750 | 0 | 296 | 135702 | 295.000 s | 0.11597 | 0.3477 | 137.0 s | 4630 MiB |
| 120 s | completed | 6000 | 6000 | 0 | 121 | 55388 | 120.000 s | 0.11600 | 0.3495 | 55.7 s | 3589 MiB |

两轮均 `result_ready=true`、`frames_in==frames_out`、`frames_dropped=0`，取回 WAV
48 kHz 单声道时长与输入一致；处理期间 `pre.file_mode=true`（AI 发送暂停）、收尾后
`false`（恢复）经逐秒采样断言通过。GPU 互斥：文件期间 `route set rvc` 挂等待槽
（active=direct、owner=file），任务结束后自动切实时。证据留设备
`/tmp/mozart-file-test-295/`、`/tmp/mozart-file-test-120/`。

## 适用范围

配置：见「配置」一节。全部数值来自该配置下的板端实测，未跨设备或跨模型外推。

样本数：

- 转换成功样本 6 个（3 s、20 s m4a、30 s、30 s 续转、120 s、295 s）；
  3 s 至 120 s 各 1 次（权威轮），295 s 修复后复测 1 次成功（修复前卡死 1 次）。
- 取消样本 1 个（60 s），权威轮 1 次；第 3 轮另测 1 次，
  cancel→cancelled 2 s、控制面恢复 4 s 与 9 s，存在轮次波动。
- 入口拒绝 2 例（不存在模型、坏扩展名），时长上限拒绝 1 例（3453 s）。
- 120 s 阈值探针修复前后多次均通过；295 s 修复后 144 s 完成。
- Task 9 状态门控交接复测：295 s/120 s 确定性合成 WAV 各 1 次（见 Task 9 一节，
  数据面复用 `[rvc]` socket，与上文 D2 独立 socket 轮次不同）。
- 合成 WAV 为确定性信号（180 Hz 基频加谐波、3 Hz 音节节拍），
  真实语音内容仅来自 m4a 截取样本。听感质量未做主观评估。

未验证项：

1. **300 s 整上限的转换完成**。阻塞原因：修复后已实测 295 s 完成，300 s 整上限尚未单独跑。
   解除条件：用 300 s 样本跑一次至 completed。295 s 已修复并实测完成（见「长文件数据面卡死（已修复）」）。
2. **整段 F0 的分段耗时**（SPEC-03 推迟项）。阻塞原因：`GetStatus` 只暴露整段
   `process_cancellable` 的 `inference_ms`，契约无 F0/HuBERT/Generator 分段计时字段。
   解除条件：rvc 增分段计时字段，或在板端做离线 profiling。
   整段推理耗时（含 F0）已实测：3 s→3296 ms、20 s→10147 ms、30 s→14515 ms、
   120 s→55286 ms、295 s→136845 ms（修复后复测）。
3. **近上限长文件的取消等待最坏值**（SPEC-03 推迟项）。阻塞原因：未在 295 s 正常推理路径上
   测取消检查点等待。解除条件：295 s 任务 infer 段发起取消并量等待。
   60 s 段的取消等待已实测（5 s，另轮 2 s）；修复前楔住状态下取消仍可发起（实测）。
4. **队列串行与满 50 拒收**。阻塞原因：本次每轮只提交单任务，未构造多任务并发排队。
   解除条件：一次提交多个任务观察串行执行，并提交至 51 个验证容量拒收。
5. **驱逐策略与结果缓存生命周期**。阻塞原因：受管目录用量远低于 1000 MiB 上限，未触发驱逐。
   解除条件：调低 `storage_limit_bytes` 或堆积已完成结果至超限，验证按时间戳驱逐与未完成任务保护。
6. **文件降噪路径**（`file.denoise = true`）。阻塞原因：本次按默认关配置验收。
   解除条件：开启后重跑 b/c 场景，验证与 pre 共用 DSP 的降噪结果。
7. **`c78c85a` 之前的 ctest 偶发超时是否可复现**。阻塞原因：5 次复跑全绿，未能复现。
   解除条件：在高构建负载下重复跑 ctest 若干轮，或延长该用例超时并记录耗时分布。
8. ~~D2 输入侧隔离的设计确认~~ 已解决：用户否决独立 socket 方案（偏离 SPEC-03 字面），
   已回退为 Task 9 状态门控交接（`PRE_SET_FILE_MODE`），见「Task 9 状态门控交接（取代 D2）」。

证据文件留在设备 `/tmp/mozart-file-test`，不入库：`config-used.toml`、`env.txt`、
`models.json`、`submit-*.json`、`job-*.json`、`progress-*.jsonl`、`st-*.json`、
`stats-*.json`、`out-*.wav`、`measure.json`、`summary.tsv`、`queue-f-*.json`、
`storage-after-cancel.txt`、`reject-*.err`、`run/logs/*.log`。
各轮运行日志为设备上 `/tmp/file-test-run1.log` 至 `run6.log`。
卡死栈与修复复测证据另留设备 `/tmp/mozart-hang-repro/`（`gdb-*-now.txt`、`proc-*.txt`、`st-hung.json`）
与 `/tmp/mozart-fix-verify/`（`st-*-final.json`、`stats-*.json`、`out-*.wav`、`monitor-*.log`），不入库。
