# pipeline：开发与部署流水线

状态：讨论中。2026-11-15 起为待办清单，`[x]` 已实现、`[ ]` 未实现，记法见 [SPEC.md](SPEC.md)。
设备目录、部署与测试操作见 [SETUP.md](../SETUP.md)；本文只记录流水线边界、待办与验收口径。
`[x]` 条目详情已于 2026-11-24 移入 [archive/SPEC-09-pipeline.md](archive/SPEC-09-pipeline.md)，本文件保留契约、关键决策摘要与活跃待办。

## 目的

把构建、检查、部署、启动、观测、回滚和远程音频桥接收进本机入口，职责按命令分开：
本机全量测试、部署、部署后测试、本机音频桥接。

## 边界

- 覆盖开发机和单台 Jetson。
- 不覆盖云端 CI/CD、容器编排和多设备集群。
- 模型、音频和密钥仍在仓外，不进流水线产物。

## 已确认决策（摘要）

> 全文条目与证据见 [archive/SPEC-09-pipeline.md](archive/SPEC-09-pipeline.md)。与归档冲突时以归档原文为准并回来修正本节。

### DevOps 约定

- 职责分离：本机测试、部署、部署后测试各一条命令；`tools/ci-local.sh` 为本地与 CI 共用的单一检查源；重复执行幂等。
- 失败恢复：构建失败不动运行时；安装失败恢复 `bin.prev`。
- 配置外置：设备地址、模型路径和密钥不写进仓库；删除类操作要求显式参数，默认不覆盖资产。
- 运行期环境变量以 `mozart.toml` 的 `[process.env]` 为唯一来源；部署启动不再导出 shell `LD_LIBRARY_PATH`，编译期资产库环境变量保留。
- 部署与回滚统一管理单一运行时生命周期（方案 1）：未装 `systemd` 时经 `mozart-ctl down` / `up`，设备已启用 `mozart.service` 且 `WorkingDirectory` 匹配目标运行目录时自动改走 `systemctl stop` / `start`；已移除独立 web-service 安装与启停步骤，安装前校验 `[web]` 已配置、健康检查核验 `processes.web`、低端口对适配器做 `setcap` 兼容；旧版 `mozart-web-service` 保留单次停服/回滚迁移兼容。
- 部署提速：C++ 只编 `mozart-rvc` 与 `mozart-enhance`，Rust 用 `--example rvc_stub`，`[profile.dev] debug = "line-tables-only"`，分段计时；板端增量部署实测 36 s → 29 s。
- 按需 `npm ci`：以 `web/package-lock.json` 的 sha256 stamp 判定，lock 未变且 `node_modules` 存在时跳过，`--full` 强制重装。
- 可观测：状态接口、结构化日志；部署记录为 `deploy.json`。

### 验收

- 新环境按文档构建检查、一条命令部署启动并过健康检查、部署提交/时间/结果可查、全过程无需手工 rsync/ssh/重启均已通过；一条命令回滚与跨平台音频桥接验收仍在待办。

## 入口

| 命令 | 作用 | 状态 |
| --- | --- | --- |
| `tools/ci-local.sh all` | 本机全量测试：仓库卫生、文档、C++/CTest、Rust、web | 已实现 |
| `tools/deploy-jetson.sh <SSH目标>` | 同步、构建、安装、重启；不跑测试 | 已实现 |
| `tools/install-jetson-systemd.sh <SSH目标>` | 安装/更新/卸载 Jetson 开机定频与 `mozart.service` 自启单元 | 已实现 |
| `tools/deploy-web.sh <SSH目标>` | 仅前端：构建 web、备份并替换运行位 `web/dist`；不编后端、不停栈、不重启 | 已实现 |
| `tools/jetson-e2e.sh <SSH目标> [套件]` | 部署后测试：unit / smoke / file / language / measure | 已实现 |
| `tools/jetson-audio.py <SSH目标> [选项]` | 本机↔Jetson 跨平台 SSH 音频桥接（听板端输出、送本机麦、双向全双工、录 WAV） | 已实现 |

- SSH 目标由参数或 `MOZART_JETSON` 提供，仓库不保存设备地址。
- 设备端单测归 e2e 的 unit 套件；部署脚本不含测试门。

## 远程音频桥接

