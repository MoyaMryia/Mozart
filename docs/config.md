# Mozart 配置规范（TOML schema）

状态：定稿中。最后更新：2026-11-13。

本文件是 TOML 配置的唯一 schema 依据：段、键、类型、默认、范围、运行时/重启分类。配置模型（TOML 基线 + 运行时状态文件、`config set`/`config save`、`restart_required` 契约）见 [SPEC-04](SPEC/SPEC-04-daemon.md)，本文件不复述。解析与校验实现于 `core/src/config.rs`，须与本文件一致。

## 分类原则

按机制 + 切换成本，**能即时生效就归运行时**：

- **运行时（R）**：键值每次操作才读（阈值/上限/开关/超时），或 owning 服务有运行中应用的控制命令。`config set` 写 TOML 基线 + 清该键状态覆盖 + 立即下发生效。
- **重启（S）**：键值启动时固化进长活结构（打开的设备、绑定的 socket、预分配的环、加载的模型、spawn 的二进制、存储目录）。`config set` 只写 TOML、不动运行进程，返回 `restart_required` + `affected`。

## 配置来源与优先级

`--config` > `MOZART_CONFIG` > `./mozart.toml` > 内置默认。环境变量覆盖：`MOZART_RUN_DIR`、`MOZART_*_BIN`、`MOZART_POLL_INTERVAL_MS`。详见 SPEC-04。

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
LD_LIBRARY_PATH = "/home/user/mozart/assets/lib:/home/user/mozart/assets/llama"
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
| playback_device | string | default | 非空、无 NUL | S |
| capture_queue_frames | int | 8 | 1..=1024 | S |

### [pre.dsp]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| denoise | bool | true | — | R |
| gate | bool | false | — | R |
| agc | bool | false | — | R |
| limiter | bool | false | — | R |

### [[models]]

顶层数组，每项声明一个可用模型。`LoadModel`、`file submit --model`、`model list` 引用此注册表。

| 键 | 类型 | 必需 | 说明 |
| --- | --- | --- | --- |
| id | string | 是 | 模型标识，同一 type 内唯一 |
| type | string | 是 | rvc / asr / tts / llm |
| path | string | 是 | 模型目录绝对路径（rvc/asr/tts）或模型文件路径（llm） |

```toml
[[models]]
id = "qiqi-zh"
type = "rvc"
path = "/home/user/mozart/assets/models/rvc/qiqi-zh-run"

[[models]]
id = "zipformer-zh"
type = "asr"
path = "/home/user/mozart/assets/models/asr/zipformer-zh-14M"
```

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
| translate_enabled | bool | true | — | R |
| speech_enabled | bool | true | — | R |
| llama_socket | string | llama.sock | 纯文件名 | S |
| llama_model | path | 空 | — | S |
| translate_queue_capacity | int | 32 | ≥1 | S |
| speech_queue_capacity | int | 16 | ≥1 | S |
| retry_max | int | 2 | 0..=2 | S |

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

- **RVC 会话参数**（f0_method / pitch_shift / index_rate / rms_mix_rate / protect）运行时生效经 `StopStream`→`StartStream` 带新参：实时在跑立刻用新参（短暂打断）；文件任务在跑则当前任务用旧参跑完、新参下一任务生效，不中途废弃。干净的块边界 `SetParams` 见 FUTURE。
- **pre DSP 开关**经新增 `SetDsp` 命令运行时生效；denoise 运行中切换需处理 WOLA 滤波状态避免伪影。
- **逻辑上限 vs 预分配环**：`[file].queue_capacity` 是入队时查的逻辑上限（未预分配）→ R；`[pre].capture_queue_frames` 与 asr/tts/bridge 的 `*_capacity` 是预分配环/队列 → S。
- **位置键**（storage_dir、subtitles_path）归 S：运行中改位置会使已有文件/队列分裂，需重启统一。
- **model_id** 是基线（S）；运行时换模型走控制命令，不经 `config set`。

## 运行时状态文件交互

运行时键的改动记入 daemon 运行时状态文件（覆盖快照），重启时 TOML 基线叠加恢复；活权威在各服务，启动恢复一次、运行时不对账。详见 SPEC-04。