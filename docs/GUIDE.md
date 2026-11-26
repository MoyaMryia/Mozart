# Mozart 使用手册

本手册只讲配置概念与 CLI 操作。
设备目录、资产、部署与测试见 [SETUP.md](SETUP.md)；TOML 字段定义见 [config.md](config.md)。

## 一、配置

加载优先级：`--config <路径>` > 环境变量 `MOZART_CONFIG` > `./mozart.toml` > 内置默认。

规则：

- 路径与模型由部署环境配置，不写进本手册。
- 不用的功能整段删除对应配置。
- 同名段只写一次；`[[models]]` 可写多块。
- 完整字段、默认值与示例见 [config.md](config.md)。

| 功能 | 需要的配置段 | 额外依赖 |
| --- | --- | --- |
| 基础直出（降噪 → 播放） | `[daemon]` `[process]` `[pre]`（`[pre.dsp]` 可选） | 无 |
| 实时变声 | 基础 + `[[models]]` `[rvc]` | 变声模型 |
| 离线文件变声 | 实时变声 + `[file]` | `ffmpeg` |
| 字幕 + 翻译 + 播报 | 基础 + `[process.env]` `[asr]` `[tts]` `[bridge]` | sherpa 库、识别/合成/翻译模型、`llama-server` |

## 二、启动与状态

```bash
mozart-ctl up                 # 启动（幂等）
mozart-ctl status             # 查看状态；--json 输出稳定机器契约
mozart-ctl down               # 关停
mozart-ctl services stop      # 只停音频与 AI 业务服务，daemon 与网页保持在线
mozart-ctl services start     # 恢复业务服务
```

`services stop` 释放 GPU 与内存，网页仍可访问并显示停机状态；文件任务正在跑时拒绝，排队任务保留，恢复服务后继续。
停服期间新的文件提交被拒绝。整机关停用 `down`。

## 三、设备选择

```bash
mozart-ctl device list                              # 列出推荐设备
mozart-ctl device list --all                        # 列出全部 ALSA 别名
mozart-ctl device use capture "<设备名>"             # 实时临时切换麦克风（不断流、不写配置）
mozart-ctl device use playback "<设备名>"            # 实时临时切换扬声器（不断流、不写配置）
mozart-ctl config set pre.capture_device "<设备名>"  # 设置默认麦克风
mozart-ctl config set pre.playback_device "<设备名>" # 设置默认扬声器
mozart-ctl restart pre                              # 重启 pre 生效默认配置
```

优先选 `plughw:`，不要选 `hw:`。无物理声卡时可用 `snd-aloop`。

扬声器外放时，麦克风会拾取扬声器输出，可能引起播报自激或变声回环。可开启声学回声消除（AEC）：

```bash
mozart-ctl config set pre.aec.enabled true   # 开启回声消除，即时生效
mozart-ctl config set pre.aec.enabled false  # 关闭
```

默认关闭。网页设备面板有「回声消除 (AEC)」开关，并显示延时估计、回声衰减（ERLE）与参考丢帧。构建未链接 AEC3 库时开关置灰并显示「未链接」。

## 四、实时变声

```bash
mozart-ctl mode rvc           # 切到变声
mozart-ctl mode direct        # 切回直出
mozart-ctl mode broadcast     # 开启翻译播报模式
mozart-ctl mute on            # 静音
mozart-ctl mute off           # 取消静音
mozart-ctl model use <模型id>  # 换模型（实时或空闲）
```

默认以托管直出模式（`direct`）启动，不自动开启变声或播报；可在 `[daemon]` 中设置 `startup_mode`。

模式命令返回受理结果。用 `mozart-ctl status --json` 查看 `output_mode.applied`；三种模式都保留识别和翻译字幕。手动执行 `route set` 或 `speech on|off|stop` 会退出托管模式；`speech clear` 仅清队列与在途播报，保持当前托管模式不变。

`model use` 短暂断流：停流 → 载入新模型 → 载入该模型的 `presets/default.toml` → 重新起流。
它把模型选择写回配置基线，重启后沿用；`config set rvc.model_id` 被拒绝。
失败时自动恢复旧模型与旧参数；文件任务占用 GPU 时拒绝换模型。
换模型要加载引擎，等待时间比一般命令长。
输出采样率随模型（32k/40k/48k），播放固定 48 kHz，由 pre 在播放边界重采样。

## 五、文件转换

支持 `.wav`、`.mp3`、`.m4a`、`.aac`、`.flac`、`.ogg`。
默认上限：单文件 100 MiB、时长 300 s；输出 48 kHz 单声道 WAV。
与实时变声共用 GPU：实时占用时文件任务排队，切回直出后自动执行。

```bash
mozart-ctl file submit <文件> [--model <模型id>]
mozart-ctl file list
mozart-ctl file status <job_id>
mozart-ctl file fetch <job_id> -o <输出文件>
mozart-ctl file cancel <job_id>
mozart-ctl file pause
mozart-ctl file resume
```

## 六、字幕、翻译与播报

```bash
mozart-ctl translate on       # 开启翻译
mozart-ctl translate off      # 关闭翻译
mozart-ctl speech on          # 开启播报；播报时自动静音实时输出
mozart-ctl speech off         # 关闭播报
mozart-ctl speech stop        # 急停并清空待播队列
mozart-ctl speech clear       # 清空待播队列并中止在途播报，保持开关与托管模式不变
```

## 七、管理与排障

```bash
mozart-ctl model list [--available]                # 查看已注册模型与加载状态；--available 只列资产存在的模型
mozart-ctl preset list                              # 查看当前模型预设、live 参数与差异
mozart-ctl preset save                              # 把当前调参保存为该模型的 default 预设
mozart-ctl preset use                               # 重新载入当前模型的 default 预设
mozart-ctl config get <键>                          # 读取配置基线
mozart-ctl restart <pre|enhance|rvc|asr|tts|bridge|llama|web>  # 重启单个服务
```

`config get` 读基线值。键尚未重启生效时，值后追加一行 `pending` 提示，重启对应服务后消失。

`config set` 支持设备键、daemon/file 运行时键、RVC 会话参数与 `[rvc.realtime]` 旋钮；完整键表见 [config.md](config.md)。
实时旋钮中只有启动缓冲 `startup_delay_ms` 与静音跳过 `skip_silence` 可调。淡化与 SOLA 搜索是定值（合计 60 ms），写入其他值会被拒绝。
RVC 会话参数按模型保存：换模型时载入该模型的 `presets/default.toml`；调参后用 `preset save` 写回。文件链在 GPU 交接后取参数快照，任务开始后的改动留给下一任务。

日志：`<run_dir>/logs/<进程名>.log`，运行目录由配置给出，目录规范见 [SETUP.md](SETUP.md)。

退出码：

| 退出码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 一般错误 |
| 2 | 参数错 |
| 3 | 连不上 daemon |
| 4 | 被拒绝 |
| 5 | 超时 |

排障顺序：

1. `mozart-ctl status --json` 看 daemon 是否可达、pre 与 rvc 是否就绪。
2. 看对应进程日志，先定位首个错误。
3. 语言链前置（sherpa 库、模型、`llama-server`）见 [SETUP.md](SETUP.md)。

## 八、开发、部署与测试

```bash
tools/ci-local.sh all                          # 本机全量即时测试
tools/deploy-jetson.sh <SSH目标>                # 部署，不跑测试
tools/jetson-e2e.sh <SSH目标>                   # 部署后测试
```

阶段划分、套件选择与失败恢复见 [SETUP.md](SETUP.md)。
