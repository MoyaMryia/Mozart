# 模型预设（preset）

状态：已实现（2026-11-16；2026-11-17 扩 `model use` 写回 `model_id` 边界）。待办记法见 [SPEC.md](SPEC.md)。
`[x]` 条目详情已于 2026-11-24 移入 [archive/SPEC-11-preset.md](archive/SPEC-11-preset.md)，本文件保留关键决策摘要、活跃待办与契约。

## 目的

把 RVC 会话参数按模型持久化。每个模型目录内存一份预设，选中模型时载入生效配置，运行时调参后可保存回预设。v1 只做单预设（`default.toml`），文件链路验收，实时链路共用同一载入机制。

解决现状：会话参数全局单份 `[rvc]`，所有模型共用，换模型丢调参，无法按模型保存。

## 边界

- 本规格 owns：预设文件契约、载入与保存编排、`preset` 命令面、首次自动创建。
- 会话参数范围校验单一来源在 `core::config`（[config.md](../config.md)），本规格复用，不另定阈值。
- 载入编排复用 [SPEC-04](SPEC-04-daemon.md) 的 `config set rvc.<会话参数>` 判据与 `SetParams` / `Stop/Start` 编排，不新增重载语义。
- 资产目录规则见 [SPEC-10](SPEC-10-env.md) 与 [SETUP.md](../SETUP.md)；`presets/` 可写例外由 SETUP.md 声明。
- `StartStream` 契约不变（[SPEC-03](SPEC-03-rvc.md)）：仍携带五个会话参数，取自 `[rvc]`。
- 术语区分：**模型预设**指会话参数的按模型持久化快照；区别于 SPEC-03 参数边界表的「C++ 内部 preset」（`StreamingRvc` 流式几何参数）。
- 预设载入路径（文件链、路由切换、轮询对账、`preset use`）不碰：模型身份（引擎、index、`spk_id`、采样率）、`[rvc]` 路径与进程启动键、`[rvc].model_id`。
- `model use` 例外（2026-11-17 用户确认的边界变更）：允许把 `[rvc].model_id` 写回 TOML 基线，让 daemon 重启后延续上次选用的模型。写回只发生在 `LoadModel` 成功后的提交临界区，与五键基线共用同一把 toml 锁；失败整体回退（见「载入」与 [SPEC-04](SPEC-04-daemon.md)）。`config set rvc.model_id` 仍拒绝。
- 采样率仍不入预设（属模型身份）。输出帧的 `sample_rate` 与 `valid_samples` 由模型采样率推导，不由预设或旋钮决定（SPEC-01、SPEC-03）。

## 已确认决策（摘要）

> 全文条目与 `file:line` 证据见 [archive/SPEC-11-preset.md](archive/SPEC-11-preset.md)。本节只保留后续开发必须遵守的约束；与归档冲突时以归档原文为准并回来修正本节。

### 存储与文件契约

- 预设放模型目录内 `presets/<name>.toml`，v1 只有 `default.toml`；资产树内唯一可写例外，daemon 是唯一写者，原子写（临时文件 + rename）。
- 预设只含五个会话参数键（与 `[rvc]` 同名）加 `version = 1`；模型身份与路径不入预设；解析与范围校验复用 `core::config` 单一来源，`deny_unknown_fields`。
- 预设缺失：首次加载该模型时自动创建 `default.toml`（规范默认值）。损坏或不认识的版本：入口校验一次、明确报错，不静默回退默认。

### 载入

- 只在模型切换时载入；同模型连续任务不重载，保留运行时调参。切回模型永远载入 `default.toml`，不记忆上次选择。
- 载入分两步：resolve 读并校验预设（缺失则建 default）→ `LoadModel` → commit 一次原子写五键基线并编排。失败都在写入前退出，live 参数与已加载模型不错位。
- daemon 记录「当前 `[rvc]` 参数来自哪个模型的预设」；记录为空或与已加载模型不一致时 `preset save` 拒绝（NotReady）。
- 编排判据复用 SPEC-04：实时流在跑且 `pitch_shift`/`index_rate`/`f0_method` 变化走 `SetParams`（`0x0308`）无中断生效；`startup_delay_ms`（及锁定的 `crossfade_ms`/`sola_search_ms`）变化才 `Stop/Start`。
- `model use` 提交：同一把 toml 锁内先写 `[rvc].model_id` 基线再写五键，任何失败按 `SwitchUndo` 整体回退并用旧模型旧参数恢复流；唯一残留窗口（回退时基线写回失败）记可查询降级原因。
- 模型目录无法解析：自动载入降级为 live 参数并记录原因；显式 `preset save/use/list` 回 NotReady。预设层不预检模型存在性，存在性由 `LoadModel` 回 InvalidArgument。

### 保存与命令面

