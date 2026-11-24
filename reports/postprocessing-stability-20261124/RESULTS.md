# 后处理与持久化稳定性回归

## 结果

| 注入场景 | 本机验收 |
| --- | --- |
| RVC 启动期发送 `valid_samples = 0` 的 48 kHz 帧 | `pre` 进入欠载淡出，不索引无效 PCM；ALSA `null` 下发送 16 帧后播放仍推进，进程正常退出。 |
| `queue.json.tmp` 不可写 | 提交、暂停、恢复、取消、开始处理和进度未泄漏未落盘的状态。移除故障后原任务可继续。 |
| 任务已完成但终态写盘失败 | 不驱逐输入和结果，RVC 租约独立归还，不启动下一文件任务。写盘恢复后提交同一终态，不重复推理；关停若先于重试，重启从已持久化状态重新排队。 |
| 状态文件无法原子替换或删除 | 保留覆盖待重试；`config save` 明确报告基线已写、覆盖未清。重新保存成功后清除覆盖和降级提示。失败的替换不残留本次临时文件。 |
| RVC 音频 socket 对端缺失、路径超长 | 各连续注入 12 次，未留下新输入绑定，进程描述符数量不增加。随后合法初始化成功。 |

## 复现

运行 `tools/ci-local.sh all`。故障用例位于 `pre/tests/playback_routing.rs`、`daemon/src/file/queue.rs`、`daemon/src/file/mod.rs`、`daemon/src/state_file.rs`、`daemon/tests/config_state.rs` 和 `tests/test_rvc_transport.cpp`。

## 适用范围

x86_64，本机协议桩、ALSA `null` 和临时目录。每项使用一套故障夹具；socket 初始化失败分别重复 12 次。写盘故障在原子替换提交前注入，不代表断电恢复验证；断电验收需可控制断电并留存文件系统证据的隔离环境。真实 Jetson 推理、声卡播放及欠载次数按 [SPEC-02](../../docs/SPEC/SPEC-02-pre.md) 的板端窗口验收，本轮不部署。
