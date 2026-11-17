# 32 kHz 模型接入与双链验收（asuka-v2-32k）

日期：2026-11-17。
依据：`docs/SPEC/SPEC-03-rvc.md` 待办「32 kHz 模型接入与双链验收」。

## 结论

- 文件链与实时链的运行时代码对采样率通用，本轮未改运行时逻辑：文件链几何按 `model_sr` 推导、收尾多相重采样到 48 kHz；实时几何支持集含 32000，输出帧 640 有效样本，播放边界重采样覆盖 32k。
- 工作主体是资产生成与板端验收：asuka-v2-32k 文件链与实时链资产成套，登记 `[[models]]`。
- 文件链 11 模型回归通过；实时 48k↔32k 9 场景通过；本机 `tools/ci-local.sh all` 通过。
- 测试补强：文件分窗几何偶数 T 扫描纳入 32k 并加 T=398@32k 锚点；重采样单测加 32k→48k 用例。

## 资产生成

原料 `~/rvc-pth/asuka-v2-32k.pth`：v2、f0、sr 32000、emb 768、109 说话人、上采样积 320（`[10,8,2,2]`），带 index。导出在设备 `~/mozart-toolkit/rvc-env`（torch 2.15.0、transformers 4.46.3），RVC 源码树 `~/mozart-assets/RVC`。

导出对拍（PyTorch vs ONNX Runtime，CPU）：

| 资产 | 形状 | relative_rmse | correlation |
| --- | --- | --- | --- |
| 动态生成器 T=100 | audio 1×1×3200 | 0.001568 % | 1.000000 |
| 动态生成器 T=200 | audio 1×1×6400 | 0.004856 % | 1.000000 |
| 动态生成器 T=202 | audio 1×1×6464 | 0.000504 % | 1.000000 |
| 实时 front | z 1×192×30 | 0.000071 % | 1.000000000 |
| 实时 decoder | audio 1×1×9600 | 0.000194 % | 1.000000000 |

decoder 输出 9600 = 30 帧 × 320 样本，与 `derive_realtime_geometry(32000)` 一致；front 280 帧、latent 54 帧与采样率无关。

引擎（设备 trtexec，TensorRT v101602）：

| 引擎 | 档位/形状 | 大小 |
| --- | --- | --- |
| asuka-v2-32k.engine | large：feats min/opt/max = 100/597/2400，source_noise 每帧 320 | 118 578 948 B |
| asuka-v2-32k-front.engine | 定长 feats 1×280×768 | 54 568 828 B |
| asuka-v2-32k-decoder.engine | 定长 z 1×192×30 → audio 9600 | 61 520 812 B |

共享 HuBERT/RMVPE 引擎（动态与实时）复用 qiqi-zh 目录成品，未重建。asuka 目录以符号链接引用四个特征 onnx 与四个特征 engine。链接 engine 的原因：`make_engine` 按给定路径同目录找 `.engine`，不解析符号链接；多模型套件按字母序取首个特征齐全模型为 FEATURE_MODEL，asuka 排首，只链 onnx 会回退 ONNX 特征。

成品目录 `~/mozart-assets/models/rvc/asuka-v2-32k/`：config.json（sr 32000、emb 768、spk 0/109）、生成器 engine+onnx、front/decoder engine+onnx、index、特征链接、build-manifest.json（large、sr 来源 config、samples_per_frame 320）。`[[models]]` 已登记于 `~/mozart-runtime/mozart.toml`。

## 板端实测：文件链

输入 `~/mozart-samples/jieshe.m4a` 截取 10 s（16 kHz 单声道），`tools/jetson-file-multimodel.sh`，debug 构建，index_rate 0。判据：job completed、frames_in == frames_out、输出 48 kHz 单声道、采样数 = 输入向上取整到 960、RMS > 0.0001、无 TensorRT 回退 ONNX。

全量 11 模型通过（`MULTIMODEL PASSED (11 models)`）：

