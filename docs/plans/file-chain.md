# 文件转换链路实现计划

以 SPEC-04（daemon 控制面）、SPEC-03（rvc）、SPEC-02（pre/DSP）为依据，落地文件转换链路：
CLI 提交 → daemon 队列/存储/仲裁 → ffmpeg 解码 → 可选降噪 → 喂帧给 rvc 整段批处理 →
收帧编码 WAV → 原子发布 → CLI 取回。同时补齐核对 SPEC 发现的控制面缺口。

## SPEC 核对差距（2026-11-13）

代码确认（读源码得出，非实测）：

| 项 | SPEC 条款 | 现状 | 处理 |
| --- | --- | --- | --- |
| `[file]` 配置段 | config.md `[file]` | `core/src/config.rs` 无此段 | Task 1 |
| 文件命令号 | SPEC-04 CLI 命令面 | daemon/ctl 无 file 命令 | Task 1/4/6 |
| `RVC_LIST_MODELS` | SPEC-03 控制消息 | C++ 有 `ModelManager::list_models`，无控制命令 0x0307 | Task 1/3 |
| 文件整段批处理 | SPEC-03 离线推理 v1 | `AudioWorker` 恒走 `StreamingRvc` 帧泵，file kind 仅换 preset，未攒整段 | Task 3 |
| 文件不丢发送 | SPEC-03 文件帧阻塞/流控 | Rust `FrameSocket::send` 与 C++ `WriteFrame` 均非阻塞丢弃 | Task 1/3 |
| 文件队列/存储/驱逐 | SPEC-04 文件链路 7 点 | daemon 无实现 | Task 4/5 |
| GPU 仲裁（实时/文件） | SPEC-04 资源仲裁 | `commit_route` 仅按路由设 `gpu_owner`，无 File 活动、无路由等待槽 | Task 4 |
| 文件数据面中间层 | SPEC-04 daemon 解码/编码 | 无 | Task 5 |
| DSP 共享 crate | SPEC-02 抽共享 crate | DSP 在 `pre/src/dsp/`，未独立 | Task 2 |
| `model list` CLI | SPEC-04 | 无 | Task 6 |
| SIGTERM/SIGINT 优雅关停 | SPEC-04 | `daemon/src/main.rs` 无信号处理 | Task 7 |
| `restart <service>` | SPEC-04 | 无 | Task 7 |
| `device list` | SPEC-04（2026-11-13） | 无 ALSA 枚举 | Task 7 |
| `config get/set/save` + daemon 运行时状态文件 | SPEC-04 配置写回 | 无；桥自写 `bridge-state.json`（偏离 daemon 单一状态文件） | 推迟（见下） |

推迟项：`config get/set/save` 与 daemon 统一运行时状态文件体量大且与文件链路正交，
本计划不做；`[file]` 运行时键 v1 经 TOML 启动读取即可满足文件转换。桥状态文件归并另议。

## 设计决策

D1. **文件批处理输入结束信号**：`StartStream`（kind=file）新增必填 `total_frames`（u32）。
C++ 攒满 `total_frames` 帧后跑整段离线推理，再回输出帧。SPEC-03 未定义结束信号，
此为最小契约扩展（不新增控制动词，符合粗粒度边界）。取消仍走 `StopStream` 检查点。

D2. **文件输出 socket（偏离 SPEC 字面，待确认）**：SPEC-03 字面要求复用 `rvc-out.sock`
并在 pre/daemon 间交接绑定。但 pre 现在启动即绑定 `rvc-out.sock` 且终身持有
（`pre/src/main.rs:179`），交接需改动已板端验收的实时路径，风险高。
本计划改用**独立文件输出 socket**：daemon 绑定 `rvc-file-out.sock`，`StartStream`
（kind=file）携带 `output_socket`（绝对路径）覆盖默认；C++ 按对端无感知原则连接该路径。
pre 实时路径完全不动。仍满足 SPEC 核心意图（C++ 只收发帧、对端无感知、文件不丢、
实时/文件互斥）。**此偏离需用户确认**；若坚持字面交接，改为 pre 路由感知绑定（后续局部改动）。

