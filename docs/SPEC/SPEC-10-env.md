# environment：环境管理（模型资产与工具链）

状态：讨论中。最后更新：2026-11-15。待办记法见 [SPEC.md](SPEC.md)。

## 目的

把模型资产的分层、命名、生命周期，以及导出/编译工具链收进统一规范。
设备目录总览与部署、测试操作见 [SETUP.md](../SETUP.md)；本规格只定义资产分层与命名。
工具链是仓库内的 Python 脚本，直接调用，不做 CLI 封装。
解决资产散落、命名不一、编译产出无配置的问题。

## 边界

- [x] 本规格 owns：模型资产分层与命名、模型生命周期、导出/编译工具链、资产配置。
- 运行时模型注册与加载契约归 [SPEC-03-rvc.md](SPEC-03-rvc.md)，本规格只引用。
- golden 验收门槛与记录归 [SPEC-08-verification.md](SPEC-08-verification.md)；对拍工具的实现归本规格。
- 代码构建与部署流水线归 [SPEC-09-pipeline.md](SPEC-09-pipeline.md)，不复述部署。
- 模型、音频、权重在仓外，不进仓库与流水线产物。

## 资产分层与命名

两层分离：原料层放待处理内容，成品层放可直接推理的整套资产。

- [x] 原料层 `~/rvc-pth/`：原始权重与下载后待本地处理的内容，按 `<id>.pth` 命名，可选配 `<id>.index`。板端已就位 7 个去重权重（qiqi-zh/en/ja/ko、mike-tyson、de-narrator、liuying）。
- [x] 成品层 `~/mozart-assets/models/rvc/<id>/`：目录名 = 模型 id = 文件前缀，已去掉 `-run` 后缀（板端 qiqi-zh / qiqi-en / de-narrator）。
- [x] 成品目录契约：`config.json` + `<id>.onnx`/`.engine`（离线生成器）+ `<id>-front.*`/`<id>-decoder.*`（实时，可选）+ `<id>.index`（可选）。加载器按 `<id>` 前缀找文件（`rvc/src/rvc/model_loader.cpp:103-120`）。
- [x] 引擎优先：同名 `.engine` 存在则优先于 `.onnx`（`rvc/src/rvc/model_loader.cpp:167-178`）。
- [x] 共享特征引擎（HuBERT/RMVPE）放一处，各模型引用，不每份复制。现共享于 qiqi-zh 目录，其余模型符号链接引用，`[rvc]` 全局路径显式指向它（`rvc/config/rvc_config.cpp:112-116`）。
- [x] 成品层资产自包含，不依赖旧仓库符号链接（de-narrator 资产已复制为真实文件）。

命名统一用小写连字符（`qiqi-zh`、`de-narrator`）。下划线变体（`qiqi_zh` 等）在迁移时归并。

## 工具链脚本

Python 脚本，居 `tools/model/`，直接调用。导出依赖 PyTorch，不能用 Rust 重写；编译是 trtexec 包装。

- [x] 导出 pth→onnx：`export_generator_onnx.py`、`export_generator_split_onnx.py`、`export_hubert_onnx.py`、`export_rmvpe_onnx.py`。`--rvc-source` 必填，指向 RVC 源码树。
- [x] 编译 onnx→engine：`build_dynamic_engines.py`。trtexec 须在目标设备执行，引擎绑设备。
- [x] golden 对拍：`run_reference.py`、`verify_golden.py`、`compare_streaming_quality.py`。`RVC_SOURCE` 环境变量必填。
- [x] 资产路径全部参数化：RVC 源码树（`--rvc-source`/`RVC_SOURCE`）、基础特征模型（`--hubert-path`/`--rmvpe-path`）、目标权重、golden 参考均由调用方注入，脚本不写死。外部依赖清单见 [SETUP](../SETUP.md)。

每模型只需编译自己的生成器（离线 1 个，实时再加 front/decoder 2 个）。特征引擎编译一次共享。

## 模型生命周期

下载 → 导出 → 编译 → 登记 → 测试。

- [ ] 下载：权重落入原料层 `~/rvc-pth/<id>.pth`。
- [ ] 导出：设备上运行导出脚本（PyTorch ARM64），产出 onnx。
- [ ] 编译：设备上运行编译脚本，产出生成器 engine。
- [ ] 登记：编辑 `[[models]]` 注册表（id、type=rvc、path），复用现有加载路径解析（`rvc/src/rvc/model_loader.cpp:262-292`）。
- [ ] 测试：`file submit --model <id>`，按需 golden 对拍（SPEC-08 验收）。

## 配置

- [x] 加载注册表优先，入口不硬拼 `models_dir/<id>`（`rvc/control/stream_session.cpp:450-465`，提交 09e396c）。
- [x] LoadModel 三态枚举：NotFound / Failed / Ok（`rvc/control/stream_session.h:71-73`）。
- [x] model not found 只在推理层运行时报错：NotFound → InvalidArgument（`rvc/control/stream_session.cpp:456-458`），不做复杂回退。
- [x] daemon 去掉模型存在预检，报错统一归推理层：入队原样收模型 id，执行时 LoadModel 失败标 failed 带原因、队列继续（`daemon/src/file/mod.rs:369-387`，回归 `daemon/tests/file_worker.rs` unknown_model_fails_at_load_and_queue_continues）。
- [x] model list 可选自动过滤资产缺失的模型（滤空）：CLI `model list --available` 只列 exists=true，缺省列全部；CLI 侧过滤，不改 ListModels 协议（`ctl/src/main.rs:584-613`）。运行时请求缺失模型再报错。
- [ ] 资产根与编译产出路径：资产根由 runtime 配置 `models_dir` 给定，编译输出走脚本参数。

## 待决策

- 无。导出与编译均在 Jetson 上执行（PyTorch ARM64 JetPack 轮子 + trtexec），无跨机编排。golden 对拍工具实现归本规格，验收门槛归 SPEC-08（边界节已定）。

## 验收

- [ ] 新增一个音色：按生命周期导出、编译、登记，`model list` 可见，`file submit --model` 可转换。
- [ ] 原料层与成品层命名一致，成品目录自包含。
- [ ] 未知模型与加载失败分别回 InvalidArgument 与 InferenceFailed。