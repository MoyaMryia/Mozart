# Mozart 配置与使用指南

## 一、目录结构

集中放在 `~/mozart/`，**所有 `mozart-*` 二进制必须放在同一个 `bin/` 目录**：

```text
~/mozart/
├── mozart.toml                       ← 配置文件
├── bin/                              ← 全部可执行文件放同一目录
│   ├── mozart-ctl                    ← CLI 入口
│   ├── mozart-daemon                 ← 守护总管
│   ├── mozart-pre                    ← 音频采集 / DSP / 播放
│   ├── mozart-rvc                    ← 变声推理（CMake 产物）
│   ├── mozart-asr                    ← 语音识别（可选）
│   ├── mozart-tts                    ← 语音合成（可选）
│   └── mozart-bridge                 ← 字幕 / 翻译 / 播报（可选）
└── assets/                           ← 模型与共享库（按需放置）
    ├── lib/libsherpa-onnx-c-api.so
    ├── llama/                        ← llama-server 及其 .so
    └── models/
        ├── rvc/<模型目录>/           ← .onnx / -front.onnx / -decoder.onnx / hubert / rmvpe
        ├── asr/<zipformer>/
        ├── asr-review/<sense-voice>/
        ├── tts/<pocket-tts>/
        └── llm/<model>.gguf
```

从仓库复制构建产物（debug 为例）：

```bash
mkdir -p ~/mozart/bin ~/mozart/assets
cp target/debug/mozart-{ctl,daemon,pre,asr,tts,bridge} ~/mozart/bin/
cp build/jetson-release/rvc/mozart-rvc ~/mozart/bin/   # 本机为 build/host-debug/rvc/mozart-rvc
```

运行时自动生成：`run_dir`（socket 与 `logs/*.log`）、`~/.local/state/mozart/`（字幕 `subtitles.jsonl`、开关状态、文件缓存）。

---

## 二、配置文件（`mozart.toml`）

- **加载优先级**：`CLI内 --config <路径>` > `环境变量 MOZART_CONFIG` > `./mozart.toml` > 内置默认
- **规则**：所有路径写**绝对路径**；同名 `[段名]` 只写一次（`[[models]]` 可写多块），不用的功能删掉对应段

| 功能 | 需要的配置段 | 额外依赖 |
| --- | --- | --- |
| 基础直出（降噪 → 播放） | `[daemon]` `[process]` `[pre]`（`[pre.dsp]` 可选） | 无 |
| 实时变声 | 上面 + `[[models]]` `[rvc]` | RVC 模型目录 |
| 离线文件变声 | 实时变声 + `[file]` | 同上 + `ffmpeg` |
| 字幕 + 翻译 + 播报 | 基础直出 + `[process.env]` `[asr]` `[tts]` `[bridge]` | sherpa 库、ASR/TTS/LLM 模型、`llama-server` |

