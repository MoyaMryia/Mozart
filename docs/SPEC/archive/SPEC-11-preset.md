# SPEC-11 归档（preset）

机制见 [SPEC.md](../SPEC.md)「归档」一节。本文件保存 [SPEC-11-preset.md](../SPEC-11-preset.md) 已完成 `[x]` 条目的全文与证据，append-only，不改写历史。活跃待办与关键决策摘要见源文件。

## 2026-11-24 首次归档

范围：「已确认」四节全部 `[x]`、「验收」全部 `[x]`、「测试策略」1 项 `[x]`、「改进点」1 项 `[x]`。链接已按归档目录调整深度。

### 已确认

2026-11-16 拷问定稿并实现。

#### 存储与文件契约

- [x] 预设放模型目录内 `presets/` 子目录：`~/mozart-assets/models/rvc/<id>/presets/`。SETUP.md 声明其为资产树内唯一可写例外，其余仍只读。
- [x] daemon 是预设唯一写者，原子写（临时文件 + rename），沿用 `config save` 做法。
- [x] 一预设一文件 `presets/<name>.toml`；`default.toml` 固定名即默认，无额外默认标记。
- [x] 预设内容只含五个会话参数键（与 `[rvc]` 同名）加 `version = 1`。模型身份与路径不入预设。
- [x] 预设解析与范围校验复用 `core::config` 会话参数校验（单一来源）；`deny_unknown_fields`。
- [x] 预设缺失：首次加载该模型时自动创建 `default.toml`（见下）。
- [x] 当前模型已加载但 `default.toml` 被删：`preset use` 按首次载入自动创建并载入；`preset list` 只读，回 NotReady 且不写盘。
- [x] 预设损坏或版本不认识：入口校验一次，明确报错停止，不静默回退默认。文件任务标 failed 带原因、队列继续（同 LoadModel 损坏处理）；`preset use` 返回错误、不写基线。

#### 载入

