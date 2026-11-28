# SPEC-09 部署加固实测（2026-11-26）

基于 `30d0eaf` 的增量发布未通过。该基线的 daemon 不识别设备配置中的 `bridge.llama_model_id`。已恢复原运行栈；未修改配置，未发布新的 `deploy.json`。systemd 托管下的冒烟停复通过。

## 增量部署与回滚

开发机执行 `git fetch origin`，在 `30d0eaf` 上创建 `feat/spec09-deploy-hardening`。部署前检查 `systemctl is-active mozart.service`、进程列表及配置摘要。原栈为 `active`。历史快照与事务目录先打包至设备主目录的 `spec09-before-prune-1795667197.tgz`。

前三次执行 `MOZART_JETSON='<SSH目标>' bash tools/deploy-jetson.sh`，均返回 1。下表阶段耗时取脚本起止标记；总耗时取主机 `time` 的 `real`。

| 事务后缀 | 网页 | 同步与停栈 | 设备构建 | 结果 |
| --- | ---: | ---: | ---: | --- |
| `PBrhop2M` | 3 s | 7 s | 22 s | 安装后健康检查失败。恢复 `bin.prev` 后，systemd 启动限流拦截了第一次重启。 |
| `DIKFfZC3` | 2 s | 6 s | 9 s | 安装前拒绝配置，自动恢复服务。全程 18.415 s。 |
| `PqY4fjYw` | 2 s | 8 s | 21 s | 启用阶段锁后复测，安装前拒绝配置，自动恢复服务。全程 32.108 s。 |

第一次 journal 与后两次 `config validate --json` 均报 `unknown field 'llama_model_id'`（TOML 第 118 行）。第一次回滚后，`bin/mozart-daemon` 与 `bin.prev/mozart-daemon` 的 SHA-256 相同。执行 `sudo -n systemctl reset-failed mozart.service`、`sudo -n systemctl start mozart.service` 恢复原栈。后两次无需人工启动。

最后一次回滚后，按同步排除项执行 `rsync -rnc --delete --itemize-changes` 比对快照和源码，差异为 0 项。复测时 Ninja 输出 `no work to do`。部署前后 `sha256sum ~/mozart-runtime/mozart.toml` 均为 `c4ef5ce137f589f678b1541e0a86927c52f208993e27a5b1c7804bba5d7f9352`。`cmp` 确认 `deploy.json` 与部署前一致（原值 `mode=full`、`systemd=true`）。

## 锁、探活与保留数量

- 锁：板端打开 fd 9 并执行 `flock -n 9`。持锁期间执行 `flock -n ~/.mozart-deploy.lock true` 返回 1，关闭 fd 后返回 0。另用独立 SSH 管道持同一把锁；对本机持有管道写端的 shell 发送 `SIGKILL`，1 s 后重试返回 0。
- 探活：单独在恢复的原栈上执行 INSTALL 的状态与 HTTP 检查片段。部署前状态取最后一次事务记录，是否配置模型取运行中 ctl 的配置读数。输出 `运行时健康检查通过：/api/v1/health HTTP 200，status=ok`。
- 访问源：由 `ssh -G '<SSH目标>'` 取得主机名；用新适配器 `--print-config` 的实值运行告警片段。输出 `告警：[web].allowed_origins 缺少访问源 http://<目标主机>:8080；未修改 mozart.toml`（地址已隐去）。
- 清理：真实部署未标记 `completed`。源码快照由 15 份增至 18 份；当前用户的事务目录由 1 份增至 4 份。未为达到数量要求手工删除失败证据。另在板端临时目录执行同一 `prune` 函数：5 份模拟快照保留 `s3`、`s4`、`s5`；5 份历史事务加当前事务保留 `active`、`r4`、`r5`。

## systemd 冒烟停复

主机执行 `time MOZART_JETSON='<SSH目标>' bash tools/jetson-e2e.sh --smoke`，返回 0，耗时 25.136 s。journal 记录 `12:29:17 Stopping`、`12:29:19 Stopped`、`12:29:41 Started`。冒烟测试执行路由切换、强杀 RVC 和恢复检查，输出 `no mozart processes`、`全部选中套件通过`、`运行时已恢复（systemd）`。

恢复稳定后执行 `~/mozart-runtime/bin/mozart-ctl --config ~/mozart-runtime/mozart.toml status`。pre、rvc、asr、tts、bridge、llama、web 共 7 项为 `running`，可选 enhance 为 `stopped`。RVC 为 `streaming`，末行为 `degraded -`。最后一次部署回滚后再次确认这 7 项状态。

## 事务修复补验

- 部署前，将 `spec09-before-prune-1795667197.tgz` 移至仓外 `/var/tmp` 备份目录。设备主目录已无匹配文件。原服务为 `active`，7 项运行，`degraded -`。
- 再次执行增量部署（事务 `WkJfEHb3`），退出码 1。新 ctl 在安装前拒绝 `llama_model_id`。脚本报告回滚完成，无需人工启动；复查 7 项运行，`degraded -`。
- 另一次增量部署（事务 `rnkrMLha`）停栈后，对本机部署 shell 发送 `SIGKILL`，退出码 137。板端存在 `initialized` 和 `stopped`，不存在 `completed`。holder 读到 EOF 后报告恢复完成；后到的构建阶段被拒绝，部署锁可重新获取。`deploy.json` 与事务备份一致。重启初期状态连接尚未就绪；稳定后 `mozart.service` 为 `active`，7 项运行，RVC 为 `streaming`，`degraded -`。
- 复审后单独验证停栈基线校验：空输出、空进程表和缺少 `state` 的条目均返回非零。设备原栈实时 `status --json` 通过校验，含 7 项运行进程。
- 若回滚启动遇 systemd 限流，按提示人工执行 `sudo systemctl reset-failed mozart.service && sudo systemctl start mozart.service`，再检查原栈状态。

## 适用范围

- 设备：单台 Jetson Orin Nano Super 8 GB，systemd 托管，Rust debug 与 CMake `jetson-release`。共 5 次增量部署尝试（含 1 次主动强杀），0 次成功发布，1 次原栈冒烟。
- 代码核对：分支已 rebase 到 `cf8c800`。该基线已识别 `bridge.llama_model_id`，解除了上述未知字段阻塞。部署保留 `MOZART_AEC3_LIB_DIR` 构建导出，安装探活已适配默认直出及 `manual` 模式空输出模式，并在停服前记录 `stopping` 意图与 `deactivating` 回滚保护；以上适配当前仅完成主机代码与单测验证。
- 新栈完整部署尚无板端实测。阻塞原因：rebase 及上述适配后目标 Jetson 当前不在线，缺少设备运行窗口。解除条件：设备在线后执行一次增量部署与冒烟验收，核验新栈状态（含 `manual`/直出分支及 `stopping` 中断恢复）、HTTP 探活、`mode=incremental` 和成功事务清理。上述原栈和临时目录分项结果不计入新栈验收。
