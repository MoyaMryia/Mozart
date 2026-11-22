# HTTP 适配器

`mozart-web-adapter` 是独立 Rust 进程。它同源托管前端静态文件、REST、服务器发送事件（SSE）和文件上传下载。业务操作经 daemon 控制 socket；`system/up` 和 `system/down` 调用可信的 `mozart-ctl`，不接受客户端指定可执行文件。接口契约和命令面欠账见 [SPEC-06](../docs/SPEC/SPEC-06-web.md)。

## 运行与配置

适配器与 daemon 使用同一运行用户和配置。`--config` 指定配置文件；省略时采用 core 的配置选择顺序。`--ctl-bin` 指定可信的 CLI 可执行文件；省略时使用适配器二进制同目录的 `mozart-ctl`。配置字段见 [config.md](../docs/config.md)，设备目录与配置示例见 [SETUP.md](../docs/SETUP.md)。

适配器独立于 daemon 生命周期。上传暂存目录由实例独占；提交结果不明时保留源文件和恢复记录，直至确认安全回收。SSE 客户端有界缓冲，通过 `Transport` 唤醒并中止溢出的 TCP 连接。Origin 和 Host 检查不代替用户鉴权；部署负责网络访问控制。

## 检查

```bash
tools/ci-local.sh adapter
```

该阶段构建真实 daemon、真实 ctl 和 core 示例，并运行适配器检查与测试。`web-adapter/tests/daemon.rs` 的两个集成测试使用真实 daemon、真实 ctl 和协议桩；文件转换结果来自桩，不是实际 RVC 推理。设备另做了真实浏览器、RVC 文件与语言链联调，见[实测报告](../reports/web-adapter-20261117/RESULTS.md)。部署与独立启动入口见 [SETUP](../docs/SETUP.md#部署行为)，完整产品验收仍按 [SPEC-06](../docs/SPEC/SPEC-06-web.md#七实现与验收待办)。

`--print-config` 只验证并输出生效的启动配置，不建立服务。`--ready-file` 供可信启动脚本确认监听已建立；HTTP 请求不能指定该路径。低端口 capability 只用于同步绑定，创建工作线程前清除。
