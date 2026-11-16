# 采样率生态与实时原生多采样率方案调研

日期：2026-11-16
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

1. **推理层（`rvc`）**：`StreamingRvc` 内部按模型原生 `tgt_sr`（支持 32000 / 40000 / 48000 Hz）计算 `model_frame_out = tgt_sr / 100`、`output_frame_samples = tgt_sr / 50`（20 ms 帧）、`hop`、交叉淡化、SOLA 搜索窗与启动缓冲；不再在 `pipeline.cpp` 与 `inferencer.cpp` 写死 48000 Hz。
2. **帧契约（`core`）**：`mozart_output_frame_t` 保留 48 kHz 20 ms 缓冲上限 `MOZART_OUTPUT_MAX_SAMPLES = 960`，新增 `sample_rate`（u32）与 `valid_samples`（u32）字段，同时支持 32k（640 样本）、40k（800 样本）与 48k（960 样本）原生输出帧。
3. **音频前端（`pre`）**：播放线程维护轻量无分配状态化重采样器；仅当收到帧的 `sample_rate` 与实际打开的 ALSA 播放采样率不一致时在边界重采样一次，采样率一致时零重采样直出。
4. **模型切换（`daemon` + `ctl`）**：新增 `model use <id>` 控制命令；考虑到 8 GB Jetson 显存紧张（同时驻留旧/新 TensorRT 引擎易触发 OOM 回退 ONNX）及跨采样率模型切换时 `StreamingRvc` 几何需重建，v1 采用**断流切换（`StopStream -> LoadModel -> StartStream`）**，失败时尝试恢复原模型与流状态。

## 适用范围

- 覆盖公开资料调研与 Mozart `core` / `pre` / `rvc` / `daemon` 架构范围界定。
- 具体 40 kHz 模型在 Jetson 板端 USB/板载声卡上的实测延迟、显存与音质数据待板端 E2E 验证后补充。
