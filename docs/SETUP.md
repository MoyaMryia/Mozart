# Jetson 开发环境与部署

最后更新：2026-11-22。本文是 Jetson 目录、资产、部署与测试的唯一权威。
连接方式不在仓库记录；部署时用参数或 `MOZART_JETSON` 提供 SSH 目标。
资产分层与工具链的设计规范见 [SPEC/SPEC-10-env.md](SPEC/SPEC-10-env.md)，TOML 字段见 [config.md](config.md)。

## 平台

- 设备：NVIDIA Jetson Orin Nano Super 8 GB，ARM64，JetPack 6。
- 推理后端：TensorRT 10.16.2 与 ONNX Runtime。引擎绑设备，须在设备上编译。
- 音频：ALSA。无物理声卡时用 snd-aloop。

## 目录规范

设备只使用下列活动目录。新增目录先写入本文。

```text
~/Mozart/                     # 部署源码与设备构建；deploy 管理，不手改
├── target/debug/             # Rust 构建（含 examples/rvc_stub）
├── build/jetson-release/     # C++/CTest 产物
└── tools/                    # 板端测试脚本（随源码同步）

~/.mozart-prev/               # 最近部署前源码快照（人工回滚锚点）
~/.mozart-source-backup.*/    # 各部署事务独立源码快照
~/.mozart-deploy.lock         # 同一账户的 flock 会话锁文件

~/mozart-runtime/             # 运行时根；同一时间只跑这一份
├── mozart.toml               # 运行配置（唯一手改入口）
├── bin/                      # 9 个程序和 examples/rvc_stub
├── bin.prev/                 # 上一次部署的二进制（保留文件 capability）
├── web.prev/                 # 上一次部署的静态资源
├── run/                      # socket、daemon.lock、logs/
├── state/                    # 持久状态（字幕、文件任务缓存）
│   └── web-uploads/          # 适配器私有上传暂存与恢复记录
├── web/dist/                 # 适配器托管的前端生产构建
├── in/                       # 手工测试输入
└── out/                      # 手工测试输出

~/mozart-assets/              # 成品资产与工具链依赖（只读）
├── lib/                      # sherpa、libmozart_aec3 等共享库
├── llama/                    # llama-server 与 .so
├── RVC/                      # 上游 RVC 源码树（导出/golden 的 --rvc-source）
└── models/
    ├── rvc/<id>/             # 含 presets/default.toml（唯一可写例外）
    ├── asr/<id>/
    ├── asr-review/<id>/
    ├── tts/<id>/
    └── llm/<file>.gguf

~/rvc-pth/                    # 原料权重（pth，不参与推理）
~/mozart-toolkit/             # 导出/golden 的 Python 环境与固定依赖
└── rvc-env/                  # venv（torch/transformers/onnx 等）
~/mozart-samples/             # 测试音频样本
~/mozart-archive/             # 归档：原始权重、基准引擎
└── legacy/                   # 历史实验目录，仅作追溯

# 可选 systemd 托管文件（仅运行 tools/install-jetson-systemd.sh 时写入）
/etc/systemd/system/mozart-clocks.service  # 开机定频（jetson_clocks）
/etc/systemd/system/mozart.service         # 运行时自启（普通运行用户前台托管 daemon）
/etc/sudoers.d/mozart-deploy               # 最小权限免密（仅放行启停/重启 mozart.service）
/var/lib/mozart/l4t_dfs.conf               # 原动态调频备份（停止 mozart-clocks 时恢复）
```

规则：

- `~/Mozart` 由部署脚本管理，不在设备上手改源码。
- `~/mozart-runtime` 是唯一运行实例；配置只改 `mozart.toml`。
- 9 个程序为 `mozart-daemon`、`mozart-ctl`、`mozart-pre`、`mozart-enhance`、`mozart-rvc`、`mozart-asr`、`mozart-tts`、`mozart-bridge` 和 `mozart-web-adapter`（由 `daemon` 统一监管，无独立启动脚本）。
- `~/mozart-assets` 只读；例外是各 RVC 模型目录内的 `presets/`，由 daemon 写入模型预设（SPEC-11）。其余测试只写 `/tmp/mozart-*` 与 `~/mozart-runtime/{run,state,out}`。
- 历史实验目录只放 `~/mozart-archive/legacy/`；旧报告中的路径视为历史记录。

