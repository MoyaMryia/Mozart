# 组装、登记、测试、清理

成品目录契约、共享特征引擎、`[[models]]` 登记：见 [SETUP.md](../../../../docs/SETUP.md) §资产布局、§运行配置。测试命令：见 [GUIDE.md](../../../../docs/GUIDE.md) §操作命令。本文只补 SETUP/GUIDE 没有的配置字段与清理。

## config.json 字段

```json
{
  "data": {"sampling_rate": 48000},
  "model": {"emb_channels": 768},
  "spk": {"id": 0, "count": 202},
  "f0": true,
  "version": "v2"
}
```

- `sampling_rate`：模型原生采样率，决定重采样与每帧输出点数。
- `emb_channels`：内容特征维度（v2 为 768）。
- `spk.id` / `spk.count`：默认说话人编号与总数。多说话人模型目前只用 `spk.id`（per-job 选人未实现）。
- `f0`：必须 true。
- `version`：v2。

## 清理（接入后必做）

清理前先 `ls` 确认范围，只删下列类别，不碰成品与原始权重：

- 导出临时：`--test-data-dir` 输出、`tensors/`、`reference.json` 等 golden 中间件。
- Python 缓存：`tools/model/` 与 RVC 源码下 `__pycache__`。
- 下载遗留：解压后不用的 `*.zip`、`*.7z`、`*.tar.gz`、`*.part`、`*.tmp`。
- 冗余导出变体：本次没用的 `<id>-t398.*`、`<id>-dynamic.*` 等，**确认后再删**。
- 编译日志：`*-build.log`、trtexec 日志，保留需说明用途。

**不要删**：最终 engine、与 engine 同名的 onnx（回退用）、`config.json`、index、`~/rvc-pth/<id>.pth`。