# O3-a0 全引擎 FP16 补齐：单块 70 ms → 26.7 ms

日期：2026-11-28。承接 O1/O2（同目录 [RESULTS.md](RESULTS.md)）。

## 发现

逐引擎 trtexec 盘点（定频、canonical 停机）发现 FP32 欠账：降延迟步骤 B（11-25）只重编了
bocchi 的四引擎，共享前端（qiqi 目录 hubert-realtime / rmvpe-realtime）与 frieren 的
front/decoder 仍是 11-15 的 FP32 资产：

| 引擎 | FP32 | FP16 | 加速 |
| --- | --- | --- | --- |
| hubert-realtime（共享） | 21.13 ms | 9.73 ms | 2.2× |
| rmvpe-realtime（共享） | 11.87 ms | 4.92 ms | 2.4× |
| frieren front | 9.22 ms | 4.72 ms | 2.0× |
| frieren decoder | 28.48 ms | 5.18 ms | 5.5× |

（sculptor 引擎本就是 FP16，1.31 ms；f0 墙钟 28.7 ms 中 RMVPE GPU 只占 11.9 ms，
其余为 HuBERT 并行支路在同一 GPU 上的争抢与 C++ mel 预处理——FP32 时代的错误归因已修正。）

## 换装与实测

- `build_realtime_engines.py --fp16` 正式管线重编四引擎，换装 qiqi（共享前端）与 frieren
  （front/decoder + build-manifest.json）；旧 FP32 备份 `/tmp/engine-backup-fp32/`。
- 板端单块（frieren + sculptor，240 ms 块，定频，canonical 停机，`MOZART_RVC_PROFILE=1`）：

| 阶段 | FP32 时代 | FP16 换装后 |
| --- | --- | --- |
| f0 + HuBERT 并行窗 | 28.7 ms | 13.5 ms |
| sculptor + 装配 | 2.5 ms | 2.5 ms |
| front + decoder（chained） | 38.7 ms | 10.6 ms |
| **单块总耗时** | **70 ms** | **26.7 ms**（p95 26.8，170 块） |

GPU 占空比 70/240 = 29% → **11%**。EN/ZH 双语采集 `frames_dropped=0`、`last_error=None`。

## golden 门（换装后）

| 指标 | EN | ZH | FP32 引擎基线 |
| --- | --- | --- | --- |
| logmel_mae_db | 4.98 | 5.09 | 4.75 / 5.31 |
| rms_envelope_corr | 0.944 | 0.928 | 0.946 / 0.925 |
| f0_median_cents | 90 | 90 | 90 / 80 |
| voiced_iou | 0.924 | 0.944 | 0.924 / 0.948 |

EN 退化 +0.2 dB（在 bocchi FP16 历史 +0.12 dB 同档），ZH 反而改善。**门通过。**

## 顺手收益与风险

- canonical（bocchi，共享前端指向 qiqi）随本次换装前端也变 FP16，预计单块再降约 10 ms；
  重启后 `daemon/pre/rvc` 探活正常（rvc idle、无错误）。canonical 上的块耗时与听感未复测。
- 未验证项：net 延迟（`startup_delay_ms` 可再下调的余量约 10–15 ms）物理回录；60 s 长跑。
- 遗留：micro 基准 `tests/bench_f0_stage.cpp`（本地工作副本，未入库——如保留需补 CTest 注册与说明）。

## 适用范围

- 配置：JetPack 6 / TensorRT 10.16.2，jetson-clocks 定频，canonical 停机窗口内采集与换装，测后已恢复。
- 引擎耗时为 trtexec `--loadEngine` 单引擎隔离口径；管道墙钟为 `[profile][realtime]` 逐块锚点（170 块）。
- golden 门样本：EN 20 s / ZH 25 s 各单次采集；未做统计重复。
- 未验证项：canonical（bocchi）换装前端后的块耗时与听感复测；net 延迟下调余量的物理回录；60 s 长跑零缺帧复验。
