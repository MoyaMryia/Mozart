# web：控制台

状态：讨论中。2026-11-15 起为待办清单，记法见 [SPEC.md](SPEC.md)。

保持 TypeScript/Vue，不改写。旧实现在仓外 Mozart-Old/frontend，仅归档参考；本仓 `web/` 未创建。

## 已定

- 适配器形态：Rust 独立进程，作 daemon 控制 socket 客户端，复用 core 协议定义。daemon 自身不起 HTTP。
- 适配器职责：一个进程全包——serve Vue 静态文件 + REST 接口（翻译 daemon 控制命令）+ SSE 推字幕 + 文件上传下载。无单独静态服务，无 Python。
- 接入路径：Jetson 开 WiFi 热点（默认）+ 局域网访问（保留）。适配器监听 `0.0.0.0:80`。热点为部署配置（SPEC-09），不改 web 架构。
- 字幕推流：适配器 inotify watch `subtitles.jsonl` → SSE 推浏览器（沿用旧方案）。桥不改，文件是唯一发布点。
- API 面：薄翻译层，HTTP 端点映射 daemon 控制命令（SPEC-04 命令面）。文件上传流式写盘经同一入队契约；下载不沿用旧固定名。`restart_required`/`affected` 透传展示。
- v1 无认证（本地设备、自有热点）。

## 待办

- [ ] 承接上游留前端的事项：glossary 编辑与 subtitle clear（SPEC-05）、监控经适配器推网页（SPEC-04）。
- [ ] `web/` 前端工程创建（TypeScript/Vue；v1 验收项：启动、切换、查看状态、停止，见 SPEC-08）。
- [ ] Rust 适配器 crate 创建（`web-adapter/` 或归入现有 crate，实现时定）。
- [ ] 热点部署配置入 SPEC-09 流水线（nmcli AP 模式 + 开机自启）。