## 隔离开发例外

用户指定的 `~/MozartMryia/` 用于 UL-UNAS 分支验证，不作为正式部署入口。
源码与构建位于该目录；`runtime/` 存放独立配置、程序、socket、状态与输出。
测试进程统一设置 `MOZART_STATE_DIR=~/MozartMryia/runtime/state/daemon`，不读取或修改正式用户级状态文件。
模型配置只读指向板端既有资产路径；同步时排除模型、engine、音频及原部署构建缓存，不复制模型。
测试不覆盖 `~/Mozart` 或 `~/mozart-runtime`。运行前确认没有其他测试；构建前暂停本实例，独占设备时先确认正式栈状态并在结束后恢复。

## 工作流

本机执行，常用命令按职责分开：

```bash
# 本机全量即时测试
tools/ci-local.sh all

# 部署：同步 → 构建 → 安装 → 重启；不跑测试
MOZART_JETSON=<SSH目标> tools/deploy-jetson.sh

# 安装 Jetson 开机定频与自启 systemd 服务（默认自动调速风扇，可选 --restart=no / --fan / --uninstall）
MOZART_JETSON=<SSH目标> tools/install-jetson-systemd.sh

# 仅前端部署：构建 web → 替换运行位 web/dist；不编后端、不停栈、不重启
MOZART_JETSON=<SSH目标> tools/deploy-web.sh

# 部署后测试：设备端单测 + 功能测试，按套件可选
MOZART_JETSON=<SSH目标> tools/jetson-e2e.sh

# 本机音频桥接：跨平台 SSH 听板端输出 / 双向试听 / 同步录 WAV
MOZART_JETSON=<SSH目标> uv run tools/jetson-audio.py
```

提交前运行 `tools/ci-local.sh --staged`；pre-commit hook 调用同一脚本。

### 本机检查阶段

`tools/ci-local.sh` 按阶段拆分，也可单独运行：

| 阶段 | 内容 |
| --- | --- |
| `repo` | 仓库卫生：忽略规则一致、单文件不超过 5 MiB |
| `lint` | 文档行尾空白、报告“适用范围”一节 |
| `native` | `host-debug` 预设构建 + CTest |
| `rust` | `cargo check` + `cargo test`（全 workspace） |
| `adapter` | 构建 daemon、ctl 和 core 示例，检查并测试适配器 |
| `web` | 前端测试与构建；`web/` 未建时跳过 |

### 部署行为

`tools/deploy-jetson.sh` 默认保留构建缓存，`--full` 清理缓存后重编。两种模式均在构建前暂停运行时，释放 Jetson 共享内存。

AEC3 为可选构建依赖：部署脚本只编 `mozart-rvc` 与 `mozart-enhance` 两个 C++ 目标，不编 `mozart_aec3`。封装库需预先构建并放入资产 lib 目录（`libmozart_aec3.so`）；部署脚本检测到后导出 `MOZART_AEC3_LIB_DIR` 供 pre 链接真实后端，否则 pre 用未链接桩，`pre.aec.enabled` 开启会报告不可用原因。构建依赖与链接契约见 [SPEC-02](SPEC/SPEC-02-pre.md)。

