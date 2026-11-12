# 文件转换链路板端实测

日期：2026-11-13。设备：Jetson Orin Nano Super 8 GB。分支：`file-chain`。

## 结论

文件转换链路在 3 s 至 120 s 样本上板端验收通过，10 个场景全部 PASS（实测）。
提交、取回、取消、GPU 互斥、重启续转、入口拒绝、时长上限均按 SPEC-04 生效。

近时长上限的 295 s 长文件存在数据面卡死缺陷：推理已完成但任务永不结束（实测）。
SPEC-08「长文件不会耗尽内存」的内存部分通过，完成部分未通过。
该缺陷是本次验收的唯一阻塞项，详见「长文件数据面卡死」。

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

## 长文件数据面卡死

295 s 样本（第 4 轮，`LONG_BUDGET_S=295`）任务永不结束，观察超过 400 s 仍为
processing（实测）。120 s 样本正常完成，故阈值落在 120 s 与 295 s 之间。

现象与证据：

- rvc 侧：`frames_in = 14750`（等于 `total_frames`，整段收齐），`blocks = 296`，
  `inference_ms_last = 135599.57 ms`，推理已完成；`frames_out = 8` 后不再增长。
  rvc 日志只有 `FileWorker started: total_frames=14750`，无 `FileWorker finished`。
- daemon 侧：任务进度冻结在 `stage=feed`、`percent=37`（约第 12537 帧），
  从未进入 infer；`gpu.owner=file` 不释放；文件 worker 线程连续 3 次采样均为
  `do_sys_poll`、状态 S；`degraded` 列表为空。
- 两侧 socket 接收队列均为 0（`ss -x`），`max_dgram_qlen = 512`。
- 内存未耗尽：rvc VmHWM 4637.5 MiB，daemon VmHWM 28.8 MiB，
  系统 available 2372 MiB、swap 仅用 99 MiB。故不是 OOM 或直接回收 stalled。
- 8 帧正好等于数据面缓冲帧数（`QUEUE_FRAMES = 8`）。
- 楔住状态下 `file cancel` 仍生效（应答 `cancelling`），`down` 正常关停，无进程残留（实测）。

推断（未验证）：daemon 停在喂帧阶段而 rvc 已收齐整段并回出 8 帧后阻塞，
两侧互等，符合「输出缓冲满 + 喂帧未收尾」的死锁形态。
确切机制尚未定位到代码行：`send_blocking_with` 在 send 成功时立即返回、
`wait_writable` 方向为 POLLOUT 且按 100 ms 分片复查取消（代码确认），
与「接收队列为空却持续 EAGAIN」的观测不一致，需要进一步定位。

影响：SPEC-08「长文件不会耗尽内存」的内存部分通过，完成部分未通过；
SPEC-04 的 300 s 时长上限兜底在该区间不可用。
3 s 至 120 s 的常规文件转换不受影响。

## 适用范围

配置：见「配置」一节。全部数值来自该配置下的板端实测，未跨设备或跨模型外推。

样本数：

- 转换成功样本 5 个（3 s、20 s m4a、30 s、30 s 续转、120 s），各 1 次。
- 取消样本 1 个（60 s），权威轮 1 次；第 3 轮另测 1 次，
  cancel→cancelled 2 s、控制面恢复 4 s 与 9 s，存在轮次波动。
- 入口拒绝 2 例（不存在模型、坏扩展名），时长上限拒绝 1 例（3453 s）。
- 卡死样本 1 个（295 s），1 次；阈值下界探针 120 s，2 次均通过。
- 合成 WAV 为确定性信号（180 Hz 基频加谐波、3 Hz 音节节拍），
  真实语音内容仅来自 m4a 截取样本。听感质量未做主观评估。

未验证项：

1. **295 s 至 300 s 区间的转换完成**。阻塞原因：上述数据面卡死缺陷。
   解除条件：定位并修复长文件喂帧/回帧互等，再用 295 s 与 300 s 样本各跑一次至 completed。
2. **整段 F0 的分段耗时**（SPEC-03 推迟项）。阻塞原因：`GetStatus` 只暴露整段
   `process_cancellable` 的 `inference_ms`，契约无 F0/HuBERT/Generator 分段计时字段。
   解除条件：rvc 增分段计时字段，或在板端做离线 profiling。
   整段推理耗时（含 F0）已实测：3 s→3296 ms、20 s→10147 ms、30 s→14515 ms、
   120 s→55286 ms、295 s→135600 ms。
3. **近上限长文件的取消等待最坏值**（SPEC-03 推迟项）。阻塞原因：295 s 段数据面卡死，
   量不到正常推理路径上的取消检查点等待。解除条件：同第 1 项。
   60 s 段的取消等待已实测（5 s，另轮 2 s）；楔住状态下取消仍可发起（实测）。
4. **卡死的确切代码机制**。阻塞原因：观测与 `send_blocking_with` 的实现语义不一致，
   尚未定位到具体阻塞行。解除条件：在板端对长文件任务做线程级 syscall 跟踪或复现最小用例。
5. **队列串行与满 50 拒收**。阻塞原因：本次每轮只提交单任务，未构造多任务并发排队。
   解除条件：一次提交多个任务观察串行执行，并提交至 51 个验证容量拒收。
6. **驱逐策略与结果缓存生命周期**。阻塞原因：受管目录用量远低于 1000 MiB 上限，未触发驱逐。
   解除条件：调低 `storage_limit_bytes` 或堆积已完成结果至超限，验证按时间戳驱逐与未完成任务保护。
7. **文件降噪路径**（`file.denoise = true`）。阻塞原因：本次按默认关配置验收。
   解除条件：开启后重跑 b/c 场景，验证与 pre 共用 DSP 的降噪结果。
8. **`c78c85a` 之前的 ctest 偶发超时是否可复现**。阻塞原因：5 次复跑全绿，未能复现。
   解除条件：在高构建负载下重复跑 ctest 若干轮，或延长该用例超时并记录耗时分布。

证据文件留在设备 `/tmp/mozart-file-test`，不入库：`config-used.toml`、`env.txt`、
`models.json`、`submit-*.json`、`job-*.json`、`progress-*.jsonl`、`st-*.json`、
`stats-*.json`、`out-*.wav`、`measure.json`、`summary.tsv`、`queue-f-*.json`、
`storage-after-cancel.txt`、`reject-*.err`、`run/logs/*.log`。
各轮运行日志为设备上 `/tmp/file-test-run1.log` 至 `run6.log`。
