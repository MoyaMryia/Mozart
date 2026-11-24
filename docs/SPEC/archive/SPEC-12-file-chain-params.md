# SPEC-12 归档（file-chain-params）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-12-file-chain-params.md](../SPEC-12-file-chain-params.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 全量归档

范围：「实施状态」3 条 `[x]`。契约各节与全部 `[ ]` 待办留在源文件。归档条目内无相对链接，无需调整深度。

### 实施状态

- [x] 档位数值单一来源：`tools/model/file_chain_tiers.json`；脚本读取，`--list-tiers` 打印；SETUP 只链接。
- [x] 共享特征引擎：HuBERT 512000、RMVPE 3232 已构建并部署；30 s miku/gkw 实测 2 窗、峰值 4.47 GiB、8.27/6.74 s，无 ONNX 特征兜底。
- [x] 窗几何：`offline_window_geometry` + `tests/test_rvc_window_geometry.cpp`，native 9/9 通过。