| 模型 | frames in/out | blocks | infer_ms | RMS |
| --- | --- | --- | --- | --- |
| asuka-v2-32k | 500/500 | 1 | 2604 | 0.0943 |
| de-narrator | 500/500 | 15 | 4365 | 0.1219 |
| frieren-v2-40k | 500/500 | 1 | 1593 | 0.1127 |
| liuying | 500/500 | 1 | 1707 | 0.1036 |
| mike-tyson | 500/500 | 1 | 1574 | 0.1123 |
| miku-v2-48k | 500/500 | 1 | 1704 | 0.1000 |
| qiqi-zh | 500/500 | 1 | 1692 | 0.0996 |
| special-week-v2-48k | 500/500 | 1 | 1736 | 0.0761 |
| tokai-teio-v2-48k | 500/500 | 1 | 1708 | 0.1148 |
| tomori-v2-40k | 500/500 | 1 | 1590 | 0.1894 |
| trump-v2 | 500/500 | 1 | 1599 | 0.1009 |

asuka 单模型首跑（冷加载）infer 8902 ms、wall 11 s；全量回归中暖加载 2604 ms。10 s 输入 + 2 s pad = 1200 帧 ≤ large max 2400，整段单窗。输出 480 000 样本 = 10 s × 48 kHz。证据：设备 `/tmp/mozart-multimodel/summary.tsv` 与 `/tmp/mozart-32k-onboard/file-models-full.log`。

## 板端实测：实时链

`tools/jetson-realtime-mm.sh`，MODEL_A=qiqi-zh（48k）、MODEL_B=asuka-v2-32k（32k），输入 3 s 锯齿波，`skip_silence = false`。9 场景全过（`MM PASSED (9 scenarios)`）：

| 场景 | 关键断言 |
| --- | --- |
| 48k 实时基线 | 帧守恒 201=201，blocks>0，last_error None |
| overlap 定值拒绝 | crossfade 40 被拒（overlap 须为 60 ms），不打断在跑流 |
| startup_delay 可调 | 200 ms 流重建，帧守恒 201=201 |
| model use 48k→32k | 加载 sr=32000，32k 帧守恒 199=199，blocks>0，预设不 dirty |
| model use 切回 48k | 载入 A 预设 pitch=0，帧守恒 200=200 |
| 未知模型失败恢复 | 旧模型旧参数恢复，帧守恒 200=200 |
| 文件占 GPU 回 Busy | model use 回 Busy，文件任务仍 completed |
| 32k 文件链 | completed，输出 48 kHz |
| 资产无 presets 残留 | A/B 资产目录前后一致 |

证据：设备 `/tmp/mozart-realtime-mm/summary.tsv` 与 `/tmp/mozart-32k-onboard/mm-test.log`。

## 本机检查

`tools/ci-local.sh all` 通过：native 10 个 C++ 用例全过，Rust 全 workspace 通过。新增覆盖：

- `tests/test_rvc_window_geometry.cpp`：T=398@32k 锚点（pad_tgt 32000、window_out 63360、crossfade_out 6400）；偶数 T 扫描 200..4200 纳入 model_sr 32000。
- `tests/test_resample.cpp`：32k→48k 长度 3/2、14 kHz 通带保留、18 kHz 镜像抑制 > 34 dB 且优于线性插值 10 倍、1 kHz 相关 > 0.99。

## 部署后复核

分支源码经 `tools/deploy-jetson.sh` 增量同步后，`tools/jetson-e2e.sh --unit --file --realtime-mm` 全过：设备 `cargo test --workspace` 各套件 0 失败、ctest 10/10；文件基础套件通过；默认实时多模型 48k↔40k 9 场景通过，无回归。运行时恢复后 `model list --available` 见 asuka-v2-32k exists=yes。证据：设备 `/tmp/mozart-32k-onboard/unit-test.log`。

## 生产运行时手动复核

运行时恢复后直接经 `ctl file submit --model <id> → file fetch` 转换，非测试套件临时配置：