- [x] 触发点：模型切换（目标模型 ≠ 当前已加载模型）。同模型连续任务不重载，保留运行时调参。
- [x] 切换判据由 daemon 记录「当前 `[rvc]` 参数来自哪个模型的预设」：daemon 启动为空，首次载入必读；同模型跳过。rvc 进程重启不重置该记录，保留未保存调参。基线写入并编排成功后记录为目标模型；写入在 `LoadModel` 成功之后，加载失败不改记录也不改 live 参数。`preset save` 在记录为空或与 rvc 已加载模型不一致时拒绝，避免覆盖错预设。
- [x] 模型目录无法解析（无注册表且 `models_dir` 为空或模型 id 非纯目录名）时，自动载入降级为 live 参数并记录可查询降级原因；显式 `preset save/use/list` 仍回 NotReady。
- [x] 预设层不预检模型存在性（SPEC-10）：目录可解析但不存在时跳过载入，由 `LoadModel` 回 InvalidArgument「model not found」，任务标 failed、队列继续。预设只在目录存在时读或建。
- [x] 文件与实时两个模型切换入口挂同一对载入函数（resolve + commit）。`model use` 共用 resolve，commit 用带模型身份的变体（见下条）。
- [x] 文件链路落点：GPU 交接（`pre_set_file_mode(true)`）完成后读并校验预设，`LoadModel` 成功后写基线并取参数快照，再起文件流。参数快照时机从 `start_job` 后移到交接后（`daemon/src/file/mod.rs:428`、`daemon/src/file/worker.rs`）。
- [x] 实时链路落点：模型切换 `LoadModel` 前读并校验，`LoadModel` 成功后写基线，再 `StartStream`；路由切换与对账同序。
- [x] 载入动作分两步：resolve 读并校验预设（缺失则建 default），commit 一次 `toml_edit` 原子写五个参数键到 TOML 基线 → 重建生效配置 → `apply_rvc_session_params_locked` 编排（`daemon/src/services.rs:881`）。预设载入路径只写五键，不碰 `[rvc]` 其余键。
- [x] `LoadModel` 夹在两步之间：预设损坏与模型加载失败都在写入前退出，live 参数与已加载模型不错位，仅剩「模型加载成功但写基线失败」的窄窗口。文件链两步之间放开 transition 锁，commit 重新取锁并复核记录与当前生效配置。
- [x] 编排判据复用现有：实时流在跑且 `pitch_shift`/`index_rate`/`f0_method` 变化时下发 `SetParams`（`0x0308`）无中断生效，`[rvc.realtime]` 中的 `startup_delay_ms`（及锁定的 `crossfade_ms`/`sola_search_ms`）变化才走 `Stop/Start`（判据见 SPEC-04）；文件交接后实时已停，自然只更新配置不打断。
- [x] `model use` 的提交与回退：resolve 在停流前读校验（损坏预设不打断在跑的流）→ StopStream → LoadModel → `commit_model_switch_locked` 在同一把 toml 锁内先写 `[rvc].model_id` 基线再写五键基线 → 重建生效配置 → 编排 → 记录 applied，返回 `SwitchUndo` 回退快照 → StartStream 复用 handle 只起一次。任何失败都用旧模型 + 旧参数恢复流；起流失败先按 `SwitchUndo` 整体回退基线、生效配置与 applied，再恢复旧流。恢复失败记持久降级原因（前缀 `rvc model use`）。唯一残留窗口是回退时 TOML 写回本身失败，记 `preset: model switch baseline restore: …; TOML baseline may still name the target model`。命令契约与编排细节见 [SPEC-04](../SPEC-04-daemon.md)。（`daemon/src/preset.rs:139-195,348-359`、`daemon/src/services.rs:978-1059`）
- [x] `model use` 的两种跳过：预设跳过（同模型已载入或目录不可解析）与无 TOML 基线（daemon 未带 `--config`）时只落模型身份，保留 live 会话参数。（`daemon/src/preset.rs:151-170,339-345`）
- [x] 首次自动创建：模型首次加载且无 `default.toml` → 用 `core::config` 规范默认值（pitch 0、index_rate 0.0、rms_mix_rate 1.0、protect 0.33、rmvpe）创建 → 载入。
- [x] 切回模型永远载入 `default.toml`，不记忆上次选择（v1 单预设，无 last-used）。

#### 保存与命令面

- [x] `preset save`：读当前生效配置五个参数 → 原子写当前模型 `default.toml`。不写 TOML。
- [x] `preset save` 在参数来源记录为空或与 rvc 已加载模型不一致时拒绝（NotReady），避免把别的模型参数写进当前模型预设；先 `preset use` 载入一次再保存。
- [x] `preset use`：读当前模型 `default.toml` → 走与模型切换相同的载入路径（重置回预设，丢弃未保存调参）。
- [x] `preset list`：返回当前模型预设值与 live 生效值及 dirty 标记。
- [x] 三个命令目标均为当前已加载模型（`rvc_status.model_id`）；无模型加载回 NotReady（同 `resolve_model`，`daemon/src/file/mod.rs:378`）。
- [x] 命令号 daemon 段顺位新增：PRESET_SAVE 0x011D、PRESET_USE 0x011E、PRESET_LIST 0x011F（`core/include/mozart/ctl.h` + `core/src/ctl.rs` 镜像与编译期断言同步）。
- [x] CLI 名词-动词：`mozart-ctl preset save|use|list`，`--json` 稳定契约，瘦客户端零决策。
- [x] `model use <id>` 是第四个载入入口，也是唯一写 `[rvc].model_id` 基线的命令：自动载入目标模型 `default.toml`，失败自动恢复旧模型与旧参数。命令号 0x0120、体与 data、Busy 判据与长超时见 [SPEC-04](../SPEC-04-daemon.md)，本文件不复述。

#### 范围与迁移

