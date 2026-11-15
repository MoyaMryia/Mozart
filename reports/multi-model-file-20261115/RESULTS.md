# 多模型文件转换与 golden 对拍

日期：2026-11-15。设备：Jetson Orin Nano Super 8 GB。分支：`multi-model-adaptation`。

## 结论

文件链路在 10 个 RVC 模型上完成转换，覆盖 40 kHz 与 48 kHz、静态定长、静态整段与动态生成器。
每个模型 500 帧入、500 帧出、480000 输出样本（10 s），帧守恒（实测）。

修复前 7 个动态生成器模型停在 `499/500` 帧；静态整段模型 `qiqi-en` 多出 5 帧。
根因是推理输出长度随生成器路径变化，未按输入时长收口。修复后所有分支统一输出长度（实测）。

模型切换时旧模型显存未释放，8 GB 设备上加载新模型会触发 TensorRT OOM 并回退 ONNX CPU。
修复后同一轮 10 模型切换无 ONNX 回退，动态模型推理稳定在约 7.5 s（实测）。

索引检索（`index_rate = 0.5`）、120 s 长文件与双模型并发提交也完成，均帧守恒（实测）。

`frieren-v2-40k` 与 PyTorch 参考对拍通过质量门槛：log-mel 相关 0.9930，F0 中位偏差 10 音分，
时间对齐 0 ms，时长差 20 ms（实测）。

## 修改

- `rvc/src/rvc/inferencer.cpp:243-442`：所有推理分支用 `finalize_output` 按输入时长统一收口输出；
  动态分支在重采样前先按模型采样率补齐，避免 RMS 混合长度错位。
- `rvc/src/rvc/pipeline.cpp:136-151`：新增 `release_inferencer`；空闲切换由后端先释放旧引用，
  使旧模型可在加载前被淘汰；流活动时保持旧模型与新模型原子换入的语义。
- `rvc/src/rvc/model_loader.cpp:307-342`：新模型需要缓存空位时先淘汰空闲模型；
  允许淘汰已释放引用的旧 current，腾出 GPU 槽位后再加载。
- `rvc/control/stream_session.cpp:140-185`：LoadModel 空闲切换失败时清空会话模型状态，
  不再把旧模型报为已加载；活动流期间沿用旧模型直到新模型换入。
- `rvc/src/rvc/file_worker.cpp:61-151`：完成日志增加实际输出样本数，便于核对长度契约。
- `tools/jetson-file-multimodel.sh`：多模型文件转换板端测试；`tools/jetson-e2e.sh --file-models` 接入。
- `tools/model/setup-export-env.sh`：默认 venv 改为 `~/mozart-toolkit/rvc-env`；
  依赖增加 `faiss-cpu` 与 `praat-parselmouth`，满足 RVC 参考代码导入。

## 配置

| 项 | 值 |
| --- | --- |
| 设备 | Jetson Orin Nano Super 8 GB，aarch64，6 核，7546 MiB 内存 |
| 源码 | commit `3611f82` + 本分支未提交的多模型改动（部署记录 `dirty=true`） |
| 二进制 | `~/mozart-runtime/bin`，`jetson-release` C++ 与 cargo debug |
| 推理 | `device = cuda`，`half = false`，`cache_limit = 2`，`f0_method = rmvpe` |
| 引擎 | TensorRT 优先；本轮 10 个模型均命中 TensorRT，脚本断言无新增 ONNX backend（实测） |
| 文件段 | `denoise = false`，`max_duration_seconds = 60`（长文件用例 300），队列串行 |
| 会话参数 | `pitch_shift = 0`、`index_rate = 0`、`rms_mix_rate = 1.0`、`protect = 0.33`（默认值） |
| 输入 | `~/mozart-samples/jieshe.m4a` 截取 10 s，16 kHz 单声道 WAV，160000 样本 |
| 输入 SHA-256 | `ffe613166f8a43a416a17af35f98427c47ef58e9a91f9d15da8d252dd19a6015` |
| 测试脚本 | `tools/jetson-file-multimodel.sh`；证据 `/tmp/mozart-multimodel-final3`（全量）、`/tmp/mozart-multimodel-index-final2`、`/tmp/mozart-multimodel-long-final3` |

