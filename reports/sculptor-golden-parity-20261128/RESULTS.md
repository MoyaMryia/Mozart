# L2 实时链路板端 golden 对拍基线（O1）

日期：2026-11-28。承接 [sculptor-jetson-deploy-20261128](../sculptor-jetson-deploy-20261128/RESULTS.md)。
golden：本机离线 L2 链输出 `prosody_sculptor_l2/local_eval_20261128/eval_l2_{en,zh}_frieren.wav`
（L2 塑形 → frieren-v2-40k RVC，`infer_stream_eval.py --rvc-eval`）。
被测：板端隔离栈实时链（frieren + sculptor TRT 引擎，`~/MozartMryia`，`ce7e53d`）。

## 方法

1. 输入同源：`eval_input_*.wav` 经 `features.read_audio`（`preprocess_16k`）得 16 kHz PCM16，
   与 golden 链消费的信号完全一致。
2. 板端 snd-aloop：`aplay → Loopback DEV0,SUB2 → pre 采集 DEV1,SUB2`；
   `pre 播放 DEV0,SUB3 → arecord DEV1,SUB3` 48 kHz 立体声回录，arecord 提前 0.5 s。
3. 路径对齐 golden：`pre.dsp.denoise=false`，rvc 参数 `pitch_shift=0`、`index_rate=0`、
   `rms_mix_rate=1.0`、`protect=0.33`（sculptor 激活时该两项由 L2 替代，与 golden 一致）。
4. 对齐：16 kHz 域 RMS 包络有界互相关粗对齐（EN/ZH 均 `1.00 s`，即回录预卷 + 启动缓冲），
   裁剪后 `tools/model/compare_streaming_quality.py`（`stream_hop=3840`，块周期 240 ms）细对齐。

## 指标（O1 基线，回归门）

| 指标 | EN | ZH |
| --- | --- | --- |
| logmel_corr | 0.915 | 0.913 |
| logmel_mae_db | 4.92 | 5.31 |
| rms_envelope_corr | 0.946 | 0.925 |
| rms_ratio_db | +0.49 | +1.26 |
| centroid_median_delta_hz | +40 | +51 |
| f0_median_cents | +90 | +80 |
| voiced_iou | 0.924 | 0.948 |
| 细对齐残差 | 0 ms | 10 ms |

运行状态：`blocks=242`、`frames_dropped=0`、`last_error=None`，frieren 40k 实时 `inference_ms_p95 = 70.0 ms`
（不定频窗口，canonical 同机在跑；定频口径见降延迟步骤 D 的 `29.89 ms` 基线为无 L2 路径）。

## 初判

- 包络/音色结构高度一致（corr 0.91–0.95），量级与文件链 FP16 对 FP32 的历史容差（Log-Mel L1
  `4.86 vs 4.74 dB`）同档，作为 O2/O3 回归门可用。
- `f0_median_cents` +80~90（≈0.7–0.9 半音）超出 fp16 噪声预期，O2 优先定位：候选因素为
  RMVPE fp16 引擎对 intent 轮廓的扰动、sculptr fp16 引擎、以及 aloop 回录增益链。
- 回录链路含 ALSA plughw 转换与 16-bit 量化，为固定系统偏置，改动前后同口径可比。

## 适用范围

- 配置：JetPack 6，隔离栈 `MozartMryia` `ce7e53d`，frieren-v2-40k 40k；canonical 停机窗口内采集，测后已恢复。
- 样本数：EN 20 s / ZH 25 s 各一条，单次采集，未做统计重复。
- 未验证项：sculptor 在真实语音（非静音流）上的板端单块耗时增量；f0 偏移的根因归因（O2 执行）。
