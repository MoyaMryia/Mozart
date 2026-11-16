# 模型预设（preset）

状态：已实现（2026-11-16）。待办记法见 [SPEC.md](SPEC.md)。

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
- 不碰：模型身份（引擎、index、`spk_id`、采样率）、`[rvc]` 路径与进程启动键、`[rvc].model_id`。

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
- [x] 切换判据由 daemon 记录「当前 `[rvc]` 参数来自哪个模型的预设」：daemon 启动为空，首次载入必读；同模型跳过。rvc 进程重启不重置该记录，保留未保存调参。基线写入成功后记录为目标模型；随后 rvc 加载失败不回退记录，下一次目标模型不同会重新载入正确预设。`preset save` 在记录与 rvc 已加载模型不一致时拒绝，避免覆盖错预设。
- [x] 模型目录无法解析（无注册表且 `models_dir` 为空或模型 id 非纯目录名）时，自动载入降级为 live 参数并记录可查询降级原因；显式 `preset save/use/list` 仍回 NotReady。
- [x] 文件与实时两个模型切换入口挂同一载入函数。
- [x] 文件链路落点：GPU 交接（`pre_set_file_mode(true)`）完成后、`LoadModel` 与取参数快照前。参数快照时机从 `start_job` 后移到交接后（`daemon/src/file/mod.rs:428`、`daemon/src/file/worker.rs`）。
- [x] 实时链路落点：模型切换 `LoadModel` 前。
- [x] 载入动作：一次 `toml_edit` 原子写五个参数键到 TOML 基线 → 重建生效配置 → `apply_rvc_session_params_locked` 编排（`daemon/src/services.rs:839`）。只写五键，不碰 `[rvc]` 其余键。
- [x] 编排判据复用现有：实时流在跑且 pitch/index/f0 变化才 Stop/Start；文件交接后实时已停，自然只更新配置不打断。
- [x] 首次自动创建：模型首次加载且无 `default.toml` → 用 `core::config` 规范默认值（pitch 0、index_rate 0.0、rms_mix_rate 1.0、protect 0.33、rmvpe）创建 → 载入。
- [x] 切回模型永远载入 `default.toml`，不记忆上次选择（v1 单预设，无 last-used）。

### 保存与命令面

- [x] `preset save`：读当前生效配置五个参数 → 原子写当前模型 `default.toml`。不写 TOML。
- [x] `preset save` 在当前 `[rvc]` 基线模型与 rvc 已加载模型不一致时拒绝（模型加载失败窗口），避免把别的模型参数写进当前模型预设。
- [x] `preset use`：读当前模型 `default.toml` → 走与模型切换相同的载入路径（重置回预设，丢弃未保存调参）。
- [x] `preset list`：返回当前模型预设值与 live 生效值及 dirty 标记。
- [x] 三个命令目标均为当前已加载模型（`rvc_status.model_id`）；无模型加载回 NotReady（同 `resolve_model`，`daemon/src/file/mod.rs:378`）。
- [x] 命令号 daemon 段顺位新增：PRESET_SAVE 0x011D、PRESET_USE 0x011E、PRESET_LIST 0x011F（`core/include/mozart/ctl.h` + `core/src/ctl.rs` 镜像与编译期断言同步）。
- [x] CLI 名词-动词：`mozart-ctl preset save|use|list`，`--json` 稳定契约，瘦客户端零决策。

### 范围与迁移

- [x] v1 单预设：只 `default.toml`。多预设、按名 use、delete、last-used 推迟 FUTURE。
- [x] 实时调整语义 = 下任务生效：`config set` 即时写生效配置，当前文件任务用启动快照跑完，下一任务用新参。不做任务中途变参（FUTURE SetParams）。
- [x] 迁移无例外：升级后首次加载按统一规则建 default 并载入。用户未手调过 `[rvc]`，无保护需求。
- [x] `[rvc]` 语义变化：从「全局持久参数来源」变为「当前载入的 live 草稿」；持久来源改为按模型预设文件。`config set`/`config save` 机制不变，五参数仍归运行时键。

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
- [x] `preset use` 后 live 五参数回到 `default.toml`；实时流在跑且 pitch/index/f0 变化时短暂 Stop/Start。
- [x] 模型 A 切 B：B 的 default 载入 `[rvc]`；切回 A：A 的 default 载入。同模型连续任务不重载。
- [x] 实时模型切换同样载入预设；文件切模型后实时切回不用错参数。
- [x] 损坏预设：文件任务 failed 带原因、队列继续；`preset use` 报错不写基线。
- [x] 无模型加载时三个 preset 命令回 NotReady。
- [x] `preset list` 正确反映 preset 与 live 差异（dirty）。
- [ ] 板端 `jetson-e2e.sh --file` 与新增 preset 套件通过；资产 `presets/` 测试后不残留污染。（脚本已实现；本机无 SSH 目标，未运行。解除条件：提供可达 Jetson 目标后执行 `MOZART_JETSON=<目标> tools/jetson-e2e.sh --preset`。）

## 测试策略

- 单元（Rust，各 crate `src/` 或 `tests/`）：预设解析与校验、首次自动建、save 原子写、载入批量写基线 + 编排、模型切换检测、NotReady 与损坏分支。
- 集成（`daemon/tests/`）：模型切换载入、preset use 实时 Stop/Start、文件下任务生效、跨链不污染。
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
- SPEC-04-daemon.md：preset 命令面与载入编排引用本文件；快照时机后移。
- config.md：`[rvc]` 会话语义注记（live 草稿）；preset 命令引用。
- FUTURE.md：多预设、last-used、delete、命名字符集、设默认。
- SPEC.md：模块索引加 SPEC-11 行；变更记录加 2026-11-16 条目。
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
- 集成：`daemon/tests/preset.rs` 8 项通过，覆盖首次自动建、save/use/list、模型切换、实时 Stop/Start、跨链、损坏、加载失败保护与 NotReady。