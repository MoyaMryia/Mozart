# 语言链路板端实测

日期：2026-11-13。设备：Jetson Orin Nano Super 8 GB。被测树：提交 `0af1118` 加本次工具链改动。

## 结论

语言链路（ASR + 字幕桥 + 翻译 + TTS）在板端跑通，并固化为一条命令：`tools/jetson-language-test.sh`。

单轮实测：30 s 中文语音 → ASR `finals=12` → 释义 16 句 → 字幕双语 final 17 条 → TTS 合成成功 5 句（测量时队列仍在消化）、失败 0 → 播放回录 RMS 0.0817 → 关停无残留。

同次部署门：Rust 测试 492 通过、CTest 7/7、三进程冒烟通过，耗时 137 s。

## 配置

| 项 | 值 |
| --- | --- |
| 设备 | Jetson Orin Nano Super 8 GB，aarch64，JetPack 6 |
| 二进制 | `~/Mozart/target/debug/{mozart-daemon,mozart-pre,mozart-asr,mozart-tts,mozart-bridge,mozart-ctl}` + `examples/rvc_stub`；`mozart-rvc` 未加载模型（rvc 桩顶替） |
| 资产根 | `~/mozart-assets`（sherpa 库、llama-server、模型；布局见下） |
| ASR | `zipformer-zh-14M`；复核 SenseVoice int8 |
| TTS | PocketTTS int8，`provider = "cpu"`，参考音 `test_wavs/bria.wav` |
| 翻译 | llama-server（CUDA 构建）+ `qwen35-0.8b-text-Q4_K_M.gguf`，Unix socket 原生 HTTP |
| 音频拓扑 | snd-aloop：采集 `DEV=1,SUBDEV=4`，播放 `DEV=0,SUBDEV=5` |
| 采样 | 48 kHz S16 双声道；pre DSP `denoise = true` |
| 控制 | poll 1 s，rpc 超时 2 s；字幕 `~`/测试目录下 `subtitles.jsonl` |
| 输入 | `~/mozart-samples/jieshe.m4a` 第 60–90 s，30 s 真实录音截取 |

## 仓外资产布局与启用

```
~/mozart-assets/
├── lib/libsherpa-onnx-c-api.so
├── llama/llama-server 与全部 .so（libllama-server-impl、libggml-* 等）
└── models/{asr,asr-review,tts,llm,rvc}
```

- 构建：`SHERPA_ONNX_LIB_DIR=$HOME/mozart-assets/lib`；未设置时 asr/tts 为 stub。
- 运行：`LD_LIBRARY_PATH=$HOME/mozart-assets/lib:$HOME/mozart-assets/llama`，daemon 及其子进程继承；llama-server 的共享库在其自身目录，必须显式提供。
- `tools/deploy-jetson.sh` 已自动注入以上两项（资产存在时）；`tools/jetson-smoke.sh` 与 `tools/jetson-file-test.sh` 默认强制语言进程 stub，只验各自链路。

## 结果

### 部署门（`tools/deploy-jetson.sh --smoke`）

| 项 | 结果 |
| --- | --- |
| `cargo test --workspace`（含 sherpa 链接构建） | 492 通过 / 0 失败 |
| `ctest` | 7/7 通过 |
| 三进程冒烟 | up → 2 s 就绪 → `route set rvc` → `kill -9 rvc` 自愈 → `route set direct` → down，无残留 |
| 总耗时 | 137 s |

### 语言链路（`tools/jetson-language-test.sh`）

| 指标 | 实测 |
| --- | --- |
| 六进程 | pre/rvc/asr/tts/bridge/llama 全部 running，0 重启 |
| ASR finals | 12 |
| 翻译 | translated=16，llama_connected=true |
| 播报投递 | speech_delivered=16 |
| TTS | synthesized=5，failed=0（测量时队列仍在消化，5 为时刻下界） |
| 字幕 | 双语 final 17 条（含修订合并） |
| 播放回录 | RMS 0.0817，非静音 |
| 关停 | down 后无 mozart 进程，控制 socket 全部失效 |

判据：六进程 running；ASR finals ≥ 1；翻译 ≥ 1；TTS synthesized ≥ 1 且 failed = 0；双语字幕 ≥ 1；回录 RMS > 0.01；关停无残留。全部满足。

## 期间修复

`daemon/tests/linkage.rs` 的 `wait_running` 只等进程 Running，`route set rvc` 却直接连 rvc 控制 socket；带 sherpa 构建时语言进程启动竞争放大该窗口，实测 3 轮 1 失败。就绪判据补 `status.rvc.is_some()` 后连跑 5 轮全过。产品代码未改。

## 复跑方式

```bash
tools/deploy-jetson.sh --smoke          # 构建（自动链接 sherpa）并跑测试门与实时冒烟
ssh <jetson> 'cd ~/Mozart && bash tools/jetson-language-test.sh'
```

可覆盖：`ASSETS_DIR`、`BIN_DIR`、`SMOKE_DIR`、`SAMPLE_M4A`、`SS_S`/`T_S`、`CAP_SUB`/`PB_SUB`、`READY_TIMEOUT`、`RESULT_TIMEOUT`。默认子设备 4/5，与 smoke/文件链路的 2/3 隔离，可并发运行。

## 适用范围

- 配置：见「配置」一节；全部数值来自该配置下的单轮板端实测，未跨设备或跨模型外推。
- 样本数：真实录音截取 30 s 一条；未做多轮重复与不同语音内容抽样。
- 未验证项 1：真实麦克风/扬声器声学验收。阻塞原因：测量走 snd-aloop 回环；解除条件：接物理声卡后按同一脚本复测，并评估 TTS 音色主观听感。
- 未验证项 2：复核（SenseVoice）质量与 partial 事件的网页展示。阻塞原因：`status` 未细分复核文本质量，web 未实现；解除条件：前端模块交付后按 SPEC-05 验收。
- 未验证项 3：24 h 稳定性与并发（翻译/播报与实时变声同时运行）。阻塞原因：单轮短时实测；解除条件：按 SPEC-08 系统级条目补长跑。
- 证据文件留在设备 `/tmp/mozart-language-test`，不入库：`language.toml`、`st-*.json`、`subtitles.jsonl`、`rec.raw`、`speech.wav`、`run/logs/*.log`。
