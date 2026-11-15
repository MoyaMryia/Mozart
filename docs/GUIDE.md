# Mozart 使用手册

本手册只讲配置概念与 CLI 操作。
设备目录、资产、部署与测试见 [SETUP.md](SETUP.md)；TOML 字段定义见 [config.md](config.md)。

## 一、配置

加载优先级：`--config <路径>` > 环境变量 `MOZART_CONFIG` > `./mozart.toml` > 内置默认。

规则：

- 路径与模型由部署环境配置，不写进本手册。
- 不用的功能整段删除对应配置。
- 同名段只写一次；`[[models]]` 可写多块。
- 完整字段、默认值与示例见 [config.md](config.md) 与 [SETUP.md](SETUP.md)。

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
```

## 三、设备选择

```bash
mozart-ctl device list                              # 列出推荐设备
mozart-ctl device list --all                        # 列出全部 ALSA 别名
mozart-ctl config set pre.capture_device "<设备名>"  # 设置麦克风
mozart-ctl config set pre.playback_device "<设备名>" # 设置扬声器
mozart-ctl restart pre                              # 重启 pre 生效
```

优先选 `plughw:`，不要选 `hw:`。无物理声卡时可用 `snd-aloop`。

## 四、实时变声

```bash
mozart-ctl route set rvc      # 切到变声
mozart-ctl route set direct   # 切回直出
mozart-ctl mute on            # 静音
mozart-ctl mute off           # 取消静音
```

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
```

## 七、管理与排障

```bash
mozart-ctl model list [--available]                # 查看已注册模型与加载状态；--available 只列资产存在的模型
mozart-ctl config get <键>                          # 读取配置基线
mozart-ctl restart <pre|rvc|asr|tts|bridge|llama>  # 重启单个服务
```

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
