# 模型预设（preset）

状态：已实现（2026-11-16；2026-11-17 扩 `model use` 写回 `model_id` 边界）。待办记法见 [SPEC.md](SPEC.md)。

## 目的

把 RVC 会话参数按模型持久化。每个模型目录内存一份预设，选中模型时载入生效配置，运行时调参后可保存回预设。v1 只做单预设（`default.toml`），文件链路验收，实时链路共用同一载入机制。

解决现状：会话参数全局单份 `[rvc]`，所有模型共用，换模型丢调参，无法按模型保存。

## 边界

- 本规格 owns：预设文件契约、载入与保存编排、`preset` 命令面、首次自动创建。
- 会话参数范围校验单一来源在 `core::config`（[config.md](../config.md)），本规格复用，不另定阈值。
- 载入编排复用 [SPEC-04](SPEC-04-daemon.md) 的 `config set rvc.<会话参数>` 判据与 Stop/Start 编排，不新增重载语义。
- 资产目录规则见 [SPEC-10](SPEC-10-env.md) 与 [SETUP.md](../SETUP.md)；`presets/` 可写例外由 SETUP.md 声明。
- `StartStream` 契约不变（[SPEC-03](SPEC-03-rvc.md)）：仍携带五个会话参数，取自 `[rvc]`。
- 术语区分：**模型预设**指会话参数的按模型持久化快照；区别于 SPEC-03 参数边界表的「C++ 内部 preset」（`StreamingRvc` 流式几何参数）。
- 预设载入路径（文件链、路由切换、轮询对账、`preset use`）不碰：模型身份（引擎、index、`spk_id`、采样率）、`[rvc]` 路径与进程启动键、`[rvc].model_id`。
- `model use` 例外（2026-11-17 用户确认的边界变更）：允许把 `[rvc].model_id` 写回 TOML 基线，让 daemon 重启后延续上次选用的模型。写回只发生在 `LoadModel` 成功后的提交临界区，与五键基线共用同一把 toml 锁；失败整体回退（见「载入」与 [SPEC-04](SPEC-04-daemon.md)）。`config set rvc.model_id` 仍拒绝。
- 采样率仍不入预设（属模型身份）。输出帧的 `sample_rate` 与 `valid_samples` 由模型采样率推导，不由预设或旋钮决定（SPEC-01、SPEC-03）。

## 已确认

2026-11-16 拷问定稿并实现。

### 存储与文件契约

- [x] 预设放模型目录内 `presets/` 子目录：`~/mozart-assets/models/rvc/<id>/presets/`。SETUP.md 声明其为资产树内唯一可写例外，其余仍只读。
- [x] daemon 是预设唯一写者，原子写（临时文件 + rename），沿用 `config save` 做法。
- [x] 一预设一文件 `presets/<name>.toml`；`default.toml` 固定名即默认，无额外默认标记。
- [x] 预设内容只含五个会话参数键（与 `[rvc]` 同名）加 `version = 1`。模型身份与路径不入预设。
- [x] 预设解析与范围校验复用 `core::config` 会话参数校验（单一来源）；`deny_unknown_fields`。
- [x] 预设缺失：首次加载该模型时自动创建 `default.toml`（见下）。
- [x] 当前模型已加载但 `default.toml` 被删：`preset use` 按首次载入自动创建并载入；`preset list` 只读，回 NotReady 且不写盘。
- [x] 预设损坏或版本不认识：入口校验一次，明确报错停止，不静默回退默认。文件任务标 failed 带原因、队列继续（同 LoadModel 损坏处理）；`preset use` 返回错误、不写基线。

### 载入

