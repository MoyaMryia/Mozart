# L2 意图塑形器板端部署实测（MozartMryia 隔离部署）

日期：2026-11-28。设备：Jetson Orin Nano Super 8 GB（jetson-wg）。部署形态：`~/MozartMryia` 隔离开发树，canonical 栈（systemd `mozart.service`）构建窗口内暂停、结束后恢复并探活。

## 部署内容

- 代码：本地 Mozart 仓库 `84cb033+`（SPEC-03 步骤 6 sculptor 接入）同步到 `~/MozartMryia/src`：整树 `rvc/`、`core/include/`（纯增量头文件）、`tests/`（rvc 相关 8 个测试 + sculptor 测试与夹具）、`tools/model/`（导出脚本 + 契约 JSON）、`docs/SPEC/SPEC-03-rvc.md`。测试注册追加到设备现有 `tests/CMakeLists.txt`（不覆盖 ulunas 分支的 enhance 测试）。
- 资产：`sculptor.onnx`（17.5 MB，单输出 `[1,280,771]` 帧主序）→ 板端 `trtexec --fp16` 编译为 `sculptor.engine`（9.9 MB），放入 `frieren-v2-40k` 与 `qiqi-zh` 模型目录（附加文件，含 `sculptor.json`：gender=1、target_f0=228、std=2.6、softness=0.7）。qiqi 的 MM 验证走 `/tmp/mozart-realtime-mm/models/qiqi-zh` 符号链接集（需重建链接后 sculptor 才被加载）。

## 实测结果

| 项 | 结果 |
| --- | --- |
| 板端 CTest（jetson-release，13 项） | 100% 通过，含 `sculptor_intent` 黄金对拍（max_diff=0，与 features.py 逐位一致） |
| TRT 编译 | `sculptor.engine` 9.9 MB；trtexec GPU Compute median **1.31 ms**（P95 1.32 ms），预算 5–10 ms/块 |
| 实时多模型 E2E（jetson-realtime-mm.sh，隔离二进制） | **9/9 场景 PASS**（48k 实时基线、overlap 定值拒绝、startup_delay 可调、跨采样率 model use、Busy 回复、40k 文件链、资产无残留） |
| 状态机 | `GetStatus`/`LoadModel` capabilities 含 `sculptor` 位；日志 `Sculptor enabled for qiqi-zh (gender=1, target_f0=228, std=2.6, softness=0.7)`；frieren 无 sculptor 时 caps 不含该位，行为不变 |
| 流健康 | 实时块 frames_in == frames_out、`frames_dropped=0`、`last_error=None` |
| canonical 恢复 | systemctl start 后 daemon/pre 正常，route=direct，rvc idle |

无 sculptor 时全链路回归不变（MM 全部场景与旧二进制口径一致）。

## 过程记录

- `build-isolated.sh` 的进程预检查与 systemd 托管 daemon 冲突（`mozart-enhance` 常驻导致预检查必然拒绝）；本次绕过脚本，手动执行等价步骤：systemctl stop → cargo/cmake 构建 → 安装隔离 bin → 测试 → systemctl start。脚本适配 systemd 留待后续。
- `frieren-v2-40k` 板端实时前端资产不健康：`rmvpe-realtime.onnx` 的外部数据 `rmvpe-realtime.onnx.data` 缺失、`hubert-realtime.onnx` 无配套 `[1,44800]` TRT engine。根因：共享前端以符号链接指入 qiqi 目录，但配对的 `.engine` 与 `.data` 未随链接。已修复：frieren 目录补三个符号链接；复测 MM（frieren 为 A 模型）9/9 PASS，frieren 40k 实时块 avg `71.7 ms`（静音流，样本 3 块）、零丢帧零错误，sculptor 双模型生效。
- MM 模型目录为符号链接集，运行期间增删资产目录文件不会反映到链接集，需重建符号链接。

## 适用范围

- 配置：JetPack 6 / TensorRT 10.16.2，Jetson 定频未确认（构建窗口外 canonical 在跑）。
- 实时 E2E 音源为 aloop 静音流（skip_silence=false），sculptor 在真实语音上的板端听感与单块耗时增量未实测——静音流方差大（qiqi 55–58 ms、frieren 71.7 ms 仅 3 块样本），sculptr 引擎自身 trtexec 实测 `1.31 ms`；需接真 mic 或语音 aloop 播放对拍后记录。
- sculptor 权重为 M2F（frieren 目标）训练口径，qiqi-zh（48k 女声）上仅验证链路功能与状态上报，音色匹配度未评估。
- canonical 栈为 bocchi 模型，未切 sculptor 路径；正式启用需在 bocchi（或其他目标模型）目录放置 sculptor 三件套。
