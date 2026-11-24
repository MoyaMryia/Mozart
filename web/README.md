# Web 控制台

Vue 3 + TypeScript + Vite。沿用旧控制台的亮暗主题与分栏布局，按新接口拆分组件。
业务请求统一使用 [SPEC-06](../docs/SPEC/SPEC-06-web.md) 的 `/api/v1`，不直接控制子进程或播放设备。
实现范围与验证记录见 [前端报告](../reports/web-console-20261116/RESULTS.md)。

## 开发

使用 Node.js 24。

```bash
npm --prefix web ci
tools/ci-local.sh web
npm --prefix web run dev
```

开发服务器默认仅监听本机。配置上游时启动代理：

```bash
MOZART_WEB_API_TARGET=http://127.0.0.1:8080 npm --prefix web run dev
```

生产构建输出到 `web/dist/`，由独立进程 [mozart-web-adapter](../web-adapter/README.md) 同源托管。
开发代理将 Host 与 Origin 归一到指定上游；不要把开发服务器作为设备公网入口。

## 浏览器检查

先安装 Playwright Chromium，或用 `MOZART_WEB_BROWSER_EXECUTABLE` 指定本机可执行文件。

```bash
npm --prefix web exec -- playwright install chromium
MOZART_WEB_BROWSER=1 tools/ci-local.sh web
```

浏览器检查只绑定本机随机端口，以 HTTP 契约夹具测试生产构建，不启动 daemon 或推理进程。
截图与结果保存在输出日志所示的临时目录，可用 `MOZART_WEB_EVIDENCE_DIR` 指定位置。

## 真实服务联调

`MOZART_WEB_LIVE_URL` 指定专用测试实例时，`tools/ci-local.sh web` 追加 `test:live`。
还须提供 `MOZART_WEB_LIVE_MODEL`、`MOZART_WEB_LIVE_INPUT`（2 s WAV）与 `MOZART_WEB_LIVE_AUDIO_GATE`。
控制器在门文件出现后注入真实音频；脚本会断网、停止和再次启动 daemon，不要指向正在使用的实例。
结果见[板端报告](../reports/web-adapter-20261117/RESULTS.md)。

## 代码

- `src/contracts.ts`：消费字段的类型与 HTTP/SSE 入口校验。
- `src/api.ts`：请求、错误信封、文件上传下载。
- `src/streams.ts`：命名 SSE、修订合并与内存游标恢复。
- `src/useConsole.ts`：查询、操作状态与组件接线。
- `src/components/`：不直接联网的展示组件。

设备和参数草稿不会被后台状态覆盖。配置响应保留待重启提示，不自动重启。
当前页面保留写请求断连后的未知结果与文件选择，不自动重发；取得新的队列快照并明确确认后才允许再次提交。
字幕游标只与内存读模型共存，刷新页面后从头恢复。淘汰记录保留按 ASR 启动划分的句号水位，旧修订不会重新变成当前字幕。
