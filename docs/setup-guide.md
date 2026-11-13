# Mozart 配置与使用手册

## 1 完整目录结构

一个能运行全部功能的部署目录：

```
~/mozart/
├── bin/                              ← 全部二进制，daemon 按同目录找子进程
│   ├── mozart-ctl                    ← CLI 入口，你只跟它打交道
│   ├── mozart-daemon                 ← 控制面，up 时自动拉起
│   ├── mozart-pre                    ← 音频采集 / DSP / 播放
│   ├── mozart-rvc                    ← 变声推理（C++）
│   ├── mozart-asr                    ← 语音识别
│   ├── mozart-tts                    ← 语音合成
│   └── mozart-bridge                 ← 字幕 / 翻译 / 播报编排
├── mozart.toml                       ← 配置文件，放这里自动识别
└── assets/                           ← 仓外资产（模型、共享库）
    ├── lib/
    │   └── libsherpa-onnx-c-api.so   ← ASR/TTS 推理库
    ├── llama/
    │   ├── llama-server              ← 翻译推理服务
    │   └── *.so                      ← 它的共享库
    └── models/
        ├── rvc/qiqi-zh-run/          ← 变声模型
        │   ├── qiqi-zh.onnx
        │   ├── qiqi-zh-front.onnx
        │   ├── qiqi-zh-decoder.onnx
        │   ├── qiqi-zh.index
        │   ├── config.json
        │   ├── hubert_base_dynamic.onnx
        │   ├── rmvpe_dynamic.onnx
        │   ├── hubert-realtime.onnx
        │   └── rmvpe-realtime.onnx
        ├── asr/zipformer-zh-14M/
        ├── asr-review/<sense-voice>/
        ├── tts/<pocket-tts>/
        │   └── test_wavs/bria.wav    ← 播报参考音色
        └── llm/<model>.gguf
```

运行时自动产生，不用手动建：

```
<run_dir>/                            ← TOML 里 [daemon].run_dir 指定
├── *.sock                            ← 进程间通信
├── daemon.lock
└── logs/*.log                        ← 各进程日志

~/.local/state/mozart/
├── subtitles.jsonl                   ← 字幕输出
├── bridge-state.json                 ← 翻译/播报开关持久化
└── files/                            ← 文件转换的源副本与结果
```

二进制来源：Cargo 产出 mozart-ctl/daemon/pre/asr/tts/bridge，CMake 产出 mozart-rvc。
把 mozart-rvc 复制到 bin/ 即可。llama-server 从 llama.cpp 编译，放 assets/llama/。

## 2 配置：什么功能依赖什么

配置文件是 `mozart.toml`。不写的段 → 对应进程不启动。按需组合：

| 功能 | 需要的配置段 | 需要的资产 |
| --- | --- | --- |
| 直出（降噪→播放） | `[daemon]` `[process].pre_bin` `[pre]` | 无 |
| 实时变声 | 上面 + `[process].rvc_bin` `[rvc]` | RVC 模型目录 |
| 文件转换 | 实时变声 + `[file]` | 同上 + ffmpeg |
| 字幕+翻译+播报 | 直出 + `[process]` 全部 + `[asr]` `[tts]` `[bridge]` | sherpa 库、ASR/TTS/LLM 模型、llama-server |

### 2.1 最小配置（仅直出）

```toml
[daemon]
run_dir = "/tmp/mozart-run"

[process]
pre_bin = "mozart-pre"

[pre]
capture_device = "default"
playback_device = "default"
```

### 2.2 加变声

追加：

```toml
[process]
rvc_bin = "mozart-rvc"

[rvc]
model_id = "qiqi-zh"
models_dir = "/home/<user>/mozart/assets/models/rvc/qiqi-zh-run"
device = "cuda"
hubert_path = "/home/<user>/mozart/assets/models/rvc/qiqi-zh-run/hubert_base_dynamic.onnx"
rmvpe_path = "/home/<user>/mozart/assets/models/rvc/qiqi-zh-run/rmvpe_dynamic.onnx"
realtime_hubert_path = "/home/<user>/mozart/assets/models/rvc/qiqi-zh-run/hubert-realtime.onnx"
realtime_rmvpe_path = "/home/<user>/mozart/assets/models/rvc/qiqi-zh-run/rmvpe-realtime.onnx"
```