- 形态与依赖：单文件跨平台 Python 脚本（`tools/jetson-audio.py`，PEP 723 内联依赖 `sounddevice`），支持 Linux / Windows / macOS 经 `uv run` 或已装 `sounddevice` 的 `python3` 运行；直连系统默认扬声器与默认麦克风（`device=None`），不创建系统虚拟麦克风。配套零外部依赖单元测试 `tools/test_jetson_audio.py`。（`tools/jetson-audio.py`、`tools/test_jetson_audio.py`）
- 方向与格式：支持 `play`（默认，Jetson 播放 → 本机扬声器）、`mic`（本机麦克风 → Jetson 采集）和 `duplex`（双向全双工）；采用纯 SSH 原始 PCM 管道（`ssh -T -o Compression=no`，远端 `arecord`/`aplay`），两端固定 48 kHz、`S16_LE`、2 声道、20 ms 帧（3840 字节/帧），零重采样。（`tools/jetson-audio.py`）
- 参数与缓冲：SSH 目标由位置参数或 `MOZART_JETSON` 提供（必填）；支持 `--mode <play|mic|duplex>`、`--record <路径>`（仅 `play`/`duplex` 可用，标准库 `wave` 同步写 48 kHz 立体声 WAV，`mic` 模式传入在入口报参数错）、`--pb-sub` / `--cap-sub`（环境变量 `PB_SUB` / `CAP_SUB`，默认 `7` / `6`，范围 `0..=7`，避开测试套件占用的 `0~5`）、`--no-switch`、`--runtime-dir`（`MOZART_RUNTIME_DIR`，默认 `~/mozart-runtime`）、`--config`（`MOZART_REMOTE_CONFIG`，默认 `<runtime-dir>/mozart.toml`）、`--buffer-ms`（默认 `60`，范围 `20..=500` ms，按 20 ms 帧向上取整；本地有界队列上限 `max(目标帧数 × 2, 6)` 帧，满丢最旧防时钟漂移积压、空补零）。（`tools/jetson-audio.py`、`tools/test_jetson_audio.py`）
- 板端设备接管与故障处理：默认预检远端 `/proc/asound/Loopback`，查询 `mozart-ctl --config <路径> status --json` 的 `pre.playback_device` / `pre.capture_device`；已在 `Loopback` 且可解析 `DEV`/`SUBDEV` 时直接配对对端（`DEV 0 ↔ 1`）复用，否则经 `mozart-ctl device use` 热切到目标子设备并在退出时自动还原（`duplex` 半切失败立即回滚）；`--no-switch` 跳过控制面直连指定子设备。捕获 `SIGINT`/`SIGTERM`，SSH 断线、板端进程退出或声卡异常时显式报错并以非零码退出，收尾依次停本地音频流、停远端 SSH 子进程、关闭 WAV 文件并逆序还原已切换设备。（`tools/jetson-audio.py`、`tools/test_jetson_audio.py`）

## 阶段

1. [ ] 预检：工具链版本、SSH 连通、设备架构、L4T 版本、磁盘余量。现仅部署脚本检查 cargo/cmake/ninja 并告警磁盘余量。
2. 构建：主机预设用于测试；设备预设用于发布。
3. 检查：本机静态检查与单元测试；设备端单测由 e2e 执行。
4. [ ] 打包：记录 git 提交、构建时间、预设和资产版本。现 `deploy.json` 记录提交、dirty、时间与预设，未记录资产版本。
5. [ ] 部署：上传到 `releases/<版本>/` 并切换 `current`。现为固定运行位 `~/mozart-runtime/bin`，版本目录与原子切换未做。
6. 启动：部署后自动重启并等待 pre/rvc 健康。
7. 功能测试：e2e 默认冒烟、文件、语言；measure 可选。
8. [ ] 回滚：切回上一版本并重启。现仅失败自动恢复（源码快照 + `bin.prev`），无主动回滚命令。

## DevOps 约定