1. 通过独立 SSH 会话持有 `flock` 文件锁。创建事务源码快照，并同步到 `~/.mozart-prev`。遇到含 `owner` 的历史锁目录时停止，请先核实旧事务。
2. rsync 本机仓库到 `~/Mozart`，排除 `target/`、`build/`、`.git/`。默认按内容同步（`--checksum --no-times`）：变更文件落传输时刻的 mtime，未变更文件保持原 mtime，构建缓存保留。`--full` 在设备构建阶段运行 `cargo clean` 和 CMake `--clean-first`。
3. 源码同步后暂停整栈，释放共享内存；保持停机至安装完成。
4. 构建 Rust debug 二进制与 `rvc_stub` 示例；构建 `jetson-release` C++ 产物；rvc 对齐到 `target/debug`。增量时 cargo/ninja 只重编受影响部分。
5. 安装前用新 ctl 的 `config validate --json` 离线校验 `mozart.toml`。备份 `~/mozart-runtime/bin` 到 `bin.prev`，备份 `~/mozart-runtime/web` 到 `web.prev`；安装 9 个程序、静态资源与 `examples/rvc_stub`，并清理旧 `bin/mozart-web-service`。安装用临时文件加原子替换，不打断运行中的进程；若配置低端口（`< 1024`）则对 `mozart-web-adapter` 授予 `cap_net_bind_service`。
6. 用 `mozart.toml` 启动整栈（若设备已启用 `mozart.service`，自动经 `systemctl start mozart.service` 启动，否则经 `mozart-ctl up`），等待 `web`、pre、RVC、配置中的增强链路及原本正常的服务恢复。
7. 按预检结果检查 RVC 状态。显式选择 `rvc` 且配置了模型时要求 `loaded` 或 `streaming`；默认直出也接受 `idle`。除手动模式下输出模式为空外，等待输出模式应用。探测 `/api/v1/health`。HTTP 返回 `200` 且 `status` 为 `ok` 后写 `deploy.json`。访问源不在非空 `allowed_origins` 中时只告警，不修改配置。
8. 成功后按修改时间保留最近 3 份源码快照。当前用户的 `/tmp/mozart-deploy.*` 事务目录也保留 3 份，其中必须包含当前事务。

发布前失败时由持锁会话执行事务内的 `rollback.sh`。发布后清理失败会返回非零，保留已发布的新栈。对原有源码目录，仅在开始同步后按内容回退；回退文件使用新的修改时间，不清空构建缓存。安装开始后还恢复 `bin.prev` 与 `web.prev`。按原托管方式恢复服务。会话结束时自动释放锁；失败事务保留记录供排查，不删除锁文件。

### 开机定频与自启（systemd）

仓库维护两份独立服务模板，不写死设备用户名和路径，安装时按当前运行用户与 `~/mozart-runtime` 渲染：

- `deploy/systemd/mozart-clocks.service.in`：开机在 `nvpmodel.service` 之后备份原调频配置到 `/var/lib/mozart/l4t_dfs.conf`，并执行 `jetson_clocks` 锁定最高频（默认保留自动调速风扇，需满载可加 `--fan`；不声明 `After=nvfancontrol.service` 以避免 `jetson_clocks --restore` 内部启停 `nvfancontrol` 时触发 systemd 死锁）；停止该服务时自动恢复原调频状态。
- `deploy/systemd/mozart.service.in`：在 `mozart-clocks.service` 就绪后，以运行用户前台启动 `~/mozart-runtime/bin/mozart-daemon --config ~/mozart-runtime/mozart.toml`，停止时发送 `SIGTERM` 由 daemon 在 30 s 内平滑收尾（`KillMode=mixed`）。

安装、切换策略或卸载均通过 `tools/install-jetson-systemd.sh` 完成（也支持在 Jetson 本机加 `--local` 执行）：

```bash
# 默认安装：开机定频（自动调速风扇）+ 异常退出自动重启（Restart=on-failure）
MOZART_JETSON=<SSH目标> tools/install-jetson-systemd.sh

# 关闭自动重启（崩溃后停在停止态，便于手工排障）
MOZART_JETSON=<SSH目标> tools/install-jetson-systemd.sh --restart=no

# 需要满载风扇散热时显式开启
MOZART_JETSON=<SSH目标> tools/install-jetson-systemd.sh --fan

# 卸载两个服务与部署免密规则
MOZART_JETSON=<SSH目标> tools/install-jetson-systemd.sh --uninstall
```

安装脚本会同时生成经 `visudo -cf` 校验的最小权限 `/etc/sudoers.d/mozart-deploy`，仅允许运行用户无交互启停与重启 `mozart.service`。此后执行 `tools/deploy-jetson.sh` 与 `tools/jetson-e2e.sh` 时，脚本会核对 `mozart.service` 的 `WorkingDirectory` 与目标运行目录一致后，自动在更新/测试期间临时停止服务（防止自动重启干扰构建或测试），结束后自动重新拉起；未安装 `systemd` 或指向其他隔离目录时保持原 `mozart-ctl down / up` 行为。

