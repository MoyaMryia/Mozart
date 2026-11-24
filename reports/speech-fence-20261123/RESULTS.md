# 播报停止屏障与模式往返本机验证

## 结论

daemon 的 `speech stop` 在桥撤销播报资格、TTS 确认截止值、pre 确认播放帧边界后才返回成功。旧代 PCM 即使在重新开启后到达，仍被 pre 拒绝。停止对账未完成时拒绝重新开启，已确认的桥关闭状态按现有机制持久化。

## 本机结果

使用真实 daemon、pre、ASR、bridge 和 TTS 进程，ASR 与 TTS 用确定性推理桩，翻译用本机 Unix HTTP 应答桩，RVC 用控制协议桩。播放使用 ALSA `null`，音频通过 ASR 输入和播报数据面注入，不经物理麦克风。

1. 注入第一句，等待旧代翻译请求到达，再暂停应答。
2. 执行严格停止，收到截止值。切到实时变声，再切回直通并开启播报。
3. 恢复第一句翻译应答。旧句译文落盘，无播报帧进入播放。
4. 向同一播报 socket 注入 4 个迟到的旧代 PCM 包；播放计数仍为零。
5. 注入第二句。两句均发布原文及译文；TTS 只受理并生成第二句，pre 播放 5 帧。ASR 启动号从步骤 1 到结束未变。

本机证据：`/tmp/mozart-speech-fence.VSrEs1tM/end_to_end.py`、`end-to-end.log`，最终运行日志与字幕位于 `/tmp/mozart-fence-e2e-76xousbh/`。此前试跑发现监督器在启动时清理预先绑定的翻译 socket，以及 pre 控制连接由 daemon 持有；最终脚本在监督器就绪后绑定应答桩，并用 daemon 状态读取 pre 快照。

自动回归分别验证线包时间戳、TTS 拒绝延迟旧任务且接受新代、pre 播放屏障，以及 TTS 缺席时 daemon 不返回成功且禁止重新开启。每个模块内的旧版停止路径只保留原有兼容语义，不作为 daemon 的严格停止证据。

## 统一模式与复核恢复补测

相同本机配置改用 `SetOutputMode` 依次进入 `broadcast → rvc → direct → broadcast`，每次等待 `output_mode.applied` 实际生效。旧句和新句均落盘译文，旧播报和 4 个迟到 PCM 包未播放，第二句播放 5 帧。ASR 启动号在整段转移中一致。证据：`/tmp/mozart-output-mode.jiW28Ale/end_to_end.py`、`end-to-end-final.log`，最终运行日志位于 `/tmp/mozart-fence-e2e-2mr5htaz/`。

独立进程测试在待复核原文已落盘后强制停止桥，再启动同一存档的桥。验证它追加一次中断修订、只翻译在线原文、忽略迟到复核且不发起 TTS 连接。单测覆盖 30 s 单调时限临界点、重复恢复、破损末行及有界尾窗。

## 适用范围

旧控制命令与统一模式各运行一次，每次两句、4 个迟到旧包。未运行真实推理或声学测试；需在 Jetson 使用实际模型和外放设备后再验收听感及设备尾音。`speech stop` 不撤回已送入 ALSA 的采样点。桥恢复仅覆盖规格定义的 JSONL 尾窗，不代表 ASR 断连事件有持久化送达保证。
