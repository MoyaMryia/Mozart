# 采样率生态与实时原生多采样率方案调研

日期：2026-11-16
修订：2026-11-17，按已落地代码校正第 4 节实现边界与常量名。
修订：2026-11-17，按用户确认的边界变更把淡化与 SOLA 锁为定值 60 ms，校正第 4 节与适用范围。
依据：`docs/SPEC/SPEC-03-rvc.md`「采样率生态与原生多采样率方案调研」

## 1. RVC 模型采样率分布

- 主流 RVC 模型（含 RVC v1/v2 与社区主流微调底模）的 `config.json` 中 `data.sampling_rate` 集中为 **32000 Hz（32k）**、**40000 Hz（40k）** 与 **48000 Hz（48k）** 三档（[Retrieval-based-Voice-Conversion-WebUI CLI 文档](https://github.com/RVC-Project/Retrieval-based-Voice-Conversion-WebUI/blob/main/docs/en/cli.md)、[Issue #1565](https://github.com/RVC-Project/Retrieval-based-Voice-Conversion-WebUI/issues/1565)）。
- 特征提取侧（HuBERT 与 RMVPE）在所有模型上固定以 **16000 Hz** 输入、**100 Hz** 帧率（`hop = 160` 样本；HuBERT 50 Hz 经 2× 上采样到 100 Hz）工作，与目标音色采样率无关。
- 声码器（NSF-HiFiGAN Generator）决定输出采样率，每帧（10 ms）输出样本数为 `tgt_sr / 100`：32k 为 320、40k 为 400、48k 为 480。

## 2. 上游实时推理与导出/编译约束

- 上游实时推理（[Retrieval-based-Voice-Conversion-WebUI/realtime_gui.py](https://github.com/RVC-Project/Retrieval-based-Voice-Conversion-WebUI/blob/main/realtime_gui.py)）按播放设备采样率计算每 10 ms 帧长 `zc = samplerate // 100`，并仅在 `rvc.tgt_sr != gui_config.samplerate` 时在输出边界实例化一次重采样器（`resampler2`）。
- Mozart 拆分导出的实时引擎（`front` + `decoder`）：
  - `front` 输入为 100 Hz 特征序列（`feats`、`p_len`、`pitch`、`sid`、`latent_noise`），其形状只依赖时间帧数与 `emb_channels`（v1 为 256，v2 为 768）及 `inter_channels`，**与目标采样率无关**。
  - `decoder` 输入 `source_noise` 长度为 `return_frames × (tgt_sr / 100)`，输出 `audio` 长度为 `return_frames × (tgt_sr / 100)`。因此同一套 `front` / `decoder` 导出公式对 32k / 40k / 48k 通用，仅 `decoder` 的 `upp`（上采样总倍率）随模型不同。

## 3. ALSA / USB / 蓝牙播放设备采样率约束

- Linux 下许多板载声卡与 USB 音频接口硬件仅原生支持 **44100 / 48000 / 96000 Hz**，不支持 40000 Hz（[Linux Audio 讨论](https://www.reddit.com/r/linuxaudio/comments/1bs44ik/unable_to_set_sample_rate_for_my_sound_card_alsa/)、[ALSA 采样率说明](https://unix.stackexchange.com/questions/74558/change-sampling-rate-in-alsa)）。
- 对 `hw:` / `plughw:` 直接调用 `snd_pcm_hw_params_set_rate_near` 请求 40000 Hz 时，若硬件不支持，ALSA `hw:` 会回退到最接近的支持采样率（如 44100 Hz 或 48000 Hz），不能假定所有播放设备都能直接以 40000 Hz 打开。

## 4. 方案结论与实现边界

结合上述生态事实与 `SPEC-03-rvc.md` 目标，采用**原生多采样率全链路 + 播放边界兜底重采样**：

1. **推理层（`rvc`）**：`StreamingRvc` 内部按模型原生 `tgt_sr`（支持 32000 / 40000 / 48000 Hz）计算 `model_frame_out = tgt_sr / 100`、`output_frame_samples = tgt_sr / 50`（20 ms 帧）、`hop`、交叉淡化、SOLA 搜索窗与启动缓冲；实时链路不再写死 48000 Hz。文件链路不变，离线收尾仍重采样到 48 kHz。交叉淡化与 SOLA 搜索窗锁为定值 50 ms 与 10 ms（合计 60 ms，等于最大 overlap），入口拒绝偏离；启动缓冲与静音跳过仍可调。锁定原因与解耦待办见 [SPEC-03 实时几何与资产参数](../../docs/SPEC/SPEC-03-rvc.md#实时几何与资产参数)。
2. **帧契约（`core`）**：`mozart_output_frame_t` 保留 960 样本缓冲上限（常量 `MOZART_OUTPUT_SAMPLES`，Rust 镜像 `OUTPUT_SAMPLES`），新增 `sample_rate`（u32）与 `valid_samples`（u32）字段，支持 32k（640 样本）、40k（800 样本）与 48k（960 样本）原生输出帧；输出线包由 3860 B 扩为 3868 B。
3. **音频前端（`pre`）**：ALSA 播放采样率固定 48 kHz，`set_rate_near` 拿不到 48000 Hz 即报错退出，不按流协商。播放线程的 `BoundaryResampler` 无分配、无锁，只取 `valid_samples` 有效段做线性插值重采样到 48 kHz；`sample_rate` 为 48000 且 `valid_samples` 为 960 时直通。切率与缺帧时重置插值历史。
4. **模型切换（`daemon` + `ctl`）**：新增 `model use <id>` 控制命令（`DAEMON_MODEL_USE` 0x0120）。考虑到 8 GB Jetson 显存紧张（同时驻留旧/新 TensorRT 引擎易触发 OOM 回退 ONNX）及跨采样率模型切换时 `StreamingRvc` 几何需重建，v1 采用**断流切换（`StopStream -> LoadModel -> 预设提交 -> StartStream`）**。失败一律用旧模型与旧参数恢复流；起流失败先整体回退 TOML 基线、生效配置与预设来源记录，再恢复旧流。

## 适用范围

- 配置：本文是公开资料调研与实现边界界定，不含本机或板端测量，无音频样本、无计时起止点。第 4 节按已落地代码核对，覆盖 Mozart `core` / `pre` / `rvc` / `daemon`。
- 样本数：引用 5 处公开来源（RVC CLI 文档、Issue #1565、`realtime_gui.py`、Linux Audio 讨论、ALSA 采样率说明）；无统计样本，完成率不适用。
- 已验证（板端实测，2026-11-17）：`tools/jetson-e2e.sh --realtime-mm --file --preset` 全过（实时多模型 9 场景、文件链 9、预设 8），含 48 kHz↔40 kHz 跨采样率 `model use`、overlap 定值拒绝、40 kHz 文件链输出 48 kHz 单声道。
- 已验证（本机实测，2026-11-17 overlap 锁定与验收修复后）：`tools/ci-local.sh all` 通过，native 10 个 C++ 用例全过（含 32k/40k/48k 几何对拍与流式用例），Rust 647 项、Python 工具链 7 项全过。板端未跑。
- 未验证项：32 kHz 模型板端实时（本轮设备只有 40 kHz 与 48 kHz 模型）、实时延迟 P95/显存/连续性/长跑稳定性、`model use` 切换接缝听感与音质主观评价。已验证的板端冒烟范围与解除条件见 [SPEC-03 板端条目](../../docs/SPEC/SPEC-03-rvc.md)。