部署要求 `mozart.toml` 已配置 `[web]` 段。`mozart-web-adapter` 由 `daemon` 统一监管（日志位于 `run/logs/web.log`）；网页 `system/down` 或 `mozart-ctl services stop` 仅停业务子服务，`daemon` 与页面保持在线。整机停机在未托管时走 `mozart-ctl down`（启用 `systemd` 托管时走 `sudo systemctl stop mozart.service`），单独重启适配器走 `mozart-ctl restart web`。

默认配置监听 `8080` 高端口，无需 `cap_net_bind_service` 或免密 `sudo` 即可启动。若手动配置低端口（`< 1024`），脚本才通过 `sudo -n setcap` 授予 `cap_net_bind_service`；没有该权限时请保持高端口配置。服务以运行目录所有者启动，不以 root 运行；绑定后、创建工作线程前清除进程 capability。设备需提供 `jq` 和 libcap 工具。

`deploy.json` 记录提交号、dirty 标记、时间、构建预设、部署模式（默认 `incremental`，指定 `--full` 时为 `full`）和 `systemd` 托管标记；部署期间整栈重启。适配器实测范围见[适配器报告](../reports/web-adapter-20261117/RESULTS.md#适用范围)。`30d0eaf` 基线的板端实测覆盖配置拦截与停栈后异常退出回滚。`cf8c800` 已支持此前被拒绝的 `bridge.llama_model_id`；新栈部署验收见[部署加固报告](../reports/deploy-hardening-20261126/RESULTS.md#适用范围)。

### 仅前端部署

`tools/deploy-web.sh` 只更新静态资源：主机构建 web（按需 `npm ci`）→ 备份设备 `web/dist` 到 `web.prev` → `rsync --checksum --delete` 替换运行位 `web/dist`。适配器按请求读盘，替换后下一个请求即生效，不编后端、不停栈、不重启。适用于只改 `web/` 且不动接口的情形；改了适配器路由或 daemon 控制面 schema 仍须走 `deploy-jetson.sh`。SSH 目标由参数或 `MOZART_JETSON` 提供，`MOZART_RUNTIME_DIR` 覆盖运行目录。手动替换不走全栈部署的事务回滚，失败时可用 `web.prev` 手动恢复。

### 部署后测试

`tools/jetson-e2e.sh` 支持套件选择；不指定时默认 unit + smoke + file + language。

| 套件 | 脚本 | 内容 |
| --- | --- | --- |
| `--unit` | `tools/jetson-unit-test.sh` | 设备端 `cargo test --workspace` + `ctest` |
| `--smoke` | `tools/jetson-smoke.sh` | daemon + pre + rvc 三进程冒烟 |
| `--file` | `tools/jetson-file-test.sh` | 文件转换、取消、队列与入口校验 |
| `--file-models` | `tools/jetson-file-multimodel.sh` | `models/rvc` 下全部模型的文件转换与帧守恒 |
| `--preset` | `tools/jetson-preset-test.sh` | 模型预设首次自动建、save/use/list、实时载入、A→B→A 切换、加载失败与损坏预设保护、资产隔离（需两个模型，`MODEL_B` 可指定） |
| `--realtime-mm` | `tools/jetson-realtime-mm.sh` | 实时多模型：跨采样率 `model use`、overlap 定值拒绝、`startup_delay` 可调、失败恢复、40 kHz 文件链（需两个采样率不同的模型，`MODEL_A`/`MODEL_B` 可指定） |
| `--language` | `tools/jetson-language-test.sh` | ASR → 翻译 → TTS 播报 |
| `--measure` | `tools/jetson-measure.sh` | 延迟、淡变、长跑、自愈测量 |

示例：

```bash
tools/jetson-e2e.sh <SSH目标> --unit
tools/jetson-e2e.sh <SSH目标> --smoke --file
tools/jetson-e2e.sh <SSH目标> --measure latency
```

功能套件开始前停止运行时，结束后自动重启。measure 需要 release 构建，部署只产 debug。
证据目录：`/tmp/mozart-smoke`、`/tmp/mozart-file-test`、`/tmp/mozart-multimodel`、`/tmp/mozart-language-test`；measure 证据在 `~/mozart-t7`。

### 本机音频桥接

`tools/jetson-audio.py` 通过 SSH 在本机与设备之间传输 48 kHz 双声道 16 位 PCM（每帧 20 ms），支持 Linux、Windows 与 macOS。本机侧直连系统默认扬声器与默认麦克风，不创建虚拟麦克风；通过 `uv run` 自动加载 `sounddevice` 依赖。

前置条件：本机到设备已配置 SSH 免密登录，设备上已加载 `snd-aloop`；Linux 主机需已安装系统 PortAudio 共享库（如 `libportaudio2` 或 `portaudio`）。脚本默认使用空闲子设备 `SUBDEV=6`（采集）和 `SUBDEV=7`（播放），避免占用测试套件的 `0~5`；如已在 Loopback 上则直接复用对端，否则通过 `mozart-ctl device use` 临时热切并在退出时还原（[验收边界见 SPEC-09](SPEC/SPEC-09-pipeline.md#验收)）。

```bash
# 听设备播放输出（默认 play 模式）
MOZART_JETSON=<SSH目标> uv run tools/jetson-audio.py

# 只把本机默认麦克风送入设备采集
uv run tools/jetson-audio.py <SSH目标> --mode mic

# 双向全双工试听，并同步保存下行 WAV
uv run tools/jetson-audio.py <SSH目标> --mode duplex --record out.wav

# 跳过控制面热切，按指定缓冲直连目标 Loopback 子设备
uv run tools/jetson-audio.py <SSH目标> --no-switch --pb-sub 7 --buffer-ms 80

# 指定隔离运行时目录或配置文件
uv run tools/jetson-audio.py <SSH目标> --runtime-dir ~/my-runtime --config ~/my-runtime/mozart.toml
```

## 资产布局

原料层放待处理内容，成品层放可直接推理的整套资产。

| 层 | 路径 | 内容 |
| --- | --- | --- |
| 原料 | `~/rvc-pth/` | 原始权重 `<id>.pth`，待导出编译 |
| 成品 | `~/mozart-assets/models/rvc/<id>/` | config + 生成器 + 特征引擎引用 + index |

成品目录名 = 模型 id = 文件前缀。加载器按 `<id>` 前缀找文件，同名 `.engine` 优先于 `.onnx`。
每个 RVC 模型目录内的 `presets/default.toml` 保存该模型的五个会话参数；首次加载自动创建，之后由 `preset save` 更新。`presets/` 是资产树内唯一可写目录。
设备上现有模型以 `mozart-ctl model list --available` 为准，不在文档中维护清单。

### 共享特征引擎

HuBERT/RMVPE 所有 v2 模型共用，放一处，其余模型引用，不每份复制。文件名约定与自动发现规则见 [config.md](config.md) 与 [SPEC-03](SPEC/SPEC-03-rvc.md)；`[rvc]` 的 `hubert_path` 等显式指向共享目录。换模型只换生成器，特征引擎不动。

### 其他资产

`~/mozart-assets/models/` 下另有 `asr/`、`asr-review/`、`tts/`、`llm/`，供语言链（识别、播报、翻译）使用。

## 运行配置

`~/mozart-runtime/mozart.toml` 是唯一手改入口。字段定义见 [config.md](config.md)。

适配器使用同一配置；以下路径相对该配置文件目录：

```toml
[web]
static_dir = "web/dist"
upload_dir = "state/web-uploads"
```

按实际访问地址配置 `allowed_origins`。适配器由 daemon 以同一运行用户拉起，高端口默认配置与低端口权限见[部署行为](#部署行为)，网络访问控制由部署提供。运行期子进程环境变量（如 `LD_LIBRARY_PATH`）统一以 [process.env](config.md#processenv) 为唯一来源。
手动文件变声 harness：`~/mozart-runtime/run-remote.sh`，执行 `ctl up → file submit → file fetch`。

当前运行配置只启用 pre、rvc 与文件转换。未配置 `[process.env]`、`[asr]`、`[tts]`、`[bridge]` 时，`status` 显示 asr/tts/llama 降级属预期；语言链由 e2e 的临时配置验证。

## 工具链

Python 脚本，居仓库 `tools/model/`，直接调用，不做 CLI 封装。
导出/golden 在 `~/mozart-toolkit/rvc-env` 的 venv 里跑（固定版本，transformers 4.46.3；首次搭建见 skill `mozart-model-onboarding` 的 references/environment.md）。
原则：**逻辑在仓库，资产在仓外，路径全部由参数或环境变量注入**，脚本不写死资产位置。

| 脚本 | 作用 |
| --- | --- |
| `export_generator_onnx.py` | 生成器 pth → onnx（离线，固定帧数） |
| `export_generator_split_onnx.py` | 生成器 pth → front/decoder onnx（实时拆分） |
| `export_hubert_onnx.py` | HuBERT base → onnx |
| `export_rmvpe_onnx.py` | RMVPE → onnx |
| `build_dynamic_engines.py` | 文件链 onnx → engine（档位预设、trtexec 包装，须在设备上） |
| `build_realtime_engines.py` | 实时定长 onnx → engine（front/decoder/hubert/rmvpe，设备上） |
| `run_reference.py` | PyTorch 参考推理，保存各模型输入张量 |
| `run_streaming_reference.py` | 流式 PyTorch 参考（golden 流式对拍，24 帧窗） |
| `verify_golden.py` | golden 对拍校验 |
| `compare_streaming_quality.py` | 流式输出质量对比（verify_golden 调用） |

### 外部资产依赖

工具链本身不含资产，运行需要以下外部输入：

| 依赖 | 用于 | 传入方式 | 位置 |
| --- | --- | --- | --- |
| RVC 源码树 | 导出、golden | `--rvc-source` / `RVC_SOURCE` 必填 | `~/mozart-assets/RVC` |
| HuBERT base 模型 | export_hubert、run_reference、run_streaming_reference | `--hubert-path` 必填 | RVC 归档 assets |
| RMVPE 权重 | export_rmvpe、run_reference、run_streaming_reference | `--rmvpe-path` 必填 | RVC 归档 assets |
| 目标权重 | export_generator、run_reference | 位置参数 / `--model` | `~/rvc-pth/<id>.pth` |
| golden 参考 | run_reference、verify_golden | `--input` / `--manifest` 等必填 | 仓外 |

导出依赖 PyTorch，在开发机运行；编译依赖 trtexec，在设备运行。RVC 源码树是上游实现，不是旧仓库代码。`build_dynamic_engines.py` 从生成器 ONNX 读取 emb/latent/source 维度，按 `--tier` 选择 profile，`--model-config` 提供采样率并校验 emb。`trtexec` 可用 `TRTEXEC` 或 `--trtexec` 指定，两个构建脚本相同。

### 调用示例

```bash
# 导出离线生成器（开发机）
python3 tools/model/export_generator_onnx.py ~/rvc-pth/qiqi-zh.pth \
    --rvc-source ~/mozart-assets/RVC --output qiqi-zh.onnx --frames 200

# 导出实时拆分生成器（帧数须匹配实时契约：front 280 / decoder 30）
python3 tools/model/export_generator_split_onnx.py ~/rvc-pth/qiqi-zh.pth \
    --rvc-source ~/mozart-assets/RVC \
    --front-output qiqi-zh-front.onnx --decoder-output qiqi-zh-decoder.onnx \
    --front-frames 280 --decoder-frames 30 --skip-head 250 --return-frames 30

# 编译引擎（设备；默认 large 档，采样率从模型 config.json 读取）
python3 tools/model/build_dynamic_engines.py \
    --generator qiqi-zh-dynamic.onnx --generator-engine qiqi-zh.engine \
    --feature-assets <特征onnx目录> --model-config <模型目录>/config.json

# 查看档位
python3 tools/model/build_dynamic_engines.py --list-tiers

# 只重建共享特征引擎（所有文件链模型共用）
python3 tools/model/build_dynamic_engines.py --feature-only \
    --feature-assets <特征onnx目录> --tier large

# golden 参考与对拍
RVC_SOURCE=~/mozart-assets/RVC python3 tools/model/run_reference.py \
    --input in.wav --model ~/rvc-pth/qiqi-zh.pth \
    --hubert-path <hubert_base目录> --rmvpe-path <rmvpe.pt> \
    --output ref.wav --tensor-dir tensors --metadata reference.json
python3 tools/model/verify_golden.py --manifest golden_manifest.json

# 流式参考（RVC_SOURCE 必填；24 帧窗，输出与 tensor 目录由参数指定）
RVC_SOURCE=~/mozart-assets/RVC python3 tools/model/run_streaming_reference.py \
    --input in.wav --model ~/rvc-pth/qiqi-zh.pth \
    --hubert-path <hubert_base目录> --rmvpe-path <rmvpe.pt> \
    --output stream.wav --tensor-dir tensors
```

### 文件链档位

`--tier` 选择一组默认 profile，默认值由 JSON 的 `default` 字段给出。档位是默认值，不是显存硬限制；脚本仍会校验形状上下限和 RMVPE 的 32 倍数。

档位数值的单一来源是 [`tools/model/file_chain_tiers.json`](../tools/model/file_chain_tiers.json)；当前值用 `--list-tiers` 查看。字段语义、公式和边界见 [SPEC-12](SPEC/SPEC-12-file-chain-params.md)。

- 文件链 HuBERT/RMVPE 是共享引擎，所有模型共用一套。提高全局档位时，先用 `--feature-only` 重建共享特征引擎，再重建各模型生成器。
- 模型生成器的 max 不应超过共享 HuBERT 的 max；超过时运行时会回退 ONNX 特征。HuBERT max 须覆盖分窗窗输入 `align_up(160T+80, 320)`（T 为生成器 max 帧数，公式镜像 `inferencer.cpp` `offline_window_geometry`），否则逐窗回退 ONNX。
- `max` 档未在板端验证。阻塞原因：40 s 窗的构建耗时和峰值显存未测。解除条件：在 8 GB 设备上按 `max` 档构建并跑 30/60 s 文件，确认峰值低于 5.8 GB。
- 静态 ONNX 输入不套用档位，按固定形状编译。
- 有生成器时，`build-manifest.json` 写在生成器 engine 同目录；仅 `--feature-only` 时写在特征 engine 目录。记录档位、profile、采样率来源、命令和哈希。

### 实时链路资产生成

实时链路用 4 个定长引擎，与文件路径的动态引擎不同。契约形状（`rvc/src/rvc/pipeline.cpp`）：

| 引擎 | 定长形状 |
| --- | --- |
| 生成器 front | feats `1×280×768` |
| 生成器 decoder | z `1×192×30` → audio |
| hubert-realtime | audio `1×44800` |
| rmvpe-realtime | mel `1×128×32` |

几何来源：窗 3840 样本、过去上下文 40000、淡化 2400、SOLA 搜索 480（`rvc/src/rvc/audio_worker.cpp`）。front 280 帧 = (40000+3840+960)/160；decoder 30 帧 = out_block 14400/480；skip-head 250 = 40000/160。

生成步骤：

1. 导出 front/decoder（每模型）：`export_generator_split_onnx.py --front-frames 280 --decoder-frames 30 --skip-head 250 --return-frames 30`。
2. 导出实时特征引擎（共享，一次）：`export_hubert_onnx.py --samples 44800`、`export_rmvpe_onnx.py --frames 32`。
3. 设备上编译定长引擎：

```bash
python3 tools/model/build_realtime_engines.py \
    --front qiqi-zh-front.onnx --decoder qiqi-zh-decoder.onnx \
    --hubert hubert-realtime.onnx --rmvpe rmvpe-realtime.onnx \
    --output-dir <成品目录>
```

`build_realtime_engines.py` 先校验 onnx 输入为定长，再由 trtexec 从模型读取形状；模型相关维度（inter_channels、source_dim）不写死。`build_dynamic_engines.py` 只管文件路径动态引擎，不覆盖实时。

## 加新模型流程

1. 权重落入 `~/rvc-pth/<id>.pth`。
2. 开发机运行导出脚本，产出动态 onnx（`--dynamic-time`）。
3. 设备上用 `--tier large --model-config <模型目录>/config.json --generator-only` 编译生成器；共享特征引擎只在校准全局档位时重建。
4. 成品层建 `<id>/` 目录，放 config 与生成器，登记 `[[models]]`。
5. 测试：`file submit --model <id>`，按需 golden 对拍。

## 适用范围

- 工具链脚本只做语法检查；功能运行需 PyTorch 与 RVC 源码树，未在本文档环境实跑。
- 目录规范与部署、测试流程于 2026-11-15 调整；旧报告中的路径不对应设备现状。