D3. **队列持久化**：SPEC-04 定稿落盘续转。队列快照写 `<storage_dir>/queue.json`，
每次变更原子写（临时文件 + rename）。重启读回 queued/processing 任务，processing 重置为
queued 从头重跑（推理状态不保留），源副本已在受管目录。暂停标志一并持久化。

D4. **命名与布局**：`job_id = "job_<epoch_us>"`；源副本 `<job_id>.input<ext>`；
结果 `<job_id>.output.wav`；临时 `<job_id>.decoded.f32` / `<job_id>.output.f32` /
`<job_id>.output.wav.part`。驱逐跳过未完成（queued/processing/cancelling）任务文件，
按文件名时间戳从旧到新删至 80%。沿用旧 `ModeController::evict_cache` 口径。

D5. **进度**：`FileProgress { stage, percent, blocks_done, blocks_total }`。
stage ∈ decode/feed/infer/publish；percent 0..100 粗阶段；infer 段用 rvc `GetStatus.blocks`
细化（blocks_total 由 daemon 按窗长估算，缺则 None）。

D6. **GPU 仲裁**：`gpu_owner: Option<Activity>`（Realtime/File）。文件任务执行claim File；
`route set rvc` 遇 File 占用 → 单槽等待（后请求覆盖），当前任务结束后切换；
文件提交遇 Realtime 占用 → 入队等待；GPU 空闲且有排队任务且未暂停 → poller 自动启动。
模式为派生状态，不入配置状态文件。

## Global Constraints

- 按 SPEC-02/03/04 与 config.md 实现，不虚捏；偏离只 D2 一处且须标注。
- C 头 `core/include/mozart/` 是跨语言契约唯一定义源；改头同步 `core/src/ctl.rs` 镜像与编译期断言。
- 控制面：16 B LE 信封 + JSON 体，SEQPACKET；命令号 daemon `0x01xx` / rvc `0x03xx`。
  响应体固定 `status`/`code`/`reason`/`data`。错误码沿用 `core/src/ctl.rs` 常量。
- 数据面：InputFrame 1300 B / OutputFrame 3860 B，Unix DGRAM，一帧一条；receiver binds / sender connects。
- 子进程 CLI 契约 `--socket <path> [--config <path>]`；daemon 以 `--config` 转交同一 TOML。
- 入口校验外部输入一次，内部信任已校验值；同一限制只定义一处；不写只有测试能触发的守卫。
- 实时线程不分配不阻塞；文件数据面在 daemon 独立线程，离线，不在实时路径上。
- 降级记原因并经 status 可查；只捕获能处理的异常，其余向上抛。
- 提交前跑 `tools/ci-local.sh --staged`，不用 `--no-verify`；commit 中文前缀，一提交一事。
- Rust 用 Cargo workspace；C++ 用 CMake 预设（host-debug / jetson-release）+ CTest。
- 主机 x86_64 做构建与单测（mock 管线，无 GPU）；声学与延迟验收在 Jetson。
- 不碰用户未提交文件；`docs/plans/` 当前未跟踪，本计划入库时一并提交。

## Task 1: core 契约与配置扩展

文件：`core/include/mozart/ctl.h`、`core/src/ctl.rs`、`core/src/ctl/schema.rs`、
`core/src/config.rs`、`core/src/dgram.rs`、`core/src/lib.rs`（如需）。

内容：
1. 命令号（ctl.h + ctl.rs 同步）：`DAEMON_FILE_SUBMIT=0x0110`、`DAEMON_FILE_PAUSE=0x0111`、
   `DAEMON_FILE_RESUME=0x0112`、`DAEMON_FILE_CANCEL=0x0113`、`DAEMON_FILE_FETCH=0x0114`、
   `DAEMON_FILE_STATUS=0x0115`、`DAEMON_FILE_LIST=0x0116`、`DAEMON_MODEL_LIST=0x0117`、
   `DAEMON_RESTART=0x0118`、`DAEMON_DEVICE_LIST=0x0119`；`RVC_LIST_MODELS=0x0307`。