- [x] 触发点：模型切换（目标模型 ≠ 当前已加载模型）。同模型连续任务不重载，保留运行时调参。
- [x] 切换判据由 daemon 记录「当前 `[rvc]` 参数来自哪个模型的预设」：daemon 启动为空，首次载入必读；同模型跳过。rvc 进程重启不重置该记录，保留未保存调参。基线写入并编排成功后记录为目标模型；写入在 `LoadModel` 成功之后，加载失败不改记录也不改 live 参数。`preset save` 在记录为空或与 rvc 已加载模型不一致时拒绝，避免覆盖错预设。
- [x] 模型目录无法解析（无注册表且 `models_dir` 为空或模型 id 非纯目录名）时，自动载入降级为 live 参数并记录可查询降级原因；显式 `preset save/use/list` 仍回 NotReady。
- [x] 预设层不预检模型存在性（SPEC-10）：目录可解析但不存在时跳过载入，由 `LoadModel` 回 InvalidArgument「model not found」，任务标 failed、队列继续。预设只在目录存在时读或建。
- [x] 文件与实时两个模型切换入口挂同一对载入函数（resolve + commit）。`model use` 共用 resolve，commit 用带模型身份的变体（见下条）。
- [x] 文件链路落点：GPU 交接（`pre_set_file_mode(true)`）完成后读并校验预设，`LoadModel` 成功后写基线并取参数快照，再起文件流。参数快照时机从 `start_job` 后移到交接后（`daemon/src/file/mod.rs:428`、`daemon/src/file/worker.rs`）。
- [x] 实时链路落点：模型切换 `LoadModel` 前读并校验，`LoadModel` 成功后写基线，再 `StartStream`；路由切换与对账同序。
- [x] 载入动作分两步：resolve 读并校验预设（缺失则建 default），commit 一次 `toml_edit` 原子写五个参数键到 TOML 基线 → 重建生效配置 → `apply_rvc_session_params_locked` 编排（`daemon/src/services.rs:881`）。预设载入路径只写五键，不碰 `[rvc]` 其余键。
- [x] `LoadModel` 夹在两步之间：预设损坏与模型加载失败都在写入前退出，live 参数与已加载模型不错位，仅剩「模型加载成功但写基线失败」的窄窗口。文件链两步之间放开 transition 锁，commit 重新取锁并复核记录与当前生效配置。
- [x] 编排判据复用现有：实时流在跑且 pitch/index/f0 或 `[rvc.realtime]` 四键变化才 Stop/Start（判据见 SPEC-04）；文件交接后实时已停，自然只更新配置不打断。
- [x] `model use` 的提交与回退：resolve 在停流前读校验（损坏预设不打断在跑的流）→ StopStream → LoadModel → `commit_model_switch_locked` 在同一把 toml 锁内先写 `[rvc].model_id` 基线再写五键基线 → 重建生效配置 → 编排 → 记录 applied，返回 `SwitchUndo` 回退快照 → StartStream 复用 handle 只起一次。任何失败都用旧模型 + 旧参数恢复流；起流失败先按 `SwitchUndo` 整体回退基线、生效配置与 applied，再恢复旧流。恢复失败记持久降级原因（前缀 `rvc model use`）。唯一残留窗口是回退时 TOML 写回本身失败，记 `preset: model switch baseline restore: …; TOML baseline may still name the target model`。命令契约与编排细节见 [SPEC-04](SPEC-04-daemon.md)。（`daemon/src/preset.rs:139-195,348-359`、`daemon/src/services.rs:978-1059`）
- [x] `model use` 的两种跳过：预设跳过（同模型已载入或目录不可解析）与无 TOML 基线（daemon 未带 `--config`）时只落模型身份，保留 live 会话参数。（`daemon/src/preset.rs:151-170,339-345`）
- [x] 首次自动创建：模型首次加载且无 `default.toml` → 用 `core::config` 规范默认值（pitch 0、index_rate 0.0、rms_mix_rate 1.0、protect 0.33、rmvpe）创建 → 载入。
- [x] 切回模型永远载入 `default.toml`，不记忆上次选择（v1 单预设，无 last-used）。

### 保存与命令面

- [x] `preset save`：读当前生效配置五个参数 → 原子写当前模型 `default.toml`。不写 TOML。
- [x] `preset save` 在参数来源记录为空或与 rvc 已加载模型不一致时拒绝（NotReady），避免把别的模型参数写进当前模型预设；先 `preset use` 载入一次再保存。
- [x] `preset use`：读当前模型 `default.toml` → 走与模型切换相同的载入路径（重置回预设，丢弃未保存调参）。
- [x] `preset list`：返回当前模型预设值与 live 生效值及 dirty 标记。
- [x] 三个命令目标均为当前已加载模型（`rvc_status.model_id`）；无模型加载回 NotReady（同 `resolve_model`，`daemon/src/file/mod.rs:378`）。
- [x] 命令号 daemon 段顺位新增：PRESET_SAVE 0x011D、PRESET_USE 0x011E、PRESET_LIST 0x011F（`core/include/mozart/ctl.h` + `core/src/ctl.rs` 镜像与编译期断言同步）。
- [x] CLI 名词-动词：`mozart-ctl preset save|use|list`，`--json` 稳定契约，瘦客户端零决策。
- [x] `model use <id>` 是第四个载入入口，也是唯一写 `[rvc].model_id` 基线的命令：自动载入目标模型 `default.toml`，失败自动恢复旧模型与旧参数。命令号 0x0120、体与 data、Busy 判据与长超时见 [SPEC-04](SPEC-04-daemon.md)，本文件不复述。

