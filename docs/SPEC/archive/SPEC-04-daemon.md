# SPEC-04 归档（daemon）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-04-daemon.md](../SPEC-04-daemon.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 全量归档

范围：「已确认」1 项 `[x]`（CLI 实现选型）、「程序集成待实现补充」1 项 `[x]`（截止时间共用逻辑提取）、「改进点」7 项 `[x]`（模式名统一、端口集中、Web 监管落地、停服收尾与竞态收口、角色集合单一来源、测试清理）。命令号与体、配置键与阈值、socket 布局、编排判据与回退语义等契约类 `[x]` 去勾选后保留在源文件，不归档。链接已按归档目录调整深度。

### 已确认

- [x] 实现：clap + serde_json；`mozart-ctl` 独立 crate，与 daemon 共用 core 的协议定义。网页适配器经控制 socket 接入，进程监管归 daemon（方案 1）；硬件控制后续复用协议。

### 程序集成待实现补充

- [x] 将 CLI 普通请求、配置写入和启动停止的截止时间计算提取为共用逻辑，供适配器复用。
  保持现有 CLI 预算，不因增加 HTTP 层缩短控制请求时间。

### 改进点

- [x] 模式名与状态字段统一。
- [x] 端口集中配置。（`core/src/config.rs`）
- [x] 统一监管 Web 适配器（方案 1）：将 `mozart-web-adapter` 纳入 `Supervisor`（`Role::Web`，读取 `[process].web_bin` 与可选 `[web]` 配置，随 daemon 拉起、崩溃退避重启、`PDEATHSIG` 孤儿防护、日志落 `run_dir/logs/web.log`）；`GetStatus.processes` 上报 `web` 状态，支持 `restart web`；`daemon` 自身仍不内置 HTTP，适配器继续经 `daemon.sock` 接入。（`core/src/config.rs`、`core/src/ctl/schema.rs`、`daemon/src/{state,supervisor,server}.rs`、`ctl/src/main.rs`）
- [x] 停服超时收尾：`services stop` 超时只记降级原因并返回 `Internal`，业务进程晚于截止时间停完后由 `poller` 补做收尾（`State::finalize_services_stopped`），重试命中 `not-running` 分支同样收尾。收尾清运行时快照与业务生命周期留下的持久降级原因（停服超时、实时/文件租约归还、播报停止屏障、RVC 会话参数），这些原因在租约与屏障复位后不会再有事件来清。`services start` 走同一收尾并重置 `restore.speech_stop`，避免陈旧播报停止屏障打到新起的服务；停服期间 `config set rvc.*` 只更新生效配置，不打已停的 rvc。（`daemon/src/{server,poller,state,services}.rs`，回归 `state::tests::stop_timeout_finalize_clears_reason_and_runtime`）
- [x] 停服期准入与竞态收口：`file submit` 在 `submit_lock` 内先校验停服并回 `NotReady`（带与不带 `model_id` 一致），只入队不起流故不取 `transition`；已有排队任务保留不动，恢复服务后按原仲裁继续。停服校验统一读锁内 `state.services_active`：`mute set` 与 `preset save|use|list` 的校验移入各自 `transition` 临界区，`translate set` 与 `model list` 在 handler 先取 `transition` 再校验。（`daemon/src/{server,services,preset,state}.rs`、`daemon/src/file/mod.rs`，回归 `daemon/tests/{control_plane,file_worker}.rs`）
- [x] 业务角色集合单一来源：新增 `Role::ALL`，`State::all_business_stopped` 与监管拉起都按角色取进程信息，业务边界只由 `Role::is_business` 定义。（`daemon/src/{state,server}.rs`）
- [x] （测试）删除 `web-adapter/tests/daemon.rs` 的恒真断言 `assert!(!h.config.as_os_str().is_empty())`，并移除只为它保留的 `Harness.config` 字段。