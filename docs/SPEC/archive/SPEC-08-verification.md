# SPEC-08 归档（verification）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-08-verification.md](../SPEC-08-verification.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 全量归档

范围：「功能验收」10 条验收通过记录、「本次接入回归」1 条、「已确认」2 条。「验收依据」2 条引用条款去勾选后留在源文件，全部 16 条活跃 `[ ]` 逐字保留。链接已按归档目录调整深度。

### 功能验收

#### 实时变声

- [x] 对着麦克风说话，扬声器持续输出目标音色；不中断、不爆音、无明显机械感。（真语音回录见 `reports/realtime-pipeline-20261112`；切换单样本阶跃复测见系统级条目）
- [x] 稳态推理块时延不劣于旧基线（固定模型与配置；中位 88 ms、P95 93 ms）。

#### 翻译字幕

- [x] 每个输入都有结果或失败记录；错误和未翻译项逐条列出。

#### 翻译播报

- [x] 译文用参考音色播出（v1 继承旧播报；经变声同音色见 FUTURE）；按句序、不丢句。（`reports/language-chain-20261113`）
- [x] 播报队列有上限，积压时不无限等待。（`tts/tests/process.rs:228`）
- [x] 播放关系（分时或混音）见 [SPEC-05-language.md](../SPEC-05-language.md)。

#### 文件转换

- [x] 用 CLI 提交音频文件（`file submit`）能得到变声结果并取回（`file fetch`）；v1 用 CLI 验收，网页上传下载归 web 适配器。（`reports/file-chain-20261113/RESULTS.md:60-63`，`tools/jetson-file-test.sh`）
- [x] 任务可取消；每个输入都有结果或明确失败原因。（取消清理见同报告）
- [x] 多模型文件转换：40k/48k、静态/动态生成器共 10 个模型完成且帧守恒；`frieren-v2-40k` 与 PyTorch 参考对拍 log-mel 相关 0.9930、F0 中位偏差 10 音分、对齐 0 ms。（`reports/multi-model-file-20261115/RESULTS.md`）

#### HTTP API 与网页

- [x] daemon 停止后页面与适配器仍可访问，并能从网页再次启动 daemon。（[板端浏览器联调](../../../reports/web-adapter-20261117/RESULTS.md)）

### 本次接入回归

- [x] 修正预设损坏测试中文件完成与状态轮询之间的竞态；仍在既有 `POLL_TIMEOUT` 内要求 `model-b` 已加载，不放宽目标或超时。

### 已确认

- [x] golden 对比沿用旧方法。（`reports/rvc-fulllength-golden-20261113/RESULTS.md`）
- [x] 实测记录按 `reports/` 约定。