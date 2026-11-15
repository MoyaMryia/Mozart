# web：控制台

状态：后续。2026-11-15 起为待办清单，记法见 [SPEC.md](SPEC.md)。

保持 TypeScript/Vue，不改写。旧实现在仓外 Mozart-Old/frontend，仅归档参考；本仓 `web/` 未创建。

## 待办

- [ ] 字幕推流方式决定：SPEC-05 明确"推流方读 subtitles.jsonl 文件，留到前端模块定"，本文件未承接。
- [ ] HTTP/SSE 适配器范围决定：SPEC-04 标待讨论并指向本模块；与本文件互相等待，见 SPEC.md 跨模块冲突。
- [ ] 承接 daemon 已定稿的接入决定并记录：HTTP 适配器作 daemon 控制 socket 客户端、文件上传（流式写盘经同一入队契约）与下载（不沿用旧固定名）、`restart_required`/`affected` 展示、监控经适配器推网页、glossary 编辑与 subtitle clear。（出处 SPEC-04、SPEC-05）
- [ ] `web/` 前端工程创建（TypeScript/Vue；v1 验收项：启动、切换、查看状态、停止，见 SPEC-08）。