| 输入 | 模型 | 输出 | RMS | 峰值 |
| --- | --- | --- | --- | --- |
| jieshe.m4a 30 s 起 15 s（16 kHz） | asuka-v2-32k | 48 kHz 单声道 720 000 样本 | 0.0933 | 0.6944 |
| 同上 | qiqi-zh | 48 kHz 单声道 720 000 样本 | 0.1109 | 0.9548 |
| gkw_male_30s.wav（44.1 kHz） | asuka-v2-32k | 48 kHz 单声道 1 440 000 样本 | 0.0875 | 0.7036 |
| 同上 | qiqi-zh | 48 kHz 单声道 1 440 000 样本 | 0.0845 | 0.9417 |

gkw 直接提交 44.1 kHz 原文件，daemon 解码重采样到 16 kHz 得 480 000 样本，输出 1 440 000 = 1500 帧 × 960；与旧报告预转 16 kHz（479 999 样本）路径的舍入差异属输入路径不同。听感样本存本机 `assets/out/20261117-1932-asuka32k-file-try/`（input/gkw-input 与两模型输出）。

## 音色诊断（asuka 听感偏闷）

结论：实现无偏差，闷感来自模型带宽与参数。引擎输出与上游 PyTorch 参考（`run_reference.py`，同输入 gkw 30 s，pitch 0、无 index，整段单窗）频谱平衡与电平一致：centroid 2425 vs 2437 Hz，RMS 0.0875 vs 0.0865，2-8k/0-2k 能量比 0.066 vs 0.064；参考听感同样偏闷（`gkw-asuka-pytorch-ref.wav`，32 kHz）。

闷感的客观来源：

- 32k 模型带宽 ≤ 16 kHz：输出 16-24k 能量 ≈ 0.0006（噪底），qiqi-zh 48k 为 0.2007；centroid 2425 vs 3529 Hz。相对 48k 模型天然闷，属采样率固有。
- asuka 频谱平衡偏中低频：0-2k 能量高于输入与 qiqi-zh，2-8k 低于两者。
- 男声 gkw 以 pitch 0 驱动女声模型，F0 保持 167 Hz，跨性别映射不自然；index_rate 0 无音色检索。

参数实验（gkw 30 s，生产运行时，自相关 F0 估计）：pitch +6 → 232 Hz，+12 → 278 Hz（估计有倍频程歧义，递增关系正确），index 0.75 → F0 不变、检索生效。试听文件同目录 `gkw-asuka-p6/p12/idx075.wav`。

实验方法注意：文件任务参数取模型预设/live（SPEC-11）；跨模型提交触发预设载入，会覆盖刚 `config set` 的基线，同模型提交保留 live。首次 +12 因 qiqi→asuka 切换被预设覆盖失效，重跑有效。

环境备注：`run_reference` 依赖 praat-parselmouth，设备 venv 缺装已补（skill environment.md 清单同步修正）；资产树 `RVC/assets/hubert_base` 与 `rmvpe/rmvpe.pt` 符号链接指向已删除的 `~/Mozart/rvc-golden`，参考推理改用归档 `~/mozart-archive/legacy/Mozart-Old/rvc-golden/assets/` 路径。

## 适用范围

- 配置：Jetson Orin Nano Super 8 GB，JetPack 6，TensorRT 10.16.2，debug 构建；文件链输入 10 s × 11 模型，实时链 3 s 锯齿波 × 9 场景，index_rate 0。部署后复核：文件基础套件 1 轮、默认实时多模型 48k↔40k 9 场景、设备单测 1 轮。手动复核：jieshe 15 s 与 gkw 30 s 各 × 2 模型，共 4 次转换。音色诊断：PyTorch 参考 1 次、pitch/index 实验 4 次（含 +12 重跑）。
- 计时：infer_ms 为 rvc FileWorker 整段推理耗时（含特征与生成器），wall_s 含提交到 fetch 全链路；实时帧守恒为 3 s 采集窗内计数。
- 未验证项：32k 实时延迟 P95、长跑稳定性、切换接缝听感与音质主观评价，阻塞原因与解除条件见 [SPEC-03 板端条目](../../docs/SPEC/SPEC-03-rvc.md)，本文不重复。