2. schema.rs 新增（serde，字段名为稳定契约）：
   - `FileJobState` 枚举 lowercase：queued/processing/cancelling/completed/failed/cancelled。
   - `FileProgress { stage: String, percent: u8, blocks_done: Option<u64>, blocks_total: Option<u64> }`。
   - `FileJob { job_id, name, model_id, state: FileJobState, progress: FileProgress, error: Option<String>, result_ready: bool, created_unix_ms: u64 }`。
   - `FileSubmitRequest { path: String, model_id: Option<String> }`（deny_unknown_fields）。
   - `FileCancelRequest { job_id: String }`、`FileFetchRequest { job_id: String, output: Option<String> }`、
     `FileStatusRequest { job_id: String }`（均 deny_unknown_fields）。
   - `ModelEntry { id: String, exists: bool, loaded: bool }`。
   - `DeviceEntry { id: String, label: String, kind: String }`（kind capture/playback）。
   - `RestartRequest { service: String }`。
   - `StartStream` 文件扩展字段在 rvc 侧解析（Task 3），core 不强制结构体。
3. config.rs 新增 `FileConfig`（`[file]`，键与默认见 config.md）：
   `storage_dir`（默认 `~/.local/state/mozart/files/`，展开 ~）、`queue_capacity=50`（≥1）、
   `storage_limit_bytes=1048576000`（>0）、`denoise=false`、`max_duration_seconds=300`（≥1）、
   `max_upload_bytes=104857600`（>0）、`ffmpeg_path="ffmpeg"`、`ffmpeg_timeout_ms=120000`（>0）、
   `output_socket="rvc-file-out.sock"`（纯文件名）。`Config` 加 `file` 段；
   `file_storage_dir()`、`file_output_socket()`（相对 run_dir）getter；validate 范围。
   分类按 config.md（storage_dir/output_socket 为 S，其余 R；v1 仅启动读取）。
4. dgram.rs：`FrameSocket::send_blocking(&self, frame)` —— 不带 `MSG_DONTWAIT`，
   EINTR 重试，EAGAIN/ENOBUFS 等可写后重试（poll POLLOUT），对端不在返回 Err（不丢弃）。
   与现有非阻塞 `send` 并存，注释标明文件链路用。
5. 单元测试：命令号唯一且分段；schema 往返稳定 + deny_unknown_fields 拒多余字段；
   FileConfig 默认/范围/~ 展开；send_blocking 对端绑定后送达、对端不在返回 Err。

验收：`cargo test -p mozart-core` 通过；`cargo check --workspace` 通过；ctl.h 与 ctl.rs 命令号一致。

## Task 2: DSP 共享 crate 抽取

文件：新 `dsp/`（crate `mozart-dsp`）、`pre/src/dsp/`（迁出）、`pre/Cargo.toml`、
`pre/src/main.rs` 等引用处、`Cargo.toml`（workspace members）、`daemon/Cargo.toml`（依赖）。

内容：
1. 把 `pre/src/dsp/` 整体迁为独立 crate `mozart-dsp`（`dsp/src/`），保留全部阶段与 golden 对拍测试。
   对外暴露离线文件降噪所需入口：按 960 样本主干帧处理 + 打包 320 样本 16 kHz 契约帧
   （复用现有 `process_frame`/`pack_frame` 语义，WOLA 首块 896/稳态 960、滞后 64、补零保持节拍）。
   暴露构造配置（denoise/gate/agc/limiter 开关）与 VAD 标志读取。
2. pre 改为依赖 `mozart-dsp`，删除 `pre/src/dsp/`，引用路径改 `mozart_dsp::`；实时行为逐位不变。
3. daemon 加 `mozart-dsp` 依赖（Task 5 离线降噪用）。
4. golden 夹具与测试随 crate 迁移，`.gitignore` 白名单路径若变动同步更新。

验收：`cargo test --workspace` 通过（含 DSP golden 对拍）；pre 现有测试不回归；
`mozart-dsp` 可被 daemon 依赖编译。

## Task 3: rvc ListModels 与文件整段批处理

文件：`core/include/mozart/ctl.h`（Task 1 已加 0x0307）、`rvc/control/stream_session.{h,cpp}`、
`rvc/control/ctl_link.cpp`（如需）、`rvc/src/rvc/audio_worker.cpp` 或新 `rvc/src/rvc/file_worker.cpp`、
`rvc/include/rvc/audio_worker.hpp`、`rvc/transport/unix_dgram_stream.{h,cpp}`、
`rvc/src/rvc/pipeline.cpp`/`inferencer.cpp`（blocks 计数）、`tests/test_rvc_control.cpp`、
新 `tests/test_rvc_file.cpp`、`tests/CMakeLists.txt`。

