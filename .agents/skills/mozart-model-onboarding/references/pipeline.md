# 导出与编译

命令、参数示例、实时生成步骤：见 [SETUP.md](../../../../docs/SETUP.md) §工具链（「调用示例」与「实时链路资产生成」两节）。本文只补 SETUP 没有的几点。

## 选哪条链

- 只要文件转换：动态生成器 + 动态特征引擎。
- 要实时：定长 front/decoder + 定长特征引擎。
- 两套形状不同，不能混用；特征引擎共享，导出编译一次。

## 版本

- torch 用 CPU 轮子（导出/golden 都在 CPU），避免 JetPack CUDA 版本不匹配。
- transformers 锁 4.46.3（5.x 用本地路径 `from_pretrained` 报错）。

## 产物改名（SETUP 未提）

`build_dynamic_engines.py` 输出固定占位名，成品目录要运行时名，需改名/复制：

- `qiqi-dynamic-explicit.engine` → `<id>.engine`
- `hubert-file.engine` → `hubert_base_dynamic.engine`（共享）
- `rmvpe-file.engine` → `rmvpe_dynamic.engine`（共享）

实时 `build_realtime_engines.py` 输出名已匹配运行时，无需改名。

## 已知缺口

- `export_rmvpe` 未设 `dynamic_axes`，文件链动态 RMVPE 导不出，需先补 `--dynamic`。
- 板端缺 `hubert_base/pytorch_model.bin`，重导 HuBERT 前要补齐（运行时已有 hubert engine，不影响推理）。
- 生成器/RMVPE 导出尚未端到端实跑，先跑一个最小导出（小 `--frames`）验证再批量。