- `preset save/use/list`（0x011D–0x011F）均作用于当前已加载模型；`use` 重置回预设并丢弃未保存调参；`list` 带 preset/live 值与 dirty 标记。CLI 瘦客户端、`--json` 稳定契约。
- `model use <id>`（0x0120）是第四个载入入口，也是唯一写 `[rvc].model_id` 基线的命令；命令契约与编排细节见 [SPEC-04](SPEC-04-daemon.md)，本文件不复述。

### 范围与迁移

- v1 单预设；多预设、按名 use、delete、last-used 见「改进点」。
- 文件任务调参 = 下任务生效（当前任务用启动快照跑完，不中途变参）；实时流期间经 `SetParams` 块边界无中断生效。
- `[rvc]` 语义为「当前载入的 live 草稿」；持久来源是按模型预设文件。迁移无例外：升级后首次加载按统一规则建 default 并载入。

### 验收与测试

- 本机 `ci-local.sh all`、`daemon/tests/preset.rs` 集成、板端 `--preset` 8 场景、`--file` 回归 9 场景、`model use` 成功与四条失败路径、板端跨采样率冒烟均已通过；逐条场景与日期证据见归档「验收」节及下文「验证状态」。
- 遗留缺口：commit 内「model_id 写成功 + 五键写失败 + 写回失败」三段时序缺集成覆盖，需 core 提供 TOML 写失败注入旋钮。

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

## 测试策略

- 单元（Rust，各 crate `src/` 或 `tests/`）：预设解析与校验、首次自动建、save 原子写、载入批量写基线 + 编排、模型切换检测、NotReady 与损坏分支。
- 集成（`daemon/tests/`）：模型切换载入、preset use 实时 `SetParams` 热更与回退、文件下任务生效、跨链不污染、`model use` 成功与四条失败路径。
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

## 待讨论

- 无重大未决。`version` 演进策略待格式变更时定。

## 验证状态

- 2026-11-16 本机：`tools/ci-local.sh all` 通过（repo、lint、native、rust；web 未建跳过）。
- 2026-11-16 修订（载入拆两步、损坏预设回 Internal、save 校验参数来源、CLI 三命令长超时）：本机 `tools/ci-local.sh all` 通过（repo、lint、native、rust；web 未建跳过）。
- 集成（2026-11-16）：`daemon/tests/preset.rs` 11 项通过，覆盖首次自动建、save/use/list、模型切换、实时 Stop/Start、跨链、损坏、文件与实时加载失败保护、参数来源未知拒绝、模型目录缺失报错归推理层、NotReady。
- 2026-11-17 本机（`model use` 与 `model_id` 写回边界）：`tools/ci-local.sh all` 通过（repo、lint、native、rust；web 未建跳过）。`daemon/tests/preset.rs` 18 项通过，新增 7 项覆盖 `model use` 成功只起一次流、LoadModel/提交/起流/恢复四条失败路径、文件占 GPU 回 Busy、损坏预设停流前拒绝。`daemon/tests/rvc_params.rs` 覆盖 `[rvc.realtime]` 旋钮与 `model use` 的 Stop/Start 编排。板端 `model use` 与跨采样率场景未跑，阻塞原因与解除条件见 SPEC-03 板端条目。
- 2026-11-18 本机（`SetParams` 接入 `preset use` / `config set`）：`tools/ci-local.sh all` 通过（C++ 与 Rust 在线调参全量用例通过）。
- 2026-11-18 板端：`tools/jetson-e2e.sh --unit --smoke --preset --realtime-mm --file` 全过（`--preset` 8 场景含实时 `preset use` 不断流载入、`--realtime-mm` 10 场景含 `SetParams 在线调参不断流`、设备端 `cargo test` 647 项 + `ctest` 10/10 全过）。带人声输出下 `preset use` 块边界热更的接缝主观听感仍未验证（阻塞原因：现有套件只校验 Loopback 下流不断裂、帧守恒与日志落盘；解除条件：向 Loopback 播放人声触发 `preset use` 并录制输出波形/听感验收）。
- 2026-11-16 板端：`tools/jetson-e2e.sh --preset` 8 场景全过（真 daemon + pre + rvc，两个模型经符号链接放临时模型目录，资产树不写）。实测确认残缺模型的失败发生在 `LoadModel` 而非预设解析，live 参数、TOML 基线与已加载模型预设三者不变。
- 板端与桩的行为差异：真 rvc 加载新模型前驱逐空闲模型，加载失败后可能无模型在载，此时 `preset save` 回 NotReady「no model loaded」属预期，不是来源守卫拒绝；桩不驱逐，故该分支只在板端可见。
- 2026-11-16 板端回归：`tools/jetson-e2e.sh --file --preset` 全过（文件链 9 场景、preset 8 场景）。`--file` 首轮暴露一处回归：预设层曾以「模型目录不存在」抢在 `LoadModel` 前报 Internal，违反 SPEC-10「模型存在性归推理层」；已改为目录不存在时跳过载入，由 `LoadModel` 回 InvalidArgument「model not found」，并补集成用例 `missing_model_dir_reports_load_model_error_not_preset_error` 锁定该契约。