- [x] v1 单预设：只 `default.toml`。多预设、按名 use、delete、last-used 见本文件改进点。
- [x] 文件任务调整语义 = 下任务生效：`config set` 即时写生效配置，当前文件任务用启动快照跑完，下一任务用新参（不做文件任务中途变参）；实时流活动期间经 `SetParams` 在块边界无中断生效。
- [x] 迁移无例外：升级后首次加载按统一规则建 default 并载入。用户未手调过 `[rvc]`，无保护需求。
- [x] `[rvc]` 语义变化：从「全局持久参数来源」变为「当前载入的 live 草稿」；持久来源改为按模型预设文件。`config set`/`config save` 机制不变，五参数仍归运行时键。`[rvc].model_id` 基线只由 `model use` 写回，预设文件仍不含模型身份。

### 验收

- [x] 新模型首次 `file submit --model <id>`：自动建 `presets/default.toml`（规范默认值），任务用该值完成。
- [x] `preset save` 后 `default.toml` 内容等于 save 时的 live 五参数；TOML 不变。
- [x] `config set rvc.pitch_shift 5` 后下一文件任务用 5；当前任务不中断、用旧参。
- [x] `preset use` 后 live 五参数回到 `default.toml`；实时流在跑且 `pitch_shift`/`index_rate`/`f0_method` 变化时经 `SetParams`（`0x0308`）无中断生效。
- [x] 模型 A 切 B：B 的 default 载入 `[rvc]`；切回 A：A 的 default 载入。同模型连续任务不重载。
- [x] 实时模型切换同样载入预设；文件切模型后实时切回不用错参数。
- [x] 损坏预设：文件任务 failed 带原因（Internal）、队列继续；`preset use` 报错不写基线。
- [x] 模型加载失败（文件链与实时链）：live 五参数与 TOML 基线不变，已加载模型的预设不被覆盖，`preset save` 仍成功。
- [x] 无模型加载时三个 preset 命令回 NotReady。
- [x] `preset list` 正确反映 preset 与 live 差异（dirty）。
- [x] 板端 preset 套件通过（8 场景：NotReady、首次自动建、save/use/list、实时载入、A→B→A 切换与同模型保留调参、加载失败不改 live 与基线、损坏预设、资产无残留）；资产 `presets/` 测试后不残留污染。
- [x] 板端 `jetson-e2e.sh --file` 回归通过（9 场景，含未知模型拒绝路径）。
- [x] `model use` 成功路径：目标预设载入，`[rvc].model_id` 基线与生效配置落目标模型，只起一次流（集成用例 `model_use_success_commits_baseline_and_starts_one_stream`）。
- [x] `model use` 失败路径：LoadModel 失败、提交失败、起流失败、恢复失败四条都用旧模型旧参数恢复；起流失败整体回退基线、生效配置与 applied；恢复失败留可查询降级原因（`daemon/tests/preset.rs` 四个对应用例）。
- [x] `model use` 在文件任务占 GPU 时回 Busy；损坏预设在停流前拒绝，不打断在跑的流。
- [x] 板端 `model use` 与跨采样率切换冒烟通过（48 kHz↔40 kHz、切回载入预设、未知模型失败恢复旧模型旧参数、文件占 GPU 回 Busy）；延迟与接缝听感仍未验证，见 SPEC-03 板端剩余条目。
- [x] `model use` 回退时 TOML 写回失败分支已覆盖：单测把基线目录置只读确定性构造 `atomic_write` 失败，断言持久降级原因可查、live 与 applied 仍回退（`daemon/src/preset.rs` `rollback_records_queryable_degraded_when_baseline_write_fails`）。残留缺口：commit 内「model_id 写成功 + 五键写失败 + 写回失败」三段时序的集成覆盖仍缺，需 core 提供 TOML 写失败注入旋钮才能确定性构造；失败分支代码本身已被单测覆盖。

### 测试策略

- [x] 预设测试首次切实时路由前等待 pre 控制面应答，不将子进程 `Running` 视为 socket 就绪；保持原等待预算和业务断言。

### 改进点 / FUTURE

- [x] 无中断 SetParams（块边界调参）：`preset use` 与 `config set` 在实时流期间经 `RVC_SET_PARAMS`（`0x0308`）在块边界生效，不断流。