```toml
[daemon]
run_dir = "/tmp/mozart-run"          # socket 与日志目录

[process]
pre_bin = "mozart-pre"               # 音频预处理进程（同目录写裸名即可）
rvc_bin = "mozart-rvc"               # 变声推理进程
asr_bin = "mozart-asr"               # 语音识别进程
tts_bin = "mozart-tts"               # 语音合成进程
bridge_bin = "mozart-bridge"         # 字幕与翻译调度进程
llama_bin = "/home/<user>/mozart/assets/llama/llama-server"   # 翻译服务绝对路径

[process.env]                        # 仅语言链需要，启动子进程时注入
LD_LIBRARY_PATH = "/home/<user>/mozart/assets/lib:/home/<user>/mozart/assets/llama"

[pre]
capture_device = "default"           # 麦克风 ALSA 名，启动后用 device list 查到 plughw 再改
playback_device = "default"          # 扬声器 ALSA 名

[pre.dsp]
denoise = true                       # 降噪
gate = false                         # 噪声门
agc = false                          # 自动增益
limiter = false                      # 峰值限幅

# ---- 模型注册表（可写多个 [[models]]；留空时 rvc 回退扫描 models_dir 子目录）----
[[models]]
id = "qiqi-zh"                       # 模型标识（rvc.model_id / model list / file submit --model 引用）
type = "rvc"                         # 模型类型：rvc / asr / tts / llm
path = "/home/<user>/mozart/assets/models/rvc/qiqi-zh-run"   # 目录绝对路径（llm 为 .gguf 文件路径）

[rvc]                                # 不用变声可整段删除；hubert/rmvpe 默认从 models_dir 自动发现
model_id = "qiqi-zh"                 # 默认加载的模型 id（对应上面 [[models]].id）
models_dir = "/home/<user>/mozart/assets/models/rvc/qiqi-zh-run"  # 特征引擎目录 & 回退扫描目录
device = "cuda"                      # 推理设备：cuda 或 cpu
pitch_shift = 0                      # 升降调（半音）：-12..=12
index_rate = 0.0                     # 音色索引权重：0.0..=1.0
rms_mix_rate = 1.0                   # 音量包络融合：0.0..=1.0
protect = 0.33                       # 清辅音保护：0.0..=0.5

[file]                               # 不用文件转换可整段删除
storage_dir = "/home/<user>/.local/state/mozart/files"   # 任务与输出 WAV 缓存目录
max_duration_seconds = 300           # 单文件最长时长（秒）
max_upload_bytes = 104857600         # 单文件大小上限（100 MiB）
ffmpeg_path = "ffmpeg"               # ffmpeg 命令名或绝对路径

[asr]                                # 不用语言链可删 [asr] [tts] [bridge]
model_dir = "/home/<user>/mozart/assets/models/asr/zipformer-zh-14M"   # 流式识别模型目录
review_model_dir = "/home/<user>/mozart/assets/models/asr-review/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17"   # 复核模型目录（留空关闭）

[tts]
model_dir = "/home/<user>/mozart/assets/models/tts/sherpa-onnx-pocket-tts-int8-2026-01-26"           # 语音合成模型目录
reference_wav = "/home/<user>/mozart/assets/models/tts/sherpa-onnx-pocket-tts-int8-2026-01-26/test_wavs/bria.wav"   # 播报参考音色
provider = "cpu"                     # 推理后端：cpu 或 cuda

[bridge]
subtitles_path = "/home/<user>/.local/state/mozart/subtitles.jsonl"    # 字幕 JSONL 输出路径
llama_model = "/home/<user>/mozart/assets/models/llm/qwen35-0.8b-text-Q4_K_M.gguf"   # 翻译 GGUF 模型
translate_enabled = true             # 启动时默认开启翻译
speech_enabled = true                # 启动时默认开启语音播报
```

---

## 三、操作命令

```bash
cd ~/mozart
export PATH="$PWD/bin:$PATH"
```

### 1. 启停与状态

```bash
mozart-ctl up               # 启动（幂等）
mozart-ctl status           # 查看状态（加 --json 输出 JSON）
mozart-ctl down             # 关停
```

### 2. 首次配置声卡

```bash
mozart-ctl device list                                       # 列出推荐设备（plughw / default）
mozart-ctl device list --all                                 # 列出全部 ALSA 别名
mozart-ctl config set pre.capture_device "plughw:CARD=..."   # 设置麦克风
mozart-ctl config set pre.playback_device "plughw:CARD=..."  # 设置扬声器
mozart-ctl restart pre                                       # 重启 pre 生效（断流 1–2 s）
```

> 选 `plughw:`（自动转换采样率/格式），不要选 `hw:`。无物理声卡时可 `sudo modprobe snd-aloop` 后用 `device list` 查看。

### 3. 实时变声

```bash
mozart-ctl route set rvc      # 切到变声
mozart-ctl route set direct   # 切回直出
mozart-ctl mute on / off      # 静音 / 取消静音
```

### 4. 文件变声

支持 `.wav/.mp3/.m4a/.aac/.flac/.ogg`（≤ 100 MiB，≤ 300 s），输出 48 kHz 单声道 WAV。与实时变声共用 GPU，开着实时变声时提交会排队，切回 `direct` 后自动执行。

```bash
mozart-ctl file submit /path/to/audio.mp3 [--model qiqi-zh]
mozart-ctl file list
mozart-ctl file status <job_id>
mozart-ctl file fetch <job_id> -o out.wav
mozart-ctl file cancel <job_id>
mozart-ctl file pause / resume
```

### 5. 字幕 / 翻译 / 播报

```bash
mozart-ctl translate on / off   # 翻译开关（持久化）
mozart-ctl speech on / off      # 播报开关（持久化；播报时自动静音实时输出）
mozart-ctl speech stop          # 急停并清空待播队列
```

### 6. 管理与排障

```bash
mozart-ctl model list                                 # 查看已注册模型与加载状态
mozart-ctl config get [key]                           # 读取配置（config set 仅支持设备键）
mozart-ctl restart <pre|rvc|asr|tts|bridge|llama>     # 重启单个服务使 TOML 改动生效
```

- **日志**：`<run_dir>/logs/<进程名>.log`。
- **检查语言链非 stub**：`ldd ~/mozart/bin/mozart-asr | grep sherpa` 有输出且无 `not found`。
- **退出码**：`0` 成功 / `1` 一般错误 / `2` 参数错 / `3` 连不上 daemon / `4` 被拒绝 / `5` 超时。
