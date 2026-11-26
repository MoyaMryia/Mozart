# Mozart 配置规范（TOML schema）

状态：定稿中。最后更新：2026-11-26。

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
| startup_mode | enum | direct | rvc / direct / broadcast / manual | S |
| poll_interval_ms | int | 1000 | >0 | R |
| rpc_timeout_ms | int | 2000 | >0 | R |
| start_timeout_ms | int | 5000 | >0 | R |
| restart_backoff_ms | int | 500 | >0 | R |
| restart_backoff_max_ms | int | 10000 | ≥ restart_backoff_ms | R |
| restart_stable_ms | int | 30000 | >0 | R |

`startup_mode` 只在启动 daemon 时读取，`MOZART_STARTUP_MODE` 可覆盖。默认 `direct` 启动为托管直出模式；`rvc` 未配置模型时回退直出，配置了模型但起流失败时保持静音并报告错误。`manual` 保留旧路由和播报命令的独立控制。一次运行期间的模式请求不改启动配置。

### [process]

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| pre_bin / enhance_bin / rvc_bin / asr_bin / tts_bin / bridge_bin / llama_bin / web_bin | path | 各裸名 | 非空 | S |

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

### [enhance]（可选）

此段存在时，`pre.dsp.denoise` 选择 UL-UNAS，旧 WOLA 不叠加。省略此段保持旧默认。
增强仅作用于 RVC／ASR 共用的 16 kHz 支路；48 kHz 直通不经过模型。

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| model | path | 必填 | 非空；相对路径按配置目录解析，只读引用已有 ONNX | S |
| socket | string | enhance.sock | 纯文件名，不能与其他 socket 冲突 | S |
| audio_socket | string | enhance-audio.sock | 同上 | S |
| queue_frames | int | 8 | 1..=1024 | S |
| request_timeout_ms | int | 100 | 1..=1000；覆盖连接、发送与接收 | S |

修改本段已配置字段后，`restart pre` 或 `restart enhance` 会联动重建两者。
新增或移除本段要求重启 daemon。崩溃自动恢复仍使用生效快照，不提前吸收待重启键。
状态在 `status.enhance` 与 `status.pre.enhancement`；禁用时不调用网络，真实增强帧后清除当前故障。
文件任务原有的 `[file].denoise` 行为不变；增强进程的独立离线模式用于同一模型核的对拍。

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
| startup_delay_ms | int | 340 | 0..=15000，且为 20 的倍数 | R |
| skip_silence | bool | true | — | R |