内容：
1. `ListModels`（0x0307）：调 `ModelManager::list_models`，返回 `{models:[{id,exists,loaded}]}`；
   只读、不加载、不阻塞推理线程。`loaded` 按当前模型 id 判定。
2. `StartStream` kind=file 扩展：必填 `total_frames`（正整数）、可选 `output_socket`（绝对路径，
   覆盖默认输出 socket）。realtime 忽略这两字段。入口校验一次。
3. 文件批处理 worker（新 `FileWorker` 或 AudioWorker file 分支）：
   - 攒帧：ReadFrame 循环把 320 样本拼成整段 `vector<float>`，计满 `total_frames` 止；每帧查取消。
   - 推理：调 `pipeline_.process_cancellable(audio_16k, cancelled)`（现有离线路径：
     峰值归一 0.95、48 Hz 高通、整段 F0、整段一次过或 2 s 滑窗、RMS 混合、限幅 0.99）。
   - 回帧：输出 48 kHz 整段切 960 样本 OutputFrame，`WriteFrame` 阻塞逐帧发；meta.frame_idx 顺序。
   - 完成：发完置 done；`GetStatus` 在 StopStream 前仍报 streaming，blocks 反映窗数。
4. 阻塞输出：`UnixDgramStream` 增阻塞发送模式（file 用），EAGAIN 等可写重试，不丢弃；
   实时模式不变。输入侧 rvc 读帧本就阻塞。
5. blocks 计数：文件模式按 Generator 窗计数（`infer_window` 每次 +1），整段一次过计 1；
   经 `GetStatus.blocks` 暴露。批处理开始清零。
6. 取消：`StopStream` 置 cancelled + 关流；攒帧/推理检查点（F0 前后、每窗）响应；
   捕获 `InferenceCancelled` 收尾，不跨进程抛异常。
7. 测试（mock 管线，无 GPU）：ListModels 返回清单；file 批处理 N 帧入 → N 帧出、内容经
   mock 上采样可验证；total_frames 校验；取消中断；阻塞输出对端慢读不丢。注册 CTest。

验收：`cmake --preset host-debug && cmake --build --preset host-debug && ctest --preset host-debug` 通过；
现有 rvc 测试不回归。

## Task 4: daemon 文件队列、存储与 GPU 仲裁

文件：新 `daemon/src/file/{mod,queue,storage,arbitrate}.rs`、`daemon/src/state.rs`、
`daemon/src/services.rs`、`daemon/src/server.rs`、`daemon/src/poller.rs`、
`daemon/tests/file_queue.rs`（新）。

内容：
1. 队列（queue.rs）：`FileQueue` 持 `Vec<FileJob>` + 暂停标志；上限 `queue_capacity`（入队查，满拒收）。
   状态机六态；`submit`（生成 job_id、queued）、`pause`/`resume`、`cancel`（queued→cancelled 删源；
   processing/cancelling→标 cancelling 交 worker）、`status`/`list`。落盘 `queue.json` 原子写（D3）；
   启动读回，processing→queued 重跑。
2. 存储（storage.rs）：`copy_in`（扩展名白名单 .wav/.mp3/.m4a/.aac/.flac/.ogg、
   `max_upload_bytes` 预检、复制到 `<job_id>.input<ext>`）；`evict`（每任务后目录总大小超
   `storage_limit_bytes` 按时间戳删至 80%，护未完成）；临时文件清理；结果路径解析。
3. 仲裁（arbitrate.rs + state.rs）：`gpu_owner`  decouple 路由——Realtime 由 route=rvc claim，
   File 由文件任务 claim。`route set rvc` 遇 File → 单槽等待（后覆盖前），任务结束 poller 应用；
   文件提交遇 Realtime → 入队；GPU 空闲 + 有排队 + 未暂停 → poller 触发 worker 启动下一任务。
   模式派生，不入状态文件。
4. 模型校验：submit 带 `--model` 经 `RVC_LIST_MODELS` 校验 exists，不存在当场拒收（InvalidArgument）；
   缺省用当前已加载模型（rvc GetStatus.model_id），无模型且未指定 → 拒收 NotReady。
