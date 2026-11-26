# web：控制台与 HTTP API

状态：v1 首批 HTTP/SSE 接口已实现，完整命令面和设备验收待完成。最后更新：2026-11-26。
`[x]` 条目详情已于 2026-11-24 移入 [archive/SPEC-06-web.md](archive/SPEC-06-web.md)，本文件保留契约、关键决策摘要与活跃待办。

适配器作为独立进程连接 daemon 控制 socket；本机入口仍包括
`mozart-ctl`，见 [SPEC-04](SPEC-04-daemon.md#本机程序集成)。
待办标记沿用 [SPEC.md](SPEC.md)。前端实现与契约夹具验证记录见
[新控制台首轮实现](../../reports/web-console-20261116/RESULTS.md)。

## 一、边界与部署

- 前端使用 TypeScript/Vue，放在 `web/`。
- Rust crate `mozart-web-adapter` 位于 `web-adapter/`，二进制同名。
- 适配器同时托管静态文件、REST API、服务器发送事件（SSE）和文件上传下载。
- 适配器复用 `mozart-core` 的控制协议，连接 daemon；daemon 自身不起 HTTP。
- 生命周期与控制权分离：进程监管按方案 1 归 `daemon`（与业务子进程同级，见 [SPEC-04](SPEC-04-daemon.md)）；控制面角色与 `mozart-ctl` 同级，一切控制读写均经 `daemon.sock` 由 `daemon` 仲裁并下发，不直连 `pre`、`enhance`、`rvc`、`asr`、`bridge`、`tts` 控制 socket（仅字幕 SSE 只读 `subtitles.jsonl` 为例外，清空等写动作仍须经 `daemon`）。
- 音频帧继续走已有数据面；本文不定义网络音频上传流、WebSocket 或远程麦克风协议。
- llama-server 的 `/v1/chat/completions` 是内部翻译接口，不转发为公共端点。

```text
浏览器／HTTP 客户端
  └─ mozart-web-adapter
       ├─ Vue 静态资源
       ├─ REST → daemon.sock → 各服务控制面
       ├─ 状态 SSE ← daemon GetStatus
       ├─ 字幕 SSE ← subtitles.jsonl
       └─ 临时上传文件 → daemon 文件队列 → WAV 下载

CLI／本机程序集成 ─────────→ daemon.sock
音频采集／推理／播放 ─────→ 独立音频数据面
```

部署与运行方式已按方案 1 收口，由 `daemon` 按同一 `mozart.toml` 统一监管适配器进程（配置 `[web]` 时拉起，`Role::Web`，见 [SPEC-04](SPEC-04-daemon.md)）。
网页启停直接调用 daemon 业务子服务组启停命令，停机期间 `daemon` 与适配器保持在线、页面仍可访问；整机完全关停保留给 `mozart-ctl down`。
默认接入方式为设备热点，保留局域网访问；热点配置归 [SPEC-09](SPEC-09-pipeline.md)。
监听地址与静态资源位置等新增配置见 [config.md 的 web 段](../config.md#web)。
适配器与 daemon 使用同一运行用户、同一 TOML 和同一运行目录，不放宽 socket 目录权限。
进程支持 `--config` 与 `--print-config`（已移除 `--ctl-bin` 与 `--ready-file`）；main 使用 `Transport` 连接句柄关闭缓冲溢出的 TCP 客户端。
默认监听高端口 `0.0.0.0:8080`，不以 root 身份运行整套系统；手动配置低端口时的部署权限见 [SETUP](../SETUP.md#部署行为)。
低端口是已知的孤儿保护缺口，不做专门适配：授予 `cap_net_bind_service` 后内核按 `AT_SECURE` 处理 `execve`，会清掉 `PDEATHSIG`，daemon 被 `SIGKILL` 时适配器可能残留。正常 `mozart-ctl down` 与 `SIGTERM` 仍由监管终止适配器；默认 `8080` 不受影响。

## 二、HTTP 通用契约

### 请求与兼容性

- API 前缀固定为 `/api/v1`。不兼容的 HTTP 变更使用新版本前缀。
- JSON 使用 UTF-8，字段名沿用 core schema，不新增一套前端状态枚举。
- JSON 写请求使用 `Content-Type: application/json`；无参数动作发送 `{}`。
- 请求对象拒绝未知字段。路径参数是 URL 解码后单个非空标识，不解释为文件路径。
- JSON 正文与 multipart 文本字段的合计大小上限引用 `MOZART_CTL_MAX_BODY_BYTES`，不另设副本常量。
- 业务参数、文件类型与准入规则由 daemon 校验；适配器只校验 HTTP 表达与传输边界。
- GET 不改变业务状态。写请求不自动重试，文件提交不承诺幂等。
- API 路径不存在返回 404；方法不符返回 405 和 `Allow`。不得回退为 Vue 页面。
- 静态文件只从配置的资源目录读取，拒绝目录穿越；仅页面导航使用 SPA 回退。

### 响应

除 SSE 和成功的文件下载外，响应使用 `application/json`，保留 core 的四字段信封：

```json
{"status":"ok","code":"Ok","reason":"","data":{}}
```

失败使用 `status="error"`、具体 `code`、非空 `reason` 和 `data=null`。
直连 daemon 的请求原样保留其响应体；适配器只映射 HTTP 状态码。
HTTP 自身失败也使用此信封，但其错误码不进入 core 控制命令定义。
API 与 SSE 响应设置 `Cache-Control: no-store`。

| 来源／错误码 | HTTP 状态 | 说明 |
| --- | --- | --- |
| `Ok` | 200 | 文件提交例外，见路由表 |
| `InvalidArgument` | 400 | 包括 daemon 当前的未知 job_id、已结束任务取消等拒绝 |
| `NotLoaded` / `NotReady` / `Busy` | 409 | 保留 daemon 原因；不把 GPU 等待意图自行改为拒绝 |
| `Unsupported` / `UnknownCommand` | 501 | 对端不支持；前端显示失败，不模拟成功 |
| `InferenceFailed` / `Internal` | 500 | 业务执行或内部失败 |
| 未识别的 daemon 错误码 | 502 | 原样保留信封，视为协议版本不匹配 |
| `BadRequest` | 400 | HTTP 解析、参数位置或 JSON 结构错误 |
| `NotFound` / `MethodNotAllowed` | 404 / 405 | HTTP 路由层错误 |
| `PayloadTooLarge` / `UnsupportedMediaType` | 413 / 415 | 传输大小或 Content-Type 不符 |
| `Forbidden` | 403 | 不符合下文同源规则 |
| `UploadTimeout` / `UploadBusy` | 408 / 429 | 上传超时或上传并发额度已满 |
| `SourceUnavailable` | 503 | 字幕源读取或记录解析失败，不能继续推流 |
| `UpstreamUnavailable` | 503 | daemon 连接不存在、被拒绝或权限不足 |
| `GatewayTimeout` | 504 | 控制请求超过截止时间 |
| `BadGateway` | 502 | daemon 响应损坏、请求标识不符或发送后连接中断 |
| `ResultUnavailable` | 409 | 下载打开文件前，结果已被驱逐 |

收到 socket 响应不等于任务完成。异步操作通过后续状态确认。
写请求发送后超时或断连时，`reason` 必须说明结果未知；不得宣称操作未发生。
调用方先查询状态或任务列表，再决定是否重新提交。
RPC 截止时间复用 CLI 的普通请求、配置写入及启动停止预算，不另设更短的 HTTP 截止时间。
复合 HTTP 请求的总截止时间覆盖全部串行控制步骤，不把单次 RPC 预算当成整个流程预算。

## 三、REST 路由

表内路径均相对 `/api/v1`。未注明的成功响应使用 200，`data` 沿用对应控制命令。
控制命令号以 `core/include/mozart/ctl.h` 为准，请求与响应字段以
`core/src/ctl/schema.rs` 为准。业务语义见 [SPEC-04](SPEC-04-daemon.md)。

| 方法与路径 | 请求 | 下游操作／成功 data |
| --- | --- | --- |
| `GET /health` | 无 | 适配器自身状态与上传保留原因，见下文；不表示推理服务健康 |
| `POST /system/up` | `{}` | `DAEMON_SERVICES_START`（`0x0123`）；`{"state":"running"}` 或 `already-running` |
| `POST /system/down` | `{}` | `DAEMON_SERVICES_STOP`（`0x0124`）；`{"state":"stopped"}` 或 `not-running` |
| `GET /status` | 无 | `DAEMON_GET_STATUS`，整机快照 |
| `PUT /route` | `{"route":"direct"}` 或 `{"route":"rvc"}` | `DAEMON_ROUTE_SET`；退出托管输出模式 |
| `PUT /mode` | `{"mode":"direct"}`、`{"mode":"rvc"}` 或 `{"mode":"broadcast"}` | `DAEMON_OUTPUT_MODE_SET`；应答表示受理，按 `GET /status` 的 `output_mode` 查询转换 |
| `PUT /mute` | `{"mute":true}` | `DAEMON_MUTE_SET` |
| `PUT /translate` | `{"enabled":true}` | `DAEMON_TRANSLATE_SET` |
| `PUT /speech` | `{"enabled":true}` | `DAEMON_SPEECH_SET` |
| `POST /speech/stop` | `{}` | `DAEMON_SPEECH_STOP` |
| `POST /speech/clear` | `{}` | `DAEMON_SPEECH_CLEAR`；清空待播报队列并截断在途合成，保持 `speech_enabled` 不变与托管模式 |
| `GET /models` | 无 | `DAEMON_MODEL_LIST`，`{"models":[...]}` |
| `GET /devices` | 可选 `all=true/false`，默认 false | `DAEMON_DEVICE_LIST`，`{"devices":[...]}` |
| `GET /config` | 可选查询参数 `key` | `DAEMON_CONFIG_GET`，`{"value":...}`；读取 TOML 基线 |
| `PUT /config` | `{"key":"rvc.pitch_shift","value":"3"}` | `DAEMON_CONFIG_SET`；value 必须是字符串 |
| `POST /config/save` | `{}` | `DAEMON_CONFIG_SAVE` |
| `POST /services/{service}/restart` | `{}` | `DAEMON_RESTART`，由路径构造 `{"service":...}` |
| `POST /files` | multipart，见下节 | `DAEMON_FILE_SUBMIT`；成功返回 202 与 `job_id/state/queue_position` |
| `GET /files` | 无 | `DAEMON_FILE_LIST`，`{"jobs":[...],"paused":...}` |
| `GET /files/{job_id}` | 无 | `DAEMON_FILE_STATUS`，单个 FileJob |
| `POST /files/{job_id}/cancel` | `{}` | `DAEMON_FILE_CANCEL` |
| `POST /files/pause` | `{}` | `DAEMON_FILE_PAUSE` |
| `POST /files/resume` | `{}` | `DAEMON_FILE_RESUME` |
| `GET /files/{job_id}/result` | 无 | `DAEMON_FILE_FETCH` 后流式返回 WAV，见下节 |

`health.data` 固定包含 `adapter="running"` 和
`uploads={"active":0,"retained":0,"reasons":[]}`。active 是正在接收或等待提交应答的数量，
retained 是提交结果未知而保留的数量；reasons 列出保留原因，不含本机路径。

`service` 取值复用 `RestartRequest` 的服务范围。未知查询参数、重复单值参数和非法布尔值返回 `BadRequest`。
`models` 不按 `exists` 过滤；前端可按该字段筛选，但不得把 `exists=true` 显示为推理已就绪。
`config set` 的支持键和生效成本归 daemon；不得因为配置表列了某键，就假定写回已经接通。
设备键写入成功后，前端按 `restart_required`/`affected` 自动调用 `POST /services/{service}/restart` 使基线生效；其余重启类提示与 `status.pending_restart` 原样展示。
设置路由走 `PUT /api/v1/mode`（`direct`/`rvc`/`broadcast`），前端按 `output_mode.requested`、`output_mode.applied`、`output_mode.phase` 与 `output_mode.last_error` 分开展示请求意图、实际模式、过渡阶段及故障原因，不把等待切换显示为已经切换。

### 业务子服务启停

`POST /system/up` 与 `POST /system/down` 通过 `daemon.sock` 透传 `DAEMON_SERVICES_START` / `DAEMON_SERVICES_STOP`（经 `core::ctl::request_timeout_ms` 共用扩展预算），不再由适配器拉起 `mozart-ctl` 子进程。
操作仅启停音频与 AI 业务子服务组，`daemon` 与 `mozart-web-adapter` 保持在线；整机停机走 `mozart-ctl down`。
成功沿用 `{"state":"running"|"already-running"}` 与 `{"state":"stopped"|"not-running"}` 幂等语义；文件任务正在执行或 GPU 归还中时停止返回 `Busy`（409），排队任务保留不动、恢复服务后按原仲裁继续。停服期间新的 `POST /files` 返回 `NotReady`（409）。`system/up` 成功只表示业务服务启动流程已触发，各服务就绪仍以 `/status` 为准。

## 四、文件上传与下载

### 上传

`POST /files` 使用 `multipart/form-data`，包含一个必需的二进制字段 `file`，
以及一个可选文本字段 `model_id`。重复字段、未知字段或空文件名返回 `BadRequest`。
模型参数无论出现在文件前后，都必须在提交 daemon 前收齐。

1. 从 filename 取得显示名；拒绝 NUL、CR/LF、路径分隔符及 `.`、`..`。
2. 在配置的上传目录创建随机独占子目录，以显示名创建临时文件。
3. 流式写盘，累计文件部分的实际字节数；不把整段音频读进内存。
4. 文件字节上限读取 daemon 生效配置的 `file.max_upload_bytes`，不使用编译期副本。
5. 完整接收并关闭文件后，将设备本地路径与可选 `model_id` 交给 `DAEMON_FILE_SUBMIT`。
6. daemon 复制并受理后返回 202，同时设置 `Location: /api/v1/files/{job_id}`。

获取当前生效上传上限的控制契约见 [SPEC-04 的程序集成补充](SPEC-04-daemon.md#程序集成待实现补充)。
该补充实现前，不用 `config get` 的 TOML 基线冒充当前生效配置。
Content-Length 包含 multipart 边界，不能直接当成文件字节数；chunked 上传同样按实际文件字节计限。
文件扩展名、解码时长、模型加载与任务错误沿用 daemon 规则，不在适配器复制业务校验。
文件受理不表示模型可加载；执行失败经 FileJob 的 `state/error` 返回。

上传目录由适配器实例锁独占。临时文件的回收按以下所有权规则执行：

- 尚未向 daemon 发送 submit：接收中断、超时或失败后直接删除。
- 收到与请求关联的明确成功或拒绝应答：daemon 已完成读取源文件，删除临时目录。
- 已发送但应答未知：保留源文件和并发额度，不把请求超时或浏览器断连当成读取结束。
- 未知请求仅在收到明确应答或确认处理该请求的 daemon 实例退出后回收；不重发 submit。

发送前将源文件信息与控制连接对端的进程身份原子记录在上传目录，供重启恢复。
进程身份必须能区分 PID 复用，例如 boot_id、PID 与启动时刻，不能只存 PID。
适配器重启不代表 daemon 已停止读取；重启后先恢复这些记录，再清理可确认安全的目录。
保留数量和原因经 `/health` 查询；`active + retained` 共用 web 配置中的上传并发额度。
daemon 的受管源副本和结果归 daemon 清理，适配器不修改队列存档。
上传接收截止时间见 [web 配置](../config.md#web)，额度在读取正文前取得。

### 下载

适配器先在同一控制连接上查询 `GetStatus.data.api_capabilities`，
确认包含 `file_fetch_safe_errors_v1`；字段或能力缺失时返回 `Unsupported`，不调用 FileFetch。
该能力的提供方前置修改见 SPEC-04，不能用提交号或统一的程序版本字符串代替能力确认。
适配器只接受 `job_id`，向 daemon 发送 `FileFetchRequest`，不接受 `path` 或 `output`。
成功后打开 daemon 返回的 `output_path`，使用已打开的文件句柄流式发送：

- `Content-Type: audio/wav`。
- `Content-Disposition: attachment`，文件名取 daemon 返回的 `name`，安全编码头字段。
- `Content-Length` 取打开文件的大小；v1 返回完整文件，不实现 Range／断点续传。

不把绝对路径作为下载 JSON 返回，也不提供任意路径读取接口。
返回头发送前文件已消失，返回 `ResultUnavailable`；打开后被驱逐不影响已打开句柄。
响应开始后发生读错误，只终止连接并记录原因，不在 WAV 后拼接 JSON 错误。
下载断开不取消转换任务。

## 五、SSE 推送

| 路径 | 事件 | data |
| --- | --- | --- |
| `GET /api/v1/events/status` | `status` | 与 `GetStatus.data` 相同的整机快照 |
| 同上 | `error` | 四字段错误信封 |
| `GET /api/v1/events/subtitles` | `subtitle` | 一条完整字幕记录，沿用 JSONL 字段 |
| 同上 | `reset` | `{"generation":"..."}`，要求清空旧字幕读模型 |
| 同上 | `error` | 四字段错误信封 |

响应为 `text/event-stream`，及时 flush，关闭代理缓冲。
心跳使用 SSE 注释，不占用业务事件 ID；间隔与每客户端缓冲上限见 web 配置。
慢客户端超过缓冲上限时记录原因并断开，由客户端重连；不得阻塞其他客户端、字幕写入或音频线程。
建立流前的参数错误用普通 HTTP 错误；流建立后的失败发 `error` 再关闭连接。

### 状态流

建立连接立即获取并发送当前快照，再按 daemon 的生效 `poll_interval_ms` 共享轮询、广播。
间隔读取 `GetStatus.data.daemon.poll_interval_ms`，新增字段要求见 SPEC-04；缺失时返回 `Unsupported`。
不为每个浏览器额外轮询各个服务，不维护第二份业务权威。
查询失败发送带原因的 `error`，不把旧快照当成新状态发送。
状态事件不带 `id`；重连重新获取当前快照，不补发历史状态。

### 字幕流与恢复

适配器用 inotify 监听字幕文件及父目录，按字节偏移读取完整 JSONL 行。
inotify 只是唤醒信号。通知溢出、watch 重建或读取状态无法确认时，更新 generation 并从头回放。
不能仅凭 inode 未变或文件大小不小于旧偏移，就认定没有发生截断。
同 inode 更新继续旧游标前，使用流式摘要核验已读前缀；前缀改变则更新 generation 并从头回放。
核验或读取期间发现源被改写，放弃该批读取并重建恢复状态；核验在独立读取任务中完成。
文件尚未创建时等待并发送心跳；其他打开、读取或 JSON 解析错误必须报告，不能跳过坏行。
末尾未换行的数据先缓存，写完换行后才发布。

字幕流的 `id` 格式为 `<generation>:<next_offset>`：

- `generation` 是适配器生成的不透明文件代号；实例启动、文件替换或截断时更新。
- `next_offset` 是该完整行之后的字节位置，使用十进制文本。
- 无恢复游标时先发 `reset`，然后从文件头流式回放，再接续追加内容。
- 恢复游标取 `Last-Event-ID`；首次重建页面可用查询参数 `cursor`，两者同时存在时以前者为准。
- 同代号游标从给定行边界继续。非法格式、越界或落在行中间返回 `BadRequest`。
- 代号变化时发 `reset`，再从新文件头回放，不把旧偏移用于新文件。
- 回放与实时追加的交接不得漏行；允许重传，客户端按记录身份和修订号去重。

页面重建只有在游标与对应字幕读模型一并恢复时才携带 `cursor`。
两者作为同一快照原子持久化；读模型丢失或不一致时，无游标重新回放。
收到 `reset` 时清空读模型与持久游标，随后按新事件重建。

`reset` 不带 SSE `id`。客户端以 `utterance_id` 标识一条字幕，
仅用更大的 `revision` 更新已有记录；不把 `seq` 当跨 ASR 重启的全局编号。
记录字段和修订语义归 [SPEC-05](SPEC-05-language.md)，适配器不改写译文或重新触发翻译。

## 六、访问边界

v1 沿用本地设备／自有热点无认证的决定，不提供公网部署入口或 TLS 终止。
前端与 API 同源，不设置通配 CORS。部署通过 web 配置声明 `allowed_origins`。
请求 Host 必须匹配该列表中某个 origin 的 authority，按默认端口归一化比较。
携带 Origin 时还必须同时匹配该列表及本次服务 origin；不能仅信任客户端提供的 Host。
拒绝浏览器 `Sec-Fetch-Site: cross-site` 请求；无 Origin 的本机或局域网程序仍可调用。
这些检查不构成用户鉴权，网络访问控制由部署负责。
配置写入、关停与取消必须使用写方法，不能通过链接或 GET 触发。

## 已确认决策（摘要）

> 全文条目与证据见 [archive/SPEC-06-web.md](archive/SPEC-06-web.md)。与归档冲突时以归档原文为准并回来修正本节。

- 默认监听 `0.0.0.0:8080`，默认 `allowed_origins` 同步带 `:8080`；手动低端口保留 `cap_net_bind_service` 兼容路径，孤儿保护缺口见第一节。
- 上传目录首次创建并规范叶目录为 `0700`；不安全的现有目录拒绝且不修改其权限，启动报错打印具体路径与实际权限/原因。
- 适配器归 `daemon` 统一监管（方案 1）：移除独立 `tools/web-service.sh` 与 `--ctl-bin`/`--ready-file` 及子进程 CLI 调起，启停透传 `DAEMON_SERVICES_START`/`DAEMON_SERVICES_STOP`。
- Rust 检查显式构建 daemon、ctl 和协议桩；适配器集成测试取 Cargo 正式产物路径，不依据测试可执行文件所在目录。
- 部署回滚恢复原先不存在的源码目录；Web 服务状态查询不修改运行目录权限。
- Web 后端与部署改动已迁移到当前主线，保留增强和 ASR 修复；资源读取失败后的写入限制与未知结果确认已修复并补前端回归，未知写入不重试。
- `web/` Vue 工程与 Rust 适配器已创建，接入共用配置、新 REST/SSE 客户端和本机检查入口；设备构建部署入口已接入，标准脚本整链的当前阻塞与已测范围见归档所引报告。
- 系统面板显示增强进程与工作状态，支持重启增强服务。
- 生效上传上限与轮询间隔查询已按 SPEC-04 补齐并接入对应 HTTP 路由。
- 验收通过：适配器存活期间 down → 页面可达 → up → 状态恢复（真实浏览器联调）；未声明下载安全能力的旧 daemon 下载明确拒绝且不透出本机路径；未知路由、方法、正文类型、同源限制、目录穿越与下载文件名编码的 HTTP 边界测试通过。

## 七、实现与验收待办

- [ ] 接口与前端收口：保持适配器为 `daemon.sock` 瘦透传层，补齐已有命令映射（适配器与前端补齐 `DAEMON_DEVICE_USE`）；适配器缓存已加载配置，不再每次 RPC 重读磁盘 TOML；前端改为按需刷新，避免每次写入后并发重拉 ALSA 设备列表与模型列表。
- [x] 控制台三态互斥路由与条件播报卡片重构：前端路由切换接入 `PUT /api/v1/mode`（`direct`／`rvc`／`broadcast`），移除左侧独立「翻译播报」卡片并在路由卡片内底部提供独立「翻译」开关按钮（`PUT /api/v1/translate`）；仅当播报启用时在右侧路由卡片下方条件展开「播报设置」卡片并提供 `POST /api/v1/speech/clear`「清除播报队列」按钮（不展示未实现功能占位文案）。
- [ ] 前端写锁与 SSE 断连加固：解耦 HTTP 状态主动查询版本号与 SSE 推送版本号，消除 `statusReadFresh` 被 SSE 抢跑打断的竞态与刷新闪烁；幂等写操作在拉回最新状态后自动解除未知结果锁，仅不可幂等的文件提交保留人工确认；前端 SSE 改用指数退避重连（`1 s` 起至上限 `10 s`），持久数据错误单独降级提示。
- [ ] 适配器网络异常、字幕回放与上传额度回收加固：将 `transport.rs` 中 `accept()` 未覆盖错误码由 `panic!` 改为记错退避；优化字幕文件增量核验与近期窗口/游标回放，避免历史文件增大导致回放撑爆 `sse_buffer_bytes`；为文件提交超时保留目录补齐可确认回收路径（确认任务状态后释放上传并发额度，不再只能等待重启 daemon）。
- [ ] 日志尾随接口：适配器提供 `GET /api/v1/logs?name=<服务>`，只读返回 `run/logs` 下该服务日志的末尾片段（白名单、定长）；前端在 INFO 面板内查看并每 2 秒刷新，不写文件、不新增 daemon 命令。
- [x] 前端移动端适配、交互细节修复与错误提示美化：保持现有视觉风格不变，补齐移动端顶栏与四项资源展示、按操作优先级排布与底部快捷导航、触控目标与安全区适配；按控件归属分流错误提示并统一告警/悬浮通知视觉与可关闭/自动消隐体验；修复首屏系统暗色主题覆盖、参数失焦丢改与回改残脏、日志切服务乱序与强制回底、字幕视口缩放高度滞后与回底入口，以及 SSE 指数退避重连与克制动效。

- 非文件写入结果未知后，确认须等待未被新状态覆盖的后续 HTTP 状态查询成功；SSE 不得替代查询。

- 上传目录必须为当前用户私有目录，拒绝符号链接及不安全的现有目录，不修改其权限。

勾选项以代码和所列测试为依据；设备部署和产品验收单独保留。
`tools/ci-local.sh adapter` 的两个 daemon 集成测试使用真实 daemon（拉起真实 `mozart-web-adapter`）、真实 ctl 与协议桩，不执行实际 RVC 推理（`web-adapter/tests/daemon.rs`）。
该测试覆盖 HTTP 业务启停（`system/down`/`system/up`）、`ctl down` 全栈停机、配置、文件提交与下载，以及无模型时的路由拒绝。
上传恢复、字幕游标和 HTTP 边界另有单元及集成测试；下列复合验收保留完整门槛。

- 对接单 default 预设和模型选择：`GET /api/v1/preset` 映射 `DAEMON_PRESET_LIST`；`POST /api/v1/preset/save` 和 `/api/v1/preset/use` 分别映射 `DAEMON_PRESET_SAVE` 和 `DAEMON_PRESET_USE`，请求体为空对象；`PUT /api/v1/model` 使用 `ModelUseRequest` 映射 `DAEMON_MODEL_USE`。响应透传 daemon 数据和错误，不复用旧多预设端点。
- [ ] CLI 与 HTTP 对同一 daemon 操作，核对业务状态和错误信封一致。
- [ ] 测试配置拒绝、待重启提示、路由等待与服务降级，网页不显示虚假成功。
- [ ] 测试 multipart 任意字段顺序、超限 chunked、并发额度、断连清理和提交超时不重发。
- [ ] 阻塞 daemon 的 submit 锁后使 HTTP 超时，再重启适配器；核对源文件仍在，直到明确应答或原 daemon 退出。
- [ ] 测试任务排队、执行失败、取消、重启续转，以及下载前驱逐和下载中断。
- [ ] 真实字幕恢复验收核对页面读模型中的话语身份、修订号和正文，不仅检查事件到达。
- [ ] 测试字幕修订、断线恢复、适配器重启、文件替换／截断、半行和坏行。
- [ ] 页面刷新只保留游标而丢失读模型时，核对客户端从头回放；同时恢复二者时才接续游标。
- [ ] 测试回放期间持续追加及 inotify 通知丢失，按游标核对无漏行。
- [ ] 同 inode 截断后立即写至超过旧偏移，并丢失通知；核对 generation 重置及新文件完整回放。
- [ ] 测试慢 SSE 客户端缓冲有界，控制请求和音频链路不受阻塞。
- [ ] 完成网页启动、切换、查看状态和停止的产品验收，保留 SPEC-08 原有门槛。
- [ ] 热点部署与低端口权限接入 SPEC-09。

实时模型选择已接入现有提供方；glossary 编辑和 subtitle clear 继续保留为接口欠账。
两者先在提供方定义命令与状态所有权，再补 HTTP 映射；不得由适配器绕过 daemon 或修改桥正在写的字幕文件。
- [ ] 补齐术语修改生效及安全清除字幕的提供方与 HTTP 契约。

当前主线迁移和控制台复核记录见[2026-11-22 报告](../../reports/web-console-20261122/RESULTS.md)。

本文不以现有命令缺口撤销这些需求；完成上述补充前，不把首批路由视为完整 v1 命令面。