### 范围与迁移

- [x] v1 单预设：只 `default.toml`。多预设、按名 use、delete、last-used 见本文件改进点。
- [x] 实时调整语义 = 下任务生效：`config set` 即时写生效配置，当前文件任务用启动快照跑完，下一任务用新参。不做任务中途变参（FUTURE SetParams）。
- [x] 迁移无例外：升级后首次加载按统一规则建 default 并载入。用户未手调过 `[rvc]`，无保护需求。
- [x] `[rvc]` 语义变化：从「全局持久参数来源」变为「当前载入的 live 草稿」；持久来源改为按模型预设文件。`config set`/`config save` 机制不变，五参数仍归运行时键。`[rvc].model_id` 基线只由 `model use` 写回，预设文件仍不含模型身份。

## 预设文件契约

位置：`~/mozart-assets/models/rvc/<id>/presets/default.toml`。

```toml
version = 1
pitch_shift = 0
index_rate = 0.0
rms_mix_rate = 1.0
protect = 0.33
f0_method = "rmvpe"
```

- 键名与 `[rvc]` 会话参数一致；范围校验复用 `core::config`（pitch -12..12、index_rate 0..1、rms_mix_rate 0..1、protect 0..0.5、f0_method 仅 rmvpe）。
- `version` 当前固定 1；读时校验，不认识拒绝。
- 写入原子（临时文件 + rename）；读入口校验一次。

## 命令体契约

| 命令 | 号 | 体 | data |
| --- | --- | --- | --- |
| preset save | 0x011D | `{}` | `{"model_id","preset":"default","values":{五参数}}` |
| preset use | 0x011E | `{}` | `{"model_id","preset":"default","values":{五参数}}`；编排失败回 error code/reason |
| preset list | 0x011F | `{}` | `{"model_id","preset":"default","preset_values":{…},"live_values":{…},"dirty":bool}` |

v1 体均空（固定 default、当前模型）。多预设阶段体扩 `{"name"?}`，data 扩 presets 数组。

## 验收

- [x] 新模型首次 `file submit --model <id>`：自动建 `presets/default.toml`（规范默认值），任务用该值完成。
- [x] `preset save` 后 `default.toml` 内容等于 save 时的 live 五参数；TOML 不变。
- [x] `config set rvc.pitch_shift 5` 后下一文件任务用 5；当前任务不中断、用旧参。
- [x] `preset use` 后 live 五参数回到 `default.toml`；实时流在跑且 pitch/index/f0 或 `[rvc.realtime]` 四键变化时短暂 Stop/Start。
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

## 测试策略

- 单元（Rust，各 crate `src/` 或 `tests/`）：预设解析与校验、首次自动建、save 原子写、载入批量写基线 + 编排、模型切换检测、NotReady 与损坏分支。
- 集成（`daemon/tests/`）：模型切换载入、preset use 实时 Stop/Start、文件下任务生效、跨链不污染、`model use` 成功与四条失败路径。
- 板端（`tools/jetson-e2e.sh`）：新增 preset 功能套件；file-models 套件用临时模型目录副本或测后清理 `presets/`，避免污染资产。
- 命令：本机 `tools/ci-local.sh all`；板端 `tools/jetson-e2e.sh <SSH目标> --file`（preset 套件并入或独立）。

## 实现约定

- 代码风格、校验与降级、仓库卫生遵循 [AGENTS.md](../../AGENTS.md)。
- 模块结构见 [SPEC.md](SPEC.md) 模块索引；预设代码落 daemon（命令、编排、保存）、core（命令号、校验复用、预设解析）、ctl（CLI）。
- 构建与检查命令见 [SETUP.md](../SETUP.md)：`tools/ci-local.sh`、`tools/deploy-jetson.sh`、`tools/jetson-e2e.sh`。

