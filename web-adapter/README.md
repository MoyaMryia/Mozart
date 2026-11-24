# HTTP 适配器

`mozart-web-adapter` 经 HTTP/SSE 托管前端静态文件、REST、服务器发送事件（SSE）和文件上传下载，在配置 `[web]` 时由 `daemon` 统一监管。全部控制操作（包括对应业务子服务组启停的 `system/up` 和 `system/down`）均经 `daemon.sock` 中继，不调起 `mozart-ctl` 子进程。接口契约和命令面欠账见 [SPEC-06](../docs/SPEC/SPEC-06-web.md)。

## 运行与配置

适配器与 daemon 使用同一运行用户和配置。`--config` 指定配置文件（由 daemon 拉起时自动传入）；省略时采用 core 的配置选择顺序。配置字段见 [config.md](../docs/config.md)，设备目录与配置示例见 [SETUP.md](../docs/SETUP.md)。

停业务期间（`system/down` 或 `mozart-ctl services stop`）daemon 与适配器保持在线；整机完全关停走 `mozart-ctl down`。上传暂存目录由实例独占；提交结果不明时保留源文件和恢复记录，直至确认安全回收。SSE 客户端有界缓冲，通过 `Transport` 唤醒并中止溢出的 TCP 连接。Origin 和 Host 检查不代替用户鉴权；部署负责网络访问控制。

## 检查

```bash
tools/ci-local.sh adapter
```

该阶段构建真实 daemon、真实 ctl、真实 `mozart-web-adapter` 和 core 示例，并运行适配器检查与测试。`web-adapter/tests/daemon.rs` 的两个集成测试使用真实 daemon 拉起真实适配器、真实 ctl 和协议桩；文件转换结果来自桩，不是实际 RVC 推理。设备另做了真实浏览器、RVC 文件与语言链联调，见[实测报告](../reports/web-adapter-20261117/RESULTS.md)。部署与运维入口见 [SETUP](../docs/SETUP.md#部署行为)，完整产品验收仍按 [SPEC-06](../docs/SPEC/SPEC-06-web.md#七实现与验收待办)。

`--print-config` 只验证并输出生效的启动配置（含 `configured` 标记），不建立服务；已移除 `--ctl-bin` 与 `--ready-file`。低端口 capability 只用于同步绑定，创建工作线程前清除。
