# SPEC-05 归档（language）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-05-language.md](../SPEC-05-language.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 全量归档

范围：「已确认 › 翻译」1 项对拍记录、「回归检查」1 项测试修正记录。其余 `[x]` 均为契约类，去勾选后保留在源文件，不归档。链接已按归档目录调整深度。

### 已确认

#### 翻译

- [x] 同音字纠错与旧实现逐条对拍：对拍夹具入本仓 `tests/fixtures/preprocess/homophones.json`，由旧 `tools/tests/test_homophones.py` 用例经旧 `homophones.correct` 实际运行提取，bridge 逐条断言文本与替换记录一致。（质检对拍旧 `test_translation_checks.py` 另由 `bridge/src/checks.rs` 内联测试覆盖。）

### 回归检查

- [x] 重试翻译的进程测试在原 5 s 预算内同时等待翻译和异步播报提交，保留全部状态断言，消除提前观察计数的竞态。