## 多模型结果

`frames_in` / `frames_out` / `output samples` 取自 rvc 日志的 `FileWorker finished` 行。
`GetStatus` 在 StopStream 后可能已切换到下一条实时流，状态里的 `frames_out` 可能为 0；
本表不以该字段作判据。

| 模型 | 采样率 | 生成器 | index | 终态 | 帧入/出 | 输出样本 | 推理 ms | 墙钟 s | RMS |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| de-narrator | 48 kHz | 静态 T=200，23 窗 | 无 | completed | 500/500 | 480000 | 25516 | 27 | 0.08962 |
| frieren-v2-40k | 40 kHz | 动态 | 无 | completed | 500/500 | 480000 | 7513 | 11 | 0.11272 |
| liuying | 48 kHz | 动态 | 无 | completed | 500/500 | 480000 | 7652 | 11 | 0.10359 |
| mike-tyson | 40 kHz | 动态 | 无 | completed | 500/500 | 480000 | 7484 | 10 | 0.11231 |
| miku-v2-48k | 48 kHz | 动态 | 无 | completed | 500/500 | 480000 | 7600 | 11 | 0.09998 |
| qiqi-en | 48 kHz | 整段 T=2380 | 无 | completed | 500/500 | 480000 | 8847 | 12 | 0.10667 |
| qiqi-zh | 48 kHz | 静态 T=200，23 窗 | 无 | completed | 500/500 | 480000 | 22716 | 25 | 0.08733 |
| special-week-v2-48k | 48 kHz | 动态 | 有 | completed | 500/500 | 480000 | 7615 | 12 | 0.07606 |
| tokai-teio-v2-48k | 48 kHz | 动态 | 有 | completed | 500/500 | 480000 | 7624 | 11 | 0.11483 |
| trump-v2 | 40 kHz | 动态 | 有 | completed | 500/500 | 480000 | 7472 | 10 | 0.10092 |

静态 T=200 模型按 1 s 窗、0.5 s hop 分窗（含 2 s 边界 pad），10 s 输入产生 23 个 Generator 窗，推理约 23–26 s。
动态与整段模型一次 Generator 调用，推理约 7.5–8.8 s。两者不是同一计时口径下的性能比较。

修复前对照：`mike-tyson` 与 `tokai-teio-v2-48k` 因 TensorRT 显存分配失败回退 ONNX CPU，
推理分别为 33329 ms 与 38599 ms；修复后为 7484 ms 与 7624 ms（实测）。

## 附加路径

| 场景 | 配置 | 结果 |
| --- | --- | --- |
| 索引检索 | `special-week-v2-48k`，`index_rate = 0.5` | completed，500/500 帧，480000 样本，10 s，推理 11994 ms，墙钟 16 s，RMS 0.08137 |
| 长文件 | `frieren-v2-40k`，120 s 输入，`max_duration_seconds = 300` | completed，6000/6000 帧，5760000 样本，120 s，121 窗，推理 75251 ms，墙钟 79 s，RMS 0.09012 |
| 并发提交 | `frieren-v2-40k` 与 `mike-tyson` 连续提交 | 两任务均 completed；FileWorker 行各 500/500、480000 样本；串行执行 |

三次附加运行均无 TensorRT 回退 ONNX（实测）。

## golden 对拍

参考由未改动的 RVC 源码与原始 `.pth` 在 CPU PyTorch 上生成，输入与文件链相同。

| 项 | 值 |
| --- | --- |
| 模型 | `~/rvc-pth/frieren-v2-40k.pth`，SHA-256 `54d59e395a13c913c489b2f60b9a758c88b320ca20544986de0457c1599521a9` |
| RVC 源码 | `~/mozart-assets/RVC`，commit `81eed5e8f68b6bed1789f682fe78cdd324495afc` |
| 参考环境 | `~/mozart-toolkit/rvc-env`，torch 2.15.0+cu132，transformers 4.46.3，librosa 1.0.1，numpy 2.5.4 |
| 参考输出 | 40 kHz，399200 样本，SHA-256 `b2519fb91c19ae4fd283d3747640f63e79cff94bbb111e6dfd5d8e626d7b6e8b` |
| Mozart 输出 | 48 kHz，480000 样本，SHA-256 `969c09ec4cbfed1d0f956979ec2fa3f5e3b0305ffd34b8b3e2b24f68f1d2e705` |