`[rvc]` 里四个引擎路径必须指向实际文件。实时变声要求 `-front.onnx` 和 `-decoder.onnx` 齐全。
同目录有 `.engine` 文件时优先走 TensorRT，加载失败删掉即回退 ONNX Runtime。

### 2.3 加文件转换

追加：

```toml
[file]
storage_dir = "~/.local/state/mozart/files/"
max_duration_seconds = 300
ffmpeg_path = "ffmpeg"
```

文件数据面复用 `[rvc]` 的 socket，不需要额外 socket 配置。

### 2.4 加语言链

追加：

```toml
[process]
asr_bin = "mozart-asr"
tts_bin = "mozart-tts"
bridge_bin = "mozart-bridge"
llama_bin = "/home/<user>/mozart/assets/llama/llama-server"

[asr]
model_dir = "/home/<user>/mozart/assets/models/asr/zipformer-zh-14M"
review_model_dir = "/home/<user>/mozart/assets/models/asr-review/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17"

[tts]
model_dir = "/home/<user>/mozart/assets/models/tts/sherpa-onnx-pocket-tts-int8-2026-01-26"
reference_wav = "/home/<user>/mozart/assets/models/tts/sherpa-onnx-pocket-tts-int8-2026-01-26/test_wavs/bria.wav"
provider = "cpu"

[bridge]
subtitles_path = "~/.local/state/mozart/subtitles.jsonl"
llama_model = "/home/<user>/mozart/assets/models/llm/qwen35-0.8b-text-Q4_K_M.gguf"
translate_enabled = true
speech_enabled = true
```

语言链不需要真变声模型。`rvc_bin` 可指向 `examples/rvc_stub`。

### 2.5 音频设备

`[pre]` 的 `capture_device` / `playback_device` 填 ALSA 设备名。
查看可用设备：`mozart-ctl device list`（启动后）或 `arecord -l` / `aplay -l`。

用 `plughw:` 前缀（ALSA 自动转换格式），不用 `hw:`（pre 固定要 48 kHz / 2ch / S16_LE，设备不支持会退出）。
无物理设备时加载 `snd-aloop` 内核模块，设备名形如 `plughw:CARD=Loopback,DEV=1,SUBDEV=2`。

换设备：改 TOML 后 `mozart-ctl restart pre`，断流 1–2 s。

## 3 启动与操作

### 3.1 启动

```bash
cd ~/mozart/bin
mozart-ctl up
```

一条命令拉起整个栈。daemon 自动 spawn 配置里写了的所有子进程。
不需要手动启动 pre、rvc、asr 等。`up` 幂等，已运行返回 already-running。

### 3.2 查看状态

```bash
mozart-ctl status            # 人读
mozart-ctl status --json     # 机器可读
```

### 3.3 实时变声

```bash
mozart-ctl route set rvc     # 切到变声
mozart-ctl route set direct  # 切回直出
mozart-ctl mute on           # 静音
mozart-ctl mute off          # 取消静音
```

对麦克风说话，扬声器输出目标音色。切换有 10 ms 淡变，无爆音。

### 3.4 文件转换

```bash
mozart-ctl file submit /path/to/audio.wav --model qiqi-zh
mozart-ctl file status <job_id>
mozart-ctl file fetch <job_id> -o result.wav
mozart-ctl file cancel <job_id>
mozart-ctl file list
mozart-ctl file pause        # 暂停队列
mozart-ctl file resume       # 恢复队列
```

