# Mozart 配置规范（TOML schema）

状态：定稿中。最后更新：2026-11-17。

本文件是 TOML 配置的唯一 schema 依据：段、键、类型、默认、范围、运行时/重启分类。配置模型（TOML 基线 + 运行时状态文件、`config set`/`config save`、`restart_required` 契约）见 [SPEC-04](SPEC/SPEC-04-daemon.md)，本文件不复述。解析与校验实现于 `core/src/config.rs`，须与本文件一致。

当前配置写回的实现范围见 [SPEC-04 的“已确认”](SPEC/SPEC-04-daemon.md#已确认)。下表 R/S 分类是既定要求，不表示对应控制路径已全部实现。

## 分类原则

按机制 + 切换成本，**能即时生效就归运行时**：

- **运行时（R）**：键值每次操作才读（阈值/上限/开关/超时），或 owning 服务有运行中应用的控制命令。`config set` 写 TOML 基线 + 清该键状态覆盖 + 立即下发生效。
- **重启（S）**：键值启动时固化进长活结构（打开的设备、绑定的 socket、预分配的环、加载的模型、spawn 的二进制、存储目录）。`config set` 只写 TOML、不动运行进程，返回 `restart_required` + `affected`。`config get` 应答带 `pending_restart_keys`（尚未重启生效的键名，无则空数组），人读输出在值后追加一行待重启提示。

## 配置来源与优先级

`--config` > `MOZART_CONFIG` > `./mozart.toml` > 内置默认。环境变量覆盖：`MOZART_RUN_DIR`、`MOZART_*_BIN`、`MOZART_POLL_INTERVAL_MS`。运行时状态文件目录：`MOZART_STATE_DIR` > `~/.local/state/mozart`（文件名 `state.json`）。详见 SPEC-04。

## 段与键

类别列：R = 运行时，S = 重启。socket 键范围均为“纯文件名”（不含 `/`，非 `.`/`..`）。

### [daemon]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| run_dir | path | /run/mozart | — | S |
| socket | string | daemon.sock | 纯文件名 | S |
| poll_interval_ms | int | 1000 | >0 | R |
| rpc_timeout_ms | int | 2000 | >0 | R |
| start_timeout_ms | int | 5000 | >0 | R |
| restart_backoff_ms | int | 500 | >0 | R |
| restart_backoff_max_ms | int | 10000 | ≥ restart_backoff_ms | R |
| restart_stable_ms | int | 30000 | >0 | R |

### [process]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| pre_bin / rvc_bin / asr_bin / tts_bin / bridge_bin / llama_bin | path | 各裸名 | 非空 | S |

### [process.env]

任意键值对，daemon spawn 子进程时注入为环境变量。所有子进程（含 llama-server）继承同一份。典型用途：`LD_LIBRARY_PATH` 指向仓外共享库目录。

```toml
[process.env]
LD_LIBRARY_PATH = "<资产根>/lib:<资产根>/llama"
```

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| （任意） | string | 空表 | 键非空 | S |

注入发生在 spawn 时，修改后需重启对应服务生效。已存在的环境变量会被覆盖。

### [pre]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| socket | string | pre.sock | 纯文件名 | S |
| capture_device | string | default | 非空、无 NUL | S |
| capture_file | path | 空 | WAV PCM16、48000 Hz、至少 1 声道；pre 启动时校验；设置后替代 ALSA 采集 | S |
| playback_device | string | default | 非空、无 NUL | S |
| capture_queue_frames | int | 8 | 1..=1024 | S |
| consumers | array | rvc+asr 两支路 | 见 [[pre.consumers]]；未设置时 socket 沿用 [rvc]/[asr] 的 input_socket | S |

### [pre.dsp]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| denoise | bool | true | — | R |
| gate | bool | false | — | R |
| agc | bool | false | — | R |
| limiter | bool | false | — | R |

### [[pre.consumers]]

输入支路消费者列表（SPEC-02 分发）：一个消费者一个 socket，背压互不影响。未设置时回退 rvc+asr 两支路，socket 沿用 `[rvc].input_socket` / `[asr].input_socket`（rvc 受文件接管影响、asr 不受）；显式声明后列表整体生效，空列表表示不分发。

| 键 | 类型 | 必需 | 说明 |
| --- | --- | --- | --- |
| name | string | 是 | 消费者名，列表内唯一；GetStatus consumer_drops 的键。名为 rvc 时额外聚合输出链路丢弃；tts 与含点名为保留键 |
| socket | string | 是 | 输入 socket，纯文件名，相对 run_dir；对端绑定收帧，pre 连接发送 |
| file_mode_affected | bool | 否（默认 false） | 是否受文件接管影响：true 时接管期间停发、启动等 daemon 断言；false 恒发 |

类别 S：消费者 socket 启动时固化进发送端，修改需重启 pre。

### [[models]]

顶层数组，每项声明一个可用模型。`LoadModel`、`file submit --model`、`model list` 引用此注册表。

| 键 | 类型 | 必需 | 说明 |
| --- | --- | --- | --- |
| id | string | 是 | 模型标识，同一 type 内唯一 |
| type | string | 是 | rvc / asr / tts / llm |
| path | string | 是 | 模型目录绝对路径（rvc/asr/tts）或模型文件路径（llm） |

```toml
[[models]]
id = "<模型id>"
type = "rvc"
path = "<资产根>/models/rvc/<模型id>"

[[models]]
id = "<模型id>"
type = "asr"
path = "<资产根>/models/asr/<模型id>"
```

示例中的 `<资产根>` 与 `<模型id>` 由部署环境提供；设备路径规范见 [SETUP.md](SETUP.md)。

注册表为空时回退现有行为：rvc 扫描 `[rvc].models_dir` 子目录。类别 S。

### [rvc]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| socket / input_socket / output_socket | string | rvc.sock / rvc-in.sock / rvc-out.sock | 纯文件名 | S |
| model_id | string | 空 | — | S |
| f0_method | string | rmvpe | 仅 rmvpe | R |
| pitch_shift | int | 0 | -12..=12 | R |
| index_rate | float | 0.0 | 0..=1 | R |
| rms_mix_rate | float | 1.0 | 0..=1 | R |
| protect | float | 0.33 | 0..=0.5 | R |
| models_dir | string | 空 | 注册表为空时的回退扫描目录 | S |
| device | string | cuda | — | S |
| half | bool | false | — | S |
| cache_limit | int | 2 | — | S |
| hubert_path / rmvpe_path / realtime_hubert_path / realtime_rmvpe_path | string | 空（自动发现） | 可选，显式配置优先 | S |

特征引擎自动发现：四个路径为空时，从 `models_dir` 按约定文件名查找（`hubert_base_dynamic.onnx`、`rmvpe_dynamic.onnx`、`hubert-realtime.onnx`、`rmvpe-realtime.onnx`）。找到即用，找不到报错。

五个会话参数在运行中是「当前模型 live 草稿」；持久来源是模型目录内的 `presets/default.toml`（SPEC-11）。模型切换时 daemon 把该模型的预设写入这五个键并应用；`config set` 改 live 值并用 `preset save` 写回预设。

### [rvc.realtime]

实时链路非形状旋钮（全局一套，不进模型预设，文件链不读不扫）。默认值、上限与整除模数的单一来源是 `tools/model/realtime_chain_params.json` 的 `geometry` 段；C++ 镜像在 `rvc/include/rvc/realtime_geometry.hpp`，Rust 镜像在 `core/src/config.rs`，两侧由读 JSON 的对拍测试钉住（SPEC-03）。ms 按模型输出采样率换算样本。

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| crossfade_ms | int | 50 | 定值 50，偏离拒绝 | R |
| sola_search_ms | int | 10 | 定值 10，偏离拒绝 | R |
| startup_delay_ms | int | 400 | 0..=15000，且为 20 的倍数 | R |
| skip_silence | bool | true | — | R |

跨键约束：`crossfade_ms + sola_search_ms` 恒为 60 ms（50 + 10），两键不可调，偏离在配置入口与 `StartStream` 入口拒绝；原因与解除条件见 [SPEC-03 实时几何与资产参数](SPEC/SPEC-03-rvc.md#实时几何与资产参数)。可调的是 `startup_delay_ms` 与 `skip_silence`：前者上限 15000 来自 C++ 输出环 16 s @48 kHz 留 1 s 余量，整除模数 20 ms 来自契约帧网格。定值与上限按 48 kHz 契约换算，各采样率的 max_out 同比缩放，故 ms 边界与采样率无关。违反在入口拒绝，不到推理层抛异常。

四键归 R 类：`config set` 后实时流在跑且值变化时走 Stop→Start（见下文特殊机制说明）；overlap 两键只接受定值，实际可变的只有 `startup_delay_ms` 与 `skip_silence`。

### [asr]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| socket / input_socket / events_socket | string | asr.sock / asr-in.sock / asr-events.sock | 纯文件名 | S |
| model_dir | path | 空 | — | S |
| review_model_dir | path | 空 | — | S |
| threads | int | 2 | ≥1 | S |
| ring_capacity | int | 64 | ≥1 | S |
| output_queue_capacity | int | 64 | ≥1 | S |

### [tts]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| socket / jobs_socket / output_socket | string | tts.sock / tts-jobs.sock / tts-out.sock | 纯文件名 | S |
| model_dir | path | 空 | — | S |
| reference_wav | path | 空 | — | S |
| threads | int | 2 | ≥1 | S |
| provider | string | cpu | 非空 | S |
| queue_capacity | int | 8 | ≥1 | S |

### [bridge]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| socket | string | bridge.sock | 纯文件名 | S |
| subtitles_path | path | ~/.local/state/mozart/subtitles.jsonl | — | S |
| corrections_path | path | 空 | 可选显式纠错表；格式见下文 | S |
| translate_enabled | bool | true | — | R |
| speech_enabled | bool | true | — | R |
| llama_socket | string | llama.sock | 纯文件名 | S |
| llama_model | path | 空 | — | S |
| translate_queue_capacity | int | 32 | ≥1 | S |
| speech_queue_capacity | int | 16 | ≥1 | S |
| retry_max | int | 2 | 0..=2 | S |

`corrections_path` 指向 JSON 对象文件，格式为 `{"原文":"改为"}`，键和值均为非空中文字符串。文件条目覆盖同键内置词条；未配置或文件不存在时使用内置表。非法内容使桥启动失败。路径加载时展开开头的 `~`。

### [file]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| storage_dir | path | ~/.local/state/mozart/files/ | — | S |
| queue_capacity | int | 50 | ≥1 | R |
| storage_limit_bytes | int | 1048576000（1000 MiB） | >0 | R |
| denoise | bool | false | — | R |
| max_duration_seconds | int | 300 | ≥1 | R |
| max_upload_bytes | int | 104857600（100 MiB） | >0 | R |
| ffmpeg_path | string | ffmpeg | — | R |
| ffmpeg_timeout_ms | int | 120000 | >0 | R |

文件数据面复用 `[rvc].input_socket` / `[rvc].output_socket`（SPEC-03 状态门控交接），`[file]` 不再有独立 socket 键。

## 特殊机制说明

- **RVC 会话参数**（f0_method / pitch_shift / index_rate / rms_mix_rate / protect）：`config set rvc.<会话参数>` 写 TOML 持久基线。实时流在跑且本次写入的键属实时生效集（`pitch_shift`、`index_rate`、`f0_method` 与 `[rvc.realtime]` 四键）且值变化时，daemon 以 `StopStream`→`StartStream` 应用新参，短暂打断。仅改 `protect` / `rms_mix_rate` 更新基线与生效配置，不打断实时流。direct/空闲只更新生效配置。文件任务不中断：当前任务用旧参、下一任务用新参。触发细则与失败恢复见 [SPEC-04](SPEC/SPEC-04-daemon.md)。`protect` / `rms_mix_rate` 在实时链路不应用，仍只对文件链路生效；实时补应用与干净块边界 `SetParams` 见 [FUTURE.md](FUTURE.md)。
- **模型预设**（SPEC-11）：每个 RVC 模型目录 `presets/default.toml` 保存五个会话参数。模型切换时 daemon 载入该文件到 `[rvc]` 并应用；`preset save` 把当前 live 五参数写回当前模型，`preset use` 重新载入，`preset list` 显示预设值、live 值与 dirty。`presets/` 是资产树唯一可写目录；daemon 是唯一写者，原子写。命令细节见 [SPEC-11](SPEC/SPEC-11-preset.md)。
- **pre DSP 开关**经 `SetDsp`（0x0207，体含 denoise/gate/agc/limiter 四键）运行时生效：denoise 重新启用时重置 WOLA 流式状态、重新攒初始化帧，gate/agc/limiter 直接切换。
- **逻辑上限 vs 预分配环**：`[file].queue_capacity` 是入队时查的逻辑上限（未预分配）→ R；`[pre].capture_queue_frames` 与 asr/tts/bridge 的 `*_capacity` 是预分配环/队列 → S。
- **位置键**（storage_dir、subtitles_path）归 S：运行中改位置会使已有文件/队列分裂，需重启统一。
- **model_id** 是基线（S）：`config set rvc.model_id` 拒绝。运行时换模型走 `model use <id>`（0x0120），由它在 `LoadModel` 成功后把目标 id 原子写回 TOML 基线，失败整体回退；这是唯一写该键的路径。命令契约见 [SPEC-04](SPEC/SPEC-04-daemon.md)，预设边界见 [SPEC-11](SPEC/SPEC-11-preset.md)。
- **输出采样率**不是配置键：实时输出随模型原生采样率（32k/40k/48k），播放固定 48 kHz 并在 pre 播放边界重采样（SPEC-02、SPEC-03）。

## 运行时状态文件交互

运行时状态文件记录服务端运行时命令产生的覆盖，不记录 `config set` 的改动：`config set` 写 TOML 持久基线并即时应用，同时清掉该键已有覆盖。v1 状态文件只记录字幕开关（`bridge.translate_enabled` / `bridge.speech_enabled`），将来的无中断 RVC `SetParams` 也走这里。daemon 重启时在 TOML 基线上叠加覆盖恢复；活权威在各服务，启动恢复一次、运行时不对账。`config save` 把全部覆盖提升为基线后清空。详见 SPEC-04。