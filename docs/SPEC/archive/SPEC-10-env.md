# SPEC-10 归档（env）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-10-env.md](../SPEC-10-env.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 全量归档

范围：「配置」1 条 `[x]`（daemon 去掉模型存在预检的审查修正记录与回归证据）。其余 `[x]` 属资产分层、工具链脚本与加载契约，已在源文件去勾选保留；全部 `[ ]` 待办与「待决策」留在源文件。归档条目内无相对链接，无需调整深度。

### 配置

- [x] daemon 去掉模型存在预检，报错统一归推理层：入队原样收模型 id，执行时 LoadModel 失败标 failed 带原因、队列继续（`daemon/src/file/mod.rs:369-387`，回归 `daemon/tests/file_worker.rs` unknown_model_fails_at_load_and_queue_continues）。