5. server.rs 接线：FILE_SUBMIT/PAUSE/RESUME/CANCEL/STATUS/LIST、MODEL_LIST 命令处理
   （数据面执行在 Task 5；本任务 submit 入队后由 worker 占位驱动，可先以 stub worker 验证队列/仲裁）。
6. 测试：队列满拒收、状态转换、落盘读回（processing 重置）、暂停只停新任务、取消排队删源、
   驱逐护未完成删至 80%、模型不存在拒收、GPU 仲裁（实时占用入队、文件占用路由等待槽、空闲自动转）。
   用 stub rvc（core/examples/rvc_stub.rs 或测试内桩）。

验收：`cargo test -p mozart-daemon` 通过；队列/存储/仲裁单测覆盖上述分支；`cargo check --workspace`。

## Task 5: daemon 文件数据面 worker

文件：新 `daemon/src/file/worker.rs`、`daemon/src/file/decode.rs`、`daemon/src/file/encode.rs`、
`daemon/src/file/mod.rs`、`daemon/Cargo.toml`（mozart-dsp）、`daemon/tests/file_worker.rs`（新）。

内容：
1. decode.rs：ffmpeg 解码（沿用旧参数）：plain `-nostdin -y -v error -i <src> -t <max+1>
   -ac 1 -ar 16000 -f f32le <tmp>`；denoise 路径 `-ar 48000`。`ffmpeg_timeout_ms` 超时杀进程；
   解码后字节数 > `max_duration_seconds*16000*4` → 拒（时长超限）；空/非 float 对齐 → 失败。
2. denoise（可选，`[file].denoise`）：48 kHz 裸流按 960 样本喂 `mozart-dsp`，取 320 样本 16 kHz
   契约帧；vad_flag==0 帧补零（沿用旧 file_denoise 语义）。
3. worker.rs（独立线程，串行处理队列）：claim GPU → LoadModel（按需）→ 解码/降噪 →
   绑定 `file_output_socket` → `StartStream{kind:file, model_handle, 会话参数, total_frames, output_socket}` →
   `send_blocking` 喂 N 帧到 rvc-in.sock → 阻塞收 N 帧 OutputFrame（带超时）→ `StopStream` →
   编码发布。进度按 D5 更新（infer 段读 poller 缓存的 rvc blocks）。取消检查点贯穿各阶段；
   执行中取消 → StopStream + 清临时 + 标 cancelled。失败标 failed 带原因、队列继续。
4. encode.rs：输出 48 kHz f32 → ffmpeg `-f f32le -ar 48000 -ac 1 -i <raw> -f wav -c:a pcm_f32le <.part>`
   → rename 发布 `<job_id>.output.wav`；失败清 .part。
5. 会话参数取 `[rvc]`（pitch_shift/index_rate/rms_mix_rate/protect/f0_method），实时/文件共用一份。
6. 临时文件（decoded.f32/output.f32/.part）取消/失败删除，成功改名；C++ 不碰文件。
7. 测试：stub rvc（mock file 批处理）端到端 submit→completed→fetch；时长超限拒；
   取消执行中任务；denoise 路径产出 16 kHz 帧数正确；ffmpeg 缺失/超时失败带原因。
   主机有 ffmpeg，用真实 ffmpeg + stub rvc。

验收：`cargo test -p mozart-daemon` 通过；端到端集成测试（stub rvc + 真 ffmpeg）产出可解析 WAV；
`cargo check --workspace`。

## Task 6: ctl CLI 文件命令与 model list

文件：`ctl/src/main.rs`、`ctl/tests/cli.rs`（新或扩）。

内容：
1. 名词-动词命令：`file submit <path> [--model <id>]`、`file pause`、`file resume`、
   `file cancel <job_id>`、`file fetch <job_id> [-o <path>]`、`file status <job_id>`、`file list`；
   `model list`。瘦客户端：解析→组消息→发 daemon.sock→格式化回复，零决策。
2. `--json` 稳定输出：submit 回 `{job_id,state,queue_position}`；status/list 回 FileJob（含 progress）；
   model list 回 `{models:[...]}`；fetch 成功回 `{job_id,output_path}`。
