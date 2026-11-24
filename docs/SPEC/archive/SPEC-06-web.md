# SPEC-06 归档（web）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-06-web.md](../SPEC-06-web.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 全量归档

范围：「七、实现与验收待办」15 条已完成 `[x]`（实现、修复、迁移、构建与测试通过记录）。3 条契约条款去勾选后留在源文件，17 条活跃 `[ ]` 逐字保留。链接已按归档目录调整深度。

### 七、实现与验收待办

- [x] 默认监听地址改为 `0.0.0.0:8080`，默认 `allowed_origins` 同步带 `:8080`；保留手动配置低端口（`< 1024`）时的 `cap_net_bind_service` 兼容路径。（`core/src/config.rs`、`tools/deploy-jetson.sh`）
- [x] 上传目录首次不存在时由适配器创建并规范叶目录为 `0700`；已存在且不安全（符号链接、非目录、属主不符或权限宽于 `0700`）时仍拒绝且不修改其权限，并在启动报错中打印具体目录路径与实际权限/原因。（`web-adapter/src/uploads.rs`）
- [x] 迁入 `daemon` 统一监管并收敛启停入口（方案 1）：移除独立 `tools/web-service.sh`，改为由 `daemon` 按同一 `mozart.toml` 拉起和监管 `mozart-web-adapter`；`POST /system/up` 与 `POST /system/down` 改为透传 `DAEMON_SERVICES_START` / `DAEMON_SERVICES_STOP`，移除 `--ctl-bin` 与 `--ready-file` 及子进程 CLI 调起路径。（`web-adapter/src/{main,lib,routes}.rs`、`web-adapter/tests/{http,daemon}.rs`、`tools/ci-local.sh`）
- [x] Rust 检查显式构建 daemon、ctl 和协议桩；适配器集成测试使用 Cargo 提供的正式产物路径，不依据测试可执行文件所在目录。使用不同的 `CARGO_TARGET_DIR` 和 `CARGO_BUILD_BUILD_DIR` 从空目录运行 `tools/ci-local.sh adapter`，两项真实 daemon 集成测试通过。
- [x] 部署回滚恢复原先不存在的源码目录；Web 服务状态查询不修改运行目录权限。
- [x] 修复资源读取失败后的写入限制和未知结果确认，补充前端回归测试。
- [x] 将已保存的 Web 后端与部署改动迁移到当前主线，保留增强和 ASR 修复，并验证未知写入不重试。
- [x] 创建 `web/` Vue 工程，拆分展示组件，接入新 REST/SSE 客户端和本机检查入口。
- [x] 创建 Rust 适配器并接入共用配置。（`web-adapter/src/main.rs`、`core/src/config.rs`）
- [x] 接入设备构建部署入口；标准脚本整链的当前阻塞与已测范围见[报告](../../../reports/web-adapter-20261117/RESULTS.md#适用范围)。
- [x] 在系统面板显示增强进程与工作状态，并支持重启增强服务。
- [x] 补齐 SPEC-04 的生效上传上限与轮询间隔查询，再接入对应 HTTP 路由。（`web-adapter/tests/daemon.rs`）
- [x] 测试适配器存活期间执行 down → 页面仍可达 → up → 状态恢复。（[真实浏览器联调](../../../reports/web-adapter-20261117/RESULTS.md)）
- [x] 使用未声明下载安全能力的旧 daemon，核对下载明确拒绝且不透出本机文件路径。（`web-adapter/tests/http.rs`、`rpc.rs`）
- [x] 测试未知路由、方法、正文类型、同源限制、目录穿越与下载文件名编码。（`web-adapter/tests/http.rs`）