| 指标 | 值 | 判据 |
| --- | --- | --- |
| 对齐滞后 | 0 ms | 0 ms |
| log-mel 相关 | 0.99305 | ≥ 0.99 |
| log-mel MAE | 1.58 dB | 记录值 |
| RMS 包络相关 | 0.99682 | ≥ 0.99 |
| RMS 比值 | +0.014 dB | ±1 dB |
| F0 中位偏差 | 10 音分 | ≤ 50 音分 |
| F0 P90 偏差 | 30 音分 | 记录值 |
| 有声区 IoU | 0.897 | ≥ 0.85 |
| 时长差 | +20 ms | ≤ 100 ms |
| 削波比例 | 0% | 0% |

动态生成器包含随机隐变量与源噪声。C++ 用 `std::mt19937` 固定种子，PyTorch 用 `torch.manual_seed`；
两者随机序列不同，因此波形不逐样本一致，对拍采用上面这组质量指标（代码确认）。
本次参考运行 `index_rate = 0`、`f0_method = rmvpe`，不触达 faiss 索引与 parselmouth 的 pm F0；
导入桩只满足模块导入（代码确认）。
参考输出比输入短 20 ms，是原版 RVC 按 Generator 帧取整的结果；Mozart 按文件数据面契约补齐到 10 s。

## 问题与修复

1. **动态模型 `499/500` 帧停住**。动态分支按 HuBERT 特征帧数推导生成器 T，
   192000 样本输入得到 1198 帧，输出经边界裁剪后为 479040 样本，比契约少 960 样本。
   文件 worker 等待第 500 帧，看门狗判失败。统一收口后为 480000 样本（实测）。
2. **静态整段 `qiqi-en` 输出 505 帧**。固定 T 生成器输出长于输入，单窗路径只补不裁。
   统一收口后为 500 帧（实测）。
3. **模型切换 OOM**。旧模型的 `inferencer_` 引用使其无法在加载前淘汰，
   新旧模型同时驻留触发 TensorRT 显存不足，回退 ONNX CPU。
   现在空闲切换先释放引用，加载前淘汰旧 current 并把空闲缓存压到 `cache_limit - 1`；
   活动流保持旧模型（代码确认，板端实测无回退）。
## 适用范围

配置见「配置」一节。全部数值来自该配置下的板端实测，未跨设备外推。

样本数：

- 多模型转换 10 个模型，每个 1 次 10 s 输入，共 10 次；证据在 `/tmp/mozart-multimodel-final3`。
- 索引检索 1 次、120 s 长文件 1 次、并发双任务 1 轮；证据见「附加路径」。
- golden 1 个模型、1 条 10 s 输入；参考、张量与对比结果在 `/tmp/mozart-golden-frieren`。
- 修复过程中共跑多轮全模型：`fix2` 仅修输出长度；`fix3` 加缓存淘汰；
  `final3`（本报告的 `/tmp/mozart-multimodel-final3`）为最终干净轮，含会话状态与脚本判据收紧。

未验证项：

1. **实时链路多模型**。阻塞原因：实时契约仍固定 front 280 / decoder 30 / hubert 44800 / rmvpe 32，
   40 kHz 模型只开放文件链路。解除条件：实时形状改为从模型能力推导后，在板端跑 40 kHz 模型。
2. **动态生成器逐样本 golden**。阻塞原因：C++ 与 PyTorch 随机序列不同。
   解除条件：固定同一随机实现，或改用确定性导出模型。
3. **32 kHz 模型**。阻塞原因：设备现有模型只有 40 kHz 与 48 kHz。
   解除条件：加入 32 kHz 模型后重跑本脚本。
4. **全新 venv 安装 golden 依赖**。阻塞原因：`faiss-cpu` 在 aarch64 上源码编译耗时不可控，
   本次改用导入桩。解除条件：在可联网设备执行 `tools/model/setup-export-env.sh` 并校验导入。