3. `file fetch`：daemon 回结果路径（或经协议传文件——v1 daemon 与 CLI 同机，回绝对路径由 CLI 复制；
   `-o` 指定目标，缺省原文件名主干 + `.wav`）。结果不可取（未完成/已驱逐）→ 拒绝带原因。
4. 退出码沿用：0 成功 / 1 其他 / 2 参数错 / 3 不可达 / 4 拒绝 / 5 超时。
5. 人读输出表格化；非终端不出颜色（NO_COLOR）。
6. 测试：命令解析、--json schema、退出码分类（mock daemon 或桩 socket）。

验收：`cargo test -p mozart-ctl` 通过；`mozart-ctl file --help`/`model list --help` 正确；
`cargo check --workspace`。

## Task 7: 控制面补齐（SIGTERM / restart / device list）

文件：`daemon/src/main.rs`、`daemon/src/server.rs`、`daemon/src/supervisor.rs`、
新 `daemon/src/device.rs`、`ctl/src/main.rs`、`daemon/tests/`（扩）。

内容：
1. SIGTERM/SIGINT：daemon 装信号 handler（signal-hook 或 libc sigaction + 自管道），
   触发与 `Shutdown` 同一优雅关停（停子进程 SIGTERM→2 s→SIGKILL、退监听、清 socket），幂等。
2. `restart <service>`（DAEMON_RESTART 0x0118）：重启单个服务（pre/rvc/asr/tts/bridge/llama），
   重读配置生效；supervisor 停再起；回 `{service, state}`。非法服务名 InvalidArgument。
3. `device list`（DAEMON_DEVICE_LIST 0x0119）：ALSA `snd_device_name_hint` 枚举采集/播放设备，
   按需不缓存，回 `{devices:[{id,label,kind}]}`；pre 不枚举。daemon 加 alsa 依赖。
4. ctl 加 `restart <service>`、`device list` 命令与 --json。
5. 测试：信号触发优雅关停（子进程被停）；restart 单服务；device list 在无声卡环境返回空或错误带原因
   （主机可能无声卡，测试容忍空清单）。

验收：`cargo test -p mozart-daemon -p mozart-ctl` 通过；`cargo check --workspace`。

## Task 8: 集成测试与 Jetson 上机验收

文件：`tools/jetson-smoke.sh`（扩或新 `tools/jetson-file-test.sh`）、
`reports/file-chain-<YYYYMMDD>/RESULTS.md`（新）。

内容：
1. 主机全量：`tools/ci-local.sh`（repo/lint/native/rust）全绿；workspace 集成测试通过。
2. 部署：`tools/deploy-jetson.sh --smoke`（rsync + 远端构建 + 测试门 + 三进程冒烟）。
3. 板端文件转换实测：用真实模型（`~/Mozart-Old/rvc-golden/qiqi-zh-run` 或 `~/models`），
   `mozart-ctl file submit <测试音频>` → `file status` 观察进度 → `file fetch` 取回 WAV；
   验证：结果可听、时长一致、取消生效、队列串行、实时/文件互斥（文件期间 route set rvc 挂等待槽）、
   重启续转（daemon 重启后 queued 任务续跑）。
4. 客观测量：输入/输出时长、RMS、峰值、推理 blocks/耗时、内存峰值（300 s 兜底）。
5. 写 `reports/file-chain-<YYYYMMDD>/RESULTS.md`：配置、样本数、实测值、适用范围、未验证项
   （整段 F0 耗时与取消等待按 SPEC 推迟项注明阻塞原因与解除条件）。

验收：板端 submit→fetch 成功产出变声 WAV；取消/互斥/续转各验证一次；报告含适用范围。
前置：Jetson 可达（当前 moyamryia@10.0.0.4 已连通）。

## 待用户确认的开口

1. **D2 文件输出 socket**：独立 `rvc-file-out.sock`（本计划默认，不动实时路径）vs SPEC 字面
   `rvc-out.sock` 交接（需改 pre 路由感知绑定）。默认前者。
2. **范围**：`config get/set/save` + daemon 统一运行时状态文件本计划推迟，是否认可。
3. **D1 total_frames**：作为文件批处理结束信号是否认可（替代新增 FinishStream 动词）。