跨键约束：`crossfade_ms + sola_search_ms` 恒为 60 ms（50 + 10），两键不可调，偏离在配置入口与 `StartStream` 入口拒绝；原因与解除条件见 [SPEC-03 实时几何与资产参数](SPEC/SPEC-03-rvc.md#实时几何与资产参数)。可调的是 `startup_delay_ms` 与 `skip_silence`：前者上限 15000 来自 C++ 输出环 16 s @48 kHz 留 1 s 余量，整除模数 20 ms 来自契约帧网格。定值与上限按 48 kHz 契约换算，各采样率的 max_out 同比缩放，故 ms 边界与采样率无关。违反在入口拒绝，不到推理层抛异常。

四键归 R 类：`config set` 修改 `skip_silence` 经 `SetParams`（`0x0308`）在块边界无中断生效，`startup_delay_ms` 走 Stop→Start（见下文特殊机制说明）；overlap 两键只接受定值，实际可变的只有 `startup_delay_ms` 与 `skip_silence`。

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
| glossary_path | path | 空 | 可选翻译术语表；非空路径，格式见下文 | S |
| translate_enabled | bool | true | — | R |
| speech_enabled | bool | false | — | R |
| llama_socket | string | llama.sock | 纯文件名 | S |
| llama_model | path | 空 | 旧单模型简写；加载时展开开头的 `~` | S |
| llama_model_id | string | 空 | 非空，须匹配 `[[bridge.models]]` 的 `id`；优先于 `llama_model` | S |
| models | array | 空数组 | 翻译模型列表，见 `[[bridge.models]]` | S |
| translate_queue_capacity | int | 32 | ≥1 | S |
| speech_queue_capacity | int | 16 | ≥1 | S |
| retry_max | int | 2 | 0..=2 | S |

`corrections_path` 指向 JSON 对象文件，格式为 `{"原文":"改为"}`，键和值均为非空中文字符串。文件条目覆盖同键内置词条；未配置或文件不存在时使用内置表。非法内容使桥启动失败。路径加载时展开开头的 `~`。

`glossary_path` 指向 JSON 对象文件，格式为 `{"中文术语":"英文译名"}`，键和值均为非空字符串。未配置或文件不存在时使用空表；非法内容使桥启动失败。中文键并入拼音同音纠错索引，命中词条作为翻译提示对注入支持术语的模型模板。路径加载时展开开头的 `~`。

翻译模型解析与回退规则：

- 指定 `llama_model_id` 时，按 `id` 从 `[[bridge.models]]` 选取对应条目；为空或未命中时入口报错。
- 未指定 `llama_model_id` 且 `llama_model` 非空时，按旧单模型默认参数构造配置（`prompt_profile = "qwen_chat"`、`ctx_size = 2048`、`cache_ram_mb = 128`、`batch_size = 256`、`ubatch_size = 128`、`gpu_layers = 99`、`threads = 2`、`max_tokens = 256`、`temperature = 0.0`、`enable_thinking = false`、`context_window = 0`）。
- 未指定 `llama_model_id` 且 `llama_model` 为空、但 `[[bridge.models]]` 非空时，入口直接报错，不默认选取首项；两者均空时不启用模型覆盖。

### [[bridge.models]]

可选翻译模型条目列表，由 `daemon` 生成 `llama-server` 启动参数、由 `bridge` 按 `prompt_profile` 组装翻译请求。全表字段均为重启类（S）。

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| id | string | 必填 | 非空，列表内唯一 | S |
| path | path | 必填 | 非空；加载时展开开头的 `~` | S |
| prompt_profile | enum | qwen_chat | `qwen_chat` / `hy_mt` / `custom` | S |
| ctx_size | int | 1024 | ≥256 | S |
| cache_ram_mb | int | 64 | ≥0 | S |
| batch_size | int | 256 | ≥1 | S |
| ubatch_size | int | 128 | 1..=batch_size | S |
| gpu_layers | int | 99 | ≥0 | S |
| threads | int | 2 | ≥1 | S |
| max_tokens | int | 256 | 16..=ctx_size | S |
| temperature | float | 0.0 | 0.0..=2.0，有限数 | S |
| repeat_penalty | float | 空 | 可选；(0.0, 2.0]，有限数 | S |
| enable_thinking | bool | 空 | 可选；`qwen_chat` 缺省发 `false`，`hy_mt` 禁止设置 | S |
| context_window | int | 0 | 0..=8（注入最近若干条源文）；`qwen_chat` 须为 0 | S |
| system_prompt | string | 空 | 可选；`custom` 或覆盖 `qwen_chat` 时使用，`hy_mt` 禁止设置 | S |
| user_template | string | 空 | `custom` 必填且须含 `{text}`（可选 `{context}`、`{glossary}`）；非 `custom` 禁止设置 | S |

提示词策略说明：

- `qwen_chat`：发送 `[system, user]` 与 `chat_template_kwargs.enable_thinking`。
- `hy_mt`：仅发送单条 `user` 消息，不发 `system` 与 `chat_template_kwargs`，按是否命中术语和上文组装指令模板。
- `custom`：按配置的 `system_prompt`（可选）与 `user_template` 替换 `{text}`、`{context}`、`{glossary}`。

```toml
[bridge]
llama_model_id = "hymt"
glossary_path = "~/.config/mozart/glossary.json"

[[bridge.models]]
id = "hymt"
path = "<资产根>/models/llm/<模型文件>.gguf"
prompt_profile = "hy_mt"
ctx_size = 1024
cache_ram_mb = 64
max_tokens = 256
repeat_penalty = 1.05
context_window = 2
```

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

### [web]（可选）

省略此段时，daemon 不拉起 `mozart-web-adapter`（`processes.web` 保持 `stopped`）；配置此段后，由 daemon 统一监管适配器进程，日志落 `run_dir/logs/web.log`。适配器与 daemon 读取同一 TOML，采用同一配置文件选择顺序，HTTP 请求不能改变配置来源。

| 键 | 类型 | 默认 | 范围 | 类别 |
| --- | --- | --- | --- | --- |
| listen | socket address | 0.0.0.0:8080 | 有效 IP 与端口 | S |
| static_dir | path | web/dist | 前端静态资源目录 | S |
| upload_dir | path | ~/.local/state/mozart/web-uploads | 适配器私有临时目录，权限 0700 | S |
| allowed_origins | array<string> | ["http://localhost:8080", "http://127.0.0.1:8080", "http://[::1]:8080"] | 无路径的 HTTP origin，不含通配符 | S |
| max_concurrent_uploads | int | 2 | ≥1 | S |
| upload_timeout_ms | int | 120000 | >0，接收一次 multipart 正文的总时限 | S |
| sse_heartbeat_ms | int | 15000 | >0 | S |
| sse_buffer_bytes | int | 1048576 | >0，每客户端待发送事件的编码字节总量 | S |

这些 S 类键通过 `restart web` 重启适配器生效（不中断其他服务）；新增或移除 `[web]` 段需重启 daemon。
相对路径按 TOML 所在目录解析；使用内置默认且无配置文件时按工作目录解析，支持开头的 `~`。
部署时向 `allowed_origins` 加入设备热点和局域网的实际访问地址。
文件字节限额只引用 daemon 的生效 `file.max_upload_bytes`，不在 web 段另设限额。
上传、SSE 与访问边界见 [SPEC-06](SPEC/SPEC-06-web.md)。

## 特殊机制说明

- **RVC 会话参数**（f0_method / pitch_shift / index_rate / rms_mix_rate / protect）：`config set rvc.<会话参数>` 与 `preset use` 写 TOML 持久基线。实时流在跑时，`pitch_shift` / `index_rate` / `f0_method` 与 `[rvc.realtime].skip_silence` 变化由 daemon 下发 `SetParams`（`0x0308`）在下一推理块边界无中断生效，失败时保留在跑旧参、生效配置保留旧值、TOML 保留新基线；`startup_delay_ms` 决定启动缓冲，变化仍走 `StopStream`→`StartStream`（失败用旧参恢复）。仅改 `protect` / `rms_mix_rate` 或参数值未变化只更新 TOML 基线与生效配置，不下发 `rvc` 命令。direct/空闲只更新生效配置。文件任务不中断：当前任务用旧参、下一任务用新参。`status.rvc_params` 表示当前生效配置；存在待生效基线或 rvc 不可达时，可能不等于正在跑的流参数。`protect` / `rms_mix_rate` 已在实时链路推理内应用（启动流时随会话参数生效；在线改值暂仅更新基线与生效配置、不触发重流）。编排细节见 [SPEC-04](SPEC/SPEC-04-daemon.md)。
- **模型预设**（SPEC-11）：每个 RVC 模型目录 `presets/default.toml` 保存五个会话参数。模型切换时 daemon 载入该文件到 `[rvc]` 并应用；`preset save` 把当前 live 五参数写回当前模型，`preset use` 重新载入，`preset list` 显示预设值、live 值与 dirty。`presets/` 是资产树唯一可写目录；daemon 是唯一写者，原子写。命令细节见 [SPEC-11](SPEC/SPEC-11-preset.md)。
- **pre DSP 开关**经 `SetDsp`（0x0207，体含 denoise/gate/agc/limiter 四键）运行时生效：denoise 重新启用时重置 WOLA 流式状态、重新攒初始化帧，gate/agc/limiter 直接切换。
- **逻辑上限 vs 预分配环**：`[file].queue_capacity` 是入队时查的逻辑上限（未预分配）→ R；`[pre].capture_queue_frames` 与 asr/tts/bridge 的 `*_capacity` 是预分配环/队列 → S。
- **位置键**（storage_dir、subtitles_path）归 S：运行中改位置会使已有文件/队列分裂，需重启统一。
- **model_id** 是基线（S）：`config set rvc.model_id` 拒绝。运行时换模型走 `model use <id>`（0x0120），由它在 `LoadModel` 成功后把目标 id 原子写回 TOML 基线，失败整体回退；这是唯一写该键的路径。命令契约见 [SPEC-04](SPEC/SPEC-04-daemon.md)，预设边界见 [SPEC-11](SPEC/SPEC-11-preset.md)。
- **输出采样率**不是配置键：实时输出随模型原生采样率（32k/40k/48k），播放固定 48 kHz 并在 pre 播放边界重采样（SPEC-02、SPEC-03）。

## 运行时状态文件交互

运行时状态文件记录服务端运行时命令产生的覆盖，不记录 `config set` 的改动：`config set` 写 TOML 持久基线并即时应用，同时清掉该键已有覆盖。当前状态文件只记录字幕开关（`bridge.translate_enabled` / `bridge.speech_enabled`）；`config set rvc.*` 与 `preset use` 直接写 TOML 基线并按变更键走 `SetParams` 或 `StopStream`→`StartStream`。daemon 重启时在 TOML 基线上叠加覆盖恢复；活权威在各服务，启动恢复一次、运行时不对账。`config save` 把全部覆盖提升为基线后清空。详见 SPEC-04。