- [ ] 文档检查补全：句长与限制句检查（旧实现依赖 Python 脚本）；现只查行尾空白与报告“## 适用范围”。
- [ ] 原子发布：版本目录加 `current` 链接，失败不破坏上一版。
- [ ] 可回滚：保留最近若干版本，回滚一条命令。
- [ ] 可追溯：每次部署记录提交、时间、预设和结果。`deploy.json` 已记录提交、dirty、时间与预设。
- [ ] 方案 1 部署流板端实测：新部署流未在 Jetson 上跑过，`ci-local.sh` 只做语法检查，健康检查读 `processes.web` 也不代表 TCP 已监听（见下条）。解除条件：板端跑一次完整 `deploy-jetson.sh` 与一次回滚，核对 `processes.web`、页面可达与 `bin.prev` 恢复结果，证据落 `reports/`。
- [ ] 部署锁自动释放与回滚缓存保护：将 `tools/deploy-jetson.sh` 的目录锁 `~/.mozart-deploy.lock` 改为绑定会话生命周期的文件锁（`flock`），确保 SSH 断连或进程异常退出时自动释放不留死锁；回滚仅在源码确实发生回退且构建产物失效时清理缓存，取消无条件 `cargo clean` 与 `ninja -t clean`。
- [ ] 部署构建与健康检查加固：收紧部署健康检查（等配置模型时的 `rvc` 或 `output_mode` 离开过渡态，并核验 `web` 运行且 `/api/v1/health` 返回 `200`）；部署末尾按当前 SSH 目标 IP 与端口核对 `[web].allowed_origins`，缺失时明文告警提示而不擅改配置。板端 Rust 构建保持 debug（用户决定：release 全量与增量编译在 8 GB 设备上过慢），不改 `--release`。
- [ ] 部署工程卫生加固：部署成功后按上限（保留最近 `3` 份）自动清理历史 `~/.mozart-source-backup.*` 与 `/tmp/mozart-deploy.*` 快照；`deploy.json` 按实际构型写入 `mode`（`full` / `incremental`）。（注：停栈后最多 `3 s` 进程收割缓冲已随 `systemd` 适配在 `tools/deploy-jetson.sh` 中补齐。）
- [x] 轻量前端部署 `tools/deploy-web.sh`：纯前端改动不必走全栈部署。主机构建 web（按需 `npm ci`，复用 lock stamp）→ 备份设备 `web/dist` 到 `web.prev` → `rsync --checksum --delete` 替换运行位 `web/dist`；不编后端、不停栈、不重启（适配器按请求读盘，立即生效）。SSH 目标由参数或 `MOZART_JETSON` 提供，`MOZART_RUNTIME_DIR` 覆盖运行目录。接口（适配器路由或 daemon schema）变更仍须走 `deploy-jetson.sh`。板端实测：构建 628 ms，替换后适配器立即提供新哈希资源，web/daemon 进程号不变。
- [x] Jetson 开机定频与 `systemd` 自启模板及部署/E2E 适配：在仓库独立维护两个单元模板 `deploy/systemd/mozart-clocks.service.in`（开机在 `nvpmodel.service` 之后备份原调频状态并执行 `jetson_clocks`，默认保留自动调速风扇、可选 `--fan` 满载，停服恢复）与 `deploy/systemd/mozart.service.in`（在定频完成后以运行用户前台托管 `~/mozart-runtime/bin/mozart-daemon`，`KillMode=mixed` + `SIGTERM` 平滑退场，经 `--restart <on-failure|no>` 可选故障自动重启）；由 `tools/install-jetson-systemd.sh`（支持远程 SSH 落临时脚本 + 按需 PTY 执行，或 `--local`）按当前用户、动态探测二进制路径与运行目录渲染安装，并写入经 `visudo -cf` 校验的最小权限 `/etc/sudoers.d/mozart-deploy`。`tools/deploy-jetson.sh`（含回滚脚本）与 `tools/jetson-e2e.sh` 核对 `WorkingDirectory` 匹配后，对 `active|activating|reloading|deactivating` 状态的 `mozart.service` 自动改走 `systemctl stop / start` 并补最多 `3 s` 进程收割等待；未装 `systemd` 或指向隔离目录时回退 `mozart-ctl down / up`。
- [ ] `systemd` 服务板端实机验收：在 Jetson 实机执行 `tools/install-jetson-systemd.sh`、验证开机定频与 `mozart.service` 启停、并跑通一次 `tools/deploy-jetson.sh` 完整部署往返与一次 `tools/jetson-e2e.sh` 停复往返。阻塞原因：尚未将本分支部署到板端并安装系统级 unit；解除条件：用户确认提交后在 Jetson 实机完成安装、部署往返及服务状态核验。

## 待讨论

- 入口是否再收成 `tools/dev.sh`；当前命令已按职责拆分，暂不新增。
- 设备上原生构建，还是开发机交叉编译。
- 发布目录布局和保留版本数。
- 设备端单测是否并入部署动作；当前保持部署无测试。
- CI 是否增加 self-hosted Jetson runner。

## 验收

- [ ] 一条命令回滚到上一版。
- [ ] 一条命令建立本机与 Jetson 的单向或双向实时音频桥接，退出后自动还原板端原设备（Linux 开发机 ↔ Jetson 实机已验证 440 Hz WAV 端到端保真、`play` Loopback 复用、`duplex` 热切采集并还原原设备且无残留进程；Windows / macOS 真机链路未验证——阻塞原因：当前无 Windows / macOS 测试主机；解除条件：在 Windows 与 macOS 主机上各自实测一次 `uv run tools/jetson-audio.py` 正常建链与退出）。