## 边界

- Always：入口校验一次；范围阈值单一来源复用 `core::config`；原子写；改状态告知；测试先于提交。
- Ask first：超出本规格范围的契约改动；新增依赖；CI 配置变更。
- Never：运行时写权重、引擎、config.json；静默回退损坏预设；预设里塞模型身份或路径；复制范围阈值。

## 文档同步

已随实现更新（最小交叉引用，契约以本文件为单一来源）：

- SETUP.md：`presets/` 可写例外；资产布局补 `presets/`。
- SPEC-10-env.md：成品目录契约补 `presets/`；生命周期注记首次运行自动建预设。
- SPEC-03-rvc.md：`StartStream` 来源注记「模型切换时从预设载入 `[rvc]`，契约不变」；术语区分。
- SPEC-04-daemon.md：preset 命令面与载入编排引用本文件；快照时机后移；`model use` 命令契约、次序与回退（2026-11-17）。
- config.md：`[rvc]` 会话语义注记（live 草稿）；preset 命令引用；`model_id` 只由 `model use` 写回（2026-11-17）。
- GUIDE.md：`model use` 用法与失败自动恢复（2026-11-17）。
- SPEC.md：模块索引加 SPEC-11 行；变更记录加 2026-11-16 与 2026-11-17 条目。
- ARCHITECTURE-HIGHLIGHTS.md：预设层要点。

## 改进点 / FUTURE

- [ ] 多预设：一模型多 `presets/<name>.toml`，按名 use/save/delete。
- [ ] last-used：切回模型恢复上次用的预设（需 per-model 持久状态）。
- [ ] 设默认：把任意预设设为 default。
- [ ] 命名字符集与校验（多预设阶段定）。
- [ ] 无中断 SetParams（块边界调参，FUTURE 已有）。

## 待讨论

- 无重大未决。`version` 演进策略待格式变更时定。

## 验证状态

- 2026-11-16 本机：`tools/ci-local.sh all` 通过（repo、lint、native、rust；web 未建跳过）。
- 2026-11-16 修订（载入拆两步、损坏预设回 Internal、save 校验参数来源、CLI 三命令长超时）：本机 `tools/ci-local.sh all` 通过（repo、lint、native、rust；web 未建跳过）。
- 集成（2026-11-16）：`daemon/tests/preset.rs` 11 项通过，覆盖首次自动建、save/use/list、模型切换、实时 Stop/Start、跨链、损坏、文件与实时加载失败保护、参数来源未知拒绝、模型目录缺失报错归推理层、NotReady。
- 2026-11-17 本机（`model use` 与 `model_id` 写回边界）：`tools/ci-local.sh all` 通过（repo、lint、native、rust；web 未建跳过）。`daemon/tests/preset.rs` 18 项通过，新增 7 项覆盖 `model use` 成功只起一次流、LoadModel/提交/起流/恢复四条失败路径、文件占 GPU 回 Busy、损坏预设停流前拒绝。`daemon/tests/rvc_params.rs` 覆盖 `[rvc.realtime]` 旋钮与 `model use` 的 Stop/Start 编排。板端 `model use` 与跨采样率场景未跑，阻塞原因与解除条件见 SPEC-03 板端条目。
- 2026-11-16 板端：`tools/jetson-e2e.sh --preset` 8 场景全过（真 daemon + pre + rvc，两个模型经符号链接放临时模型目录，资产树不写）。实测确认残缺模型的失败发生在 `LoadModel` 而非预设解析，live 参数、TOML 基线与已加载模型预设三者不变。
- 板端与桩的行为差异：真 rvc 加载新模型前驱逐空闲模型，加载失败后可能无模型在载，此时 `preset save` 回 NotReady「no model loaded」属预期，不是来源守卫拒绝；桩不驱逐，故该分支只在板端可见。
- 2026-11-16 板端回归：`tools/jetson-e2e.sh --file --preset` 全过（文件链 9 场景、preset 8 场景）。`--file` 首轮暴露一处回归：预设层曾以「模型目录不存在」抢在 `LoadModel` 前报 Internal，违反 SPEC-10「模型存在性归推理层」；已改为目录不存在时跳过载入，由 `LoadModel` 回 InvalidArgument「model not found」，并补集成用例 `missing_model_dir_reports_load_model_error_not_preset_error` 锁定该契约。