输出固定 WAV（pcm_f32le / 48 kHz / 单声道）。
入口限制：`.wav/.mp3/.m4a/.aac/.flac/.ogg`，≤ 100 MiB，≤ 300 s。
文件转换与实时变声互斥（GPU 单资源），正在变声时文件排队，切 direct 后自动开始。

### 3.5 字幕 / 翻译 / 播报

配置了语言链段后，`up` 即自动运行。对麦克风说话：

- 字幕写入 `subtitles_path`（JSONL，每行一句，含原文和译文）
- 播报从扬声器输出参考音色译文

运行时开关：

```bash
mozart-ctl translate on / off    # 翻译
mozart-ctl speech on / off       # 播报
mozart-ctl speech stop           # 停当前播报
```

开关持久化，重启后恢复。播报时实时输出自动静音（分时），播完恢复。

### 3.6 管理

```bash
mozart-ctl model list                  # 可用模型
mozart-ctl device list [--all]         # 音频设备
mozart-ctl restart <pre|rvc|asr|tts|bridge|llama>   # 重启单个服务
mozart-ctl config get [key]            # 读配置
mozart-ctl config set <key> <value>    # 改配置（v1 仅设备键）
mozart-ctl down                        # 关停整个栈
```

退出码：0 成功 / 1 其他 / 2 参数错 / 3 daemon 不可达 / 4 被拒 / 5 超时。

### 3.7 日志

`<run_dir>/logs/` 下按进程名分文件。出问题先看对应日志。

## 4 运行时环境

语言链需要两个环境变量（直出和变声不需要）：

```bash
export LD_LIBRARY_PATH=~/mozart/assets/lib:~/mozart/assets/llama
```

- `assets/lib`：让 mozart-asr / mozart-tts 找到 libsherpa-onnx-c-api.so
- `assets/llama`：让 llama-server 找到自身的 libggml*.so

构建时如果没设 `SHERPA_ONNX_LIB_DIR`，asr/tts 会编译为 stub（空输出）。
确认方法：`ldd bin/mozart-asr | grep sherpa`，有输出且不是 "not found" 才是真链接。

## 5 配置文件加载机制

不需要每次加 `--config`。解析顺序（`core/src/config.rs`）：

1. `--config <path>`（显式指定）
2. `MOZART_CONFIG` 环境变量
3. **当前目录下的 `mozart.toml`**（自动检测）
4. 内置默认

把 `mozart.toml` 放在你运行命令的目录下，直接 `mozart-ctl up` 即可。
或者设一次环境变量：

```bash
export MOZART_CONFIG=~/mozart/mozart.toml
```

之后在任何目录都不用加参数。daemon 启动后把解析出的绝对路径传给子进程，子进程不需要自己找。

## 6 运行时依赖

Mozart 进程本身依赖的系统库：

| 依赖 | 用途 | 必需场景 |
| --- | --- | --- |
| libasound2（ALSA） | 音频采集/播放 | 全部 |
| libonnxruntime / libnvinfer | 推理后端 | 变声（板端 JetPack 自带） |
| libsherpa-onnx-c-api.so | ASR/TTS 推理 | 语言链（仓外资产） |
| ffmpeg | 文件解码/编码 | 文件转换 |
| llama-server + 其 .so | 翻译推理 | 语言链（仓外资产） |

**不依赖** aplay、arecord、Python、PyTorch。
aplay/arecord 是 ALSA 工具包里的调试命令，不是 Mozart 的运行时组件。
pre 通过 ALSA C API 直接操作声卡，不 shell 外部命令。

## 7 已知限制

- 换设备需重启 pre，断流 1–2 s，热切换未实现。
- `config set` v1 仅支持设备键，其余键手改 TOML 后重启。
- `config save`、`logs` 命令未实现。
- HTTP/SSE 接口和 web 前端未实现，v1 只有 CLI。
- 播报与实时是分时（播报时静音），不是混音。
- 文件结果是有限缓存（默认 1000 MiB），超限驱逐旧结果，需及时 fetch。