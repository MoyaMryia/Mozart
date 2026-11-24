# SPEC-12 文件链参数

状态：讨论中。最后更新：2026-11-16。
`[x]` 条目详情已于 2026-11-24 移入 [archive/SPEC-12-file-chain-params.md](archive/SPEC-12-file-chain-params.md)，本文件保留契约、关键决策摘要与活跃待办。

## 目的

文件链资产档位、运行时几何和构建规则的单一来源。数值只存
`tools/model/file_chain_tiers.json`；本文定义字段、公式和边界，不复制数值。

## 边界

- 本文 owns：文件链资产档位、生成器/HuBERT/RMVPE profile 的语义、文件链运行时几何公式、构建与部署规则。
- 运行时会话参数（`pitch_shift`、`index_rate`、`rms_mix_rate`、`protect`、`f0_method`）归 [SPEC-11](SPEC-11-preset.md) 与 [config.md](../config.md)，本文不重复。
- 实时链路的几何与固定形状（front 280 / decoder 30 / HuBERT 44800 / RMVPE 32）归 [SPEC-03](SPEC-03-rvc.md) 实时几何与资产参数节，本文不覆盖。
- 文件时长和上传上限归 [config.md](../config.md) 的 `[file]`，本文不重复。

## 已确认决策（摘要）

> 全文条目与证据见 [archive/SPEC-12-file-chain-params.md](archive/SPEC-12-file-chain-params.md)。与归档冲突时以归档原文为准并回来修正本节。

- 档位数值单一来源已落地：脚本读取 `tools/model/file_chain_tiers.json`，`--list-tiers` 打印，SETUP 只链接。
- 共享特征引擎已构建部署（HuBERT 512000、RMVPE 3232）；30 s 文件实测 2 窗、峰值 4.47 GiB，无 ONNX 特征兜底。
- 窗几何落地为 `offline_window_geometry` 并带几何对拍测试，native 9/9 通过。

## 数值来源

`tools/model/file_chain_tiers.json` 是档位数值的唯一来源。

- `build_dynamic_engines.py` 启动时读取该文件；`--list-tiers` 打印当前值。
- `docs/SETUP.md` 只链接本文和命令，不复制档位表。
- 改档位只改 JSON；脚本和文档引用同步生效。

## 字段与公式

生成器帧率 100 Hz（10 ms/帧）。对档位生成器上限 `T`：

- 档位 T 均大于 398，pad 封顶 1 s；主窗 = `T × 160 − 2 × pad` 样本（生成器输出覆盖），档位主窗 = `T / 100 - 2` 秒，2 秒为前后各 1 秒上下文。
- HuBERT 窗输入 = `align_up(160T + 80, 320)` 样本：卷积栈 hop 320、感受野 400，产 `floor((L−400)/320)+1` 帧，生成器 T = 2×帧数（`offline_window_geometry`）。HuBERT 的 `max` 必须覆盖窗输入，否则逐窗回退 ONNX。
- RMVPE 整段 mel：整段补 1 秒上下文后按 100 Hz 取帧；STFT 边缘多 1 帧，向上补到 32 的倍数。RMVPE 的 `max` 应覆盖目标文件长度。
- `min` 是最小输入；`opt` 是忙时形状；`max` 是能力上限。`opt` 不决定上限，只影响 TensorRT 调优和构建耗时。
- 生成器 profile 的 `min/opt/max` 同时约束 `pitch`、`pitchf`、`latent_noise`、`source_noise`；`source_noise` 长度 = `T × 采样率 / 100`。

## 构建与部署

- 共享 HuBERT/RMVPE 只建一次：`--feature-only --tier <全局档位>`。
- 每个模型只建生成器：`--generator-only --tier <档位>`；生成器 `max` 不超过共享 HuBERT 的 `max`。
- 构建前停服务；构建后写 `build-manifest.json`，记录档位、profile、采样率来源、命令和哈希。
- 默认档位由 JSON 的 `default` 字段给出。
- `max` 档未在板端验证；阻塞原因和解除条件见 [SETUP.md](../SETUP.md) 的文件链档位一节。

## 运行时

- 运行时读引擎 profile 的 `max`，不读档位 JSON；档位只影响资产构建。
- 文件链几何常量（参考 T=398、1 秒 pad、100 Hz、HuBERT 160 样本/帧、hop 320/感受野 400 样本）是实现参考；改动必须先更新本文和 `inferencer.cpp` 的常量与注释。
- 单文件链路峰值目标 5.8 GB（8 GB 设备）；实测记录见 `reports/`。

## 实施状态

- [ ] `max` 档板端验证：40 s 窗的构建耗时和峰值显存；阻塞与解除条件见 SETUP。
- [ ] 当前模型集按 `large` 重建；qiqi-zh large 生成器重建中。miku 生成器仍为旧 opt=398。

## 适用范围

- 本文覆盖文件链动态/静态生成器、共享 HuBERT/RMVPE 和档位构建；不覆盖实时链路的固定形状。
- `max` 档的构建耗时和峰值显存未验证；解除条件见 SETUP。
