# SPEC-09 归档（pipeline）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-09-pipeline.md](../SPEC-09-pipeline.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 全量归档

范围：「DevOps 约定」11 条 `[x]`、「验收」4 条 `[x]`。「边界」「入口」「远程音频桥接」的 `[x]` 属契约，已在源文件去勾选保留；「阶段」4 条编号条款是流水线行为契约，同样去勾选留在源文件；全部 `[ ]` 待办留在源文件。归档条目内无相对链接，无需调整深度。

### DevOps 约定

- [x] 职责分离：本机测试、部署、部署后测试各一条命令。
- [x] 单一检查源：`tools/ci-local.sh` 供本地与 CI 共用。
- [x] 幂等：重复执行不产生额外副作用。
- [x] 失败恢复：构建失败不动运行时；安装失败恢复 `bin.prev`。
- [x] 配置外置：设备地址、模型路径和密钥不写进仓库。
- [x] 运行期环境变量统一源：部署启动运行时不再额外导出 shell `LD_LIBRARY_PATH`，运行期共享库等子进程环境变量统一以 `mozart.toml` 的 `[process.env]` 为唯一来源（编译期仍保留资产库环境变量）；已在 Jetson 实机验证部署与全栈拉起通过。（`tools/deploy-jetson.sh`）
- [x] 合并运行时启停入口（方案 1）：`mozart-web-adapter` 迁入 `daemon` 监管后，已移除 `tools/deploy-jetson.sh` 中独立的 `mozart-web-service` 安装、准备与启停步骤，安装前校验 `[web]` 已配置、健康检查核验 `processes.web` 运行且低端口直接对 `mozart-web-adapter` 做 `setcap` 兼容；部署与回滚统一经 `mozart-ctl down` / `up` 管理单一运行时生命周期，同时保留旧版 `mozart-web-service` 的单次停服/回滚迁移兼容。本机证据限于 `bash -n` 与适配器集成测试，板端实测见下条。（`tools/deploy-jetson.sh`、`tools/ci-local.sh`、`docs/SETUP.md`）
- [x] 部署提速：C++ 侧限定 `mozart-rvc` 与 `mozart-enhance` 两个目标（不再随部署编测试程序），Rust 侧 `--example rvc_stub` 取代 `--examples`，`[profile.dev] debug = "line-tables-only"`，五阶段与 cargo/cmake 分段计时。板端同触发条件（touch 一个 Rust 源与一个 C++ 源）增量部署实测 36 s → 29 s：cargo 15 s → 9 s，cmake 7 s → 7 s（链接目标 9 → 1，但耗时集中在被改 `.cpp` 的编译，故本次无 wall time 收益）。profile 变更触发一次性全量重编 cargo 81 s。放宽 `--jobs` 与更换链接器留待后续数据支撑。
- [x] 按需 `npm ci`：以 `web/package-lock.json` 的 sha256  stamp（`web/node_modules/.mozart-npm-ci.sha256`）判定，lock 未变且 `node_modules` 存在时跳过安装，`--full` 强制重装。板端实测网页阶段 5 s → 3 s，空跑整次部署 12 s。
- [x] 安全默认：删除类操作要求显式参数；默认不覆盖资产。
- [x] 可观测：状态接口、结构化日志；部署记录为 `deploy.json`。

### 验收

- [x] 新环境按文档完成构建和检查（本机）。
- [x] 一条命令部署到设备并启动，健康检查通过。
- [x] 每次部署都能查到提交号、时间和结果（`deploy.json`）。
- [x] 全过程不需要手工 rsync、ssh 或重启命令。