# O2 逐引擎精度盘点：fp32 回退路线证伪（O1 门内差分实测）

日期：2026-11-28。承接 [O1 基线](../sculptor-golden-parity-20261128/RESULTS.md)。
golden：`eval_l2_en_frieren.wav`（本机离线 L2 链）。被测：板端隔离栈实时链（frieren + sculptor），
snd-aloop 同源回灌 `eval_input_20s.wav`（`preprocess_16k` 16 kHz PCM16）。

## 对拍工具

新增 `tools/model/gate_compare.py`：RMS 包络粗对齐（±6 s，覆盖回录预卷与启动缓冲）→
mel 相关细对齐（±40 帧，宽于 `compare_streaming_quality` 的 ±20 帧），输出整体指标与
逐秒 cents/mel 分布。对拍指标地板（golden 自对齐 + 人为 10 ms 错位）为 **10 cents**。

## 差分矩阵（仅改一个引擎，其余 fp16）

| 配置 | logmel_mae | rms_corr | cents \|abs\|中位 | signed 中位 | p90 \|abs\| |
| --- | --- | --- | --- | --- | --- |
| A0 全 fp16（TRT） | 4.75 dB | 0.946 | 90 | +30 | 190 |
| A1 sculptor ORT fp32 | 4.87 dB | 0.946 | 90 | +30 | ~190 |
| A2 rmvpe fp32 引擎 | 4.81 dB | 0.948 | 100 | +30 | 199 |
| A3 hubert fp32 引擎 | 4.81 dB | 0.948 | 90 | +30 | 190 |

**结论：fp32 回退无可测收益，精度瓶颈不在引擎 fp16。保留全部 fp16 引擎（fp32 编译产物留在设备 `/tmp/*fp32.engine` 备查，未入资产）。**

## 偏差结构（A0 对齐后逐秒）

- 多数秒 30–100 cents；无八度跳变。
- 启动段 t=0–2 s：-100~200 cents（意图 EMA 冷启动 + RMVPE 窗缘）。
- 少数重读段（t=9–10 s、t=17–18 s）：+145–160 cents。
- 初判与引擎精度无关（A1–A3 三方向均无变化），指向结构性口径差：
  a) 实时 RMVPE 逐块 mel 补零 vs golden 整段 RMVPE 的帧级差；
  b) 启动态历史窗零填充差异；
  c) ContentVec 实时导出与 Applio 整段前端的帧对齐差异。
  归因列为后续观察，不阻塞 O3 提速项（O3 各项的等价性验证以「改动前后同口径对拍」为门，不要求逼近 golden 的绝对值）。

## 运行状态

三轮采集均 `frames_dropped=0`、`last_error=None`；fp32 引擎编译于停机窗口（canonical 暂停/恢复正常）。

## 适用范围

- 配置：JetPack 6，隔离栈 `MozartMryia` `ce7e53d`，frieren-v2-40k 40k，EN 20 s 单样本单次采集。
- 未验证项：ZH 侧逐秒分布（O1 已有整体指标）；实时 RMVPE 与整段 RMVPE 的帧级差量化。
