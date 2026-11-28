# AEC 客观修复板端验证与声学实录

## 结论

AEC 客观对齐、联动与诊断加固（提交 `d48fcfa`）在 Jetson 板端通过单元与功能套件验证；ALSA Loopback 纯 AEC3 回声衰减 32.27 dB（前次部署验证实测）。第三轮物理声学实录取得有效回声样本：物理耦合已建立，远端单讲稳态实测声学 ERLE 18.73 dB，链路计数干净；多房间、双讲与调参仍待完成（SPEC-02 步骤 1 未关闭）。实录同时暴露延时估计口径差异（注入 45 ms / 互相关 71 ms / WebRTC 内部 112 ms），留作步骤 1 调参输入。板端存在三处与本轮改动无关的既有测试时序问题（journal 内核时间戳粒度竞态、daemon 用例收尾竞态、文件取消后 GPU 释放滞后采样），均已定位根因，未改代码。

## 配置与口径

- 提交：`d48fcfa`（降噪后端热切换与 AEC 对齐加固，新增声学录测工具），工作区干净。
- 设备：Jetson Orin Nano Super 8 GB，JetPack 6，`moyamryia@10.0.0.4`；板端 `~/Mozart` 源码与本地关键文件 MD5 一致（`pre/src/aec.rs`、`pre/src/main.rs`、`aec3/src/mozart_aec3.cpp`、`core/src/ctl/schema.rs`），未重新部署。
- 声卡：采集 `plughw:CARD=MIC,DEV=0`（HK MIC，硬件增益 100% / +15.6 dB），播放 `plughw:CARD=Device,DEV=0`（USB PnP Sound Device，PCM 80% / -5.69 dB）。
- 声学实录：`tools/jetson-aec-acoustic.sh`，每轮 12 s 真实法语 TTS 语音外放，AEC 关/开两轮，禁用降噪/AGC/限幅/音量门；ERLE 取 16 kHz 输出轨第 4 s 至结尾稳态 RMS 之比；对齐与漂移由 `--aec-probe` + `pre/aec-probe/align.py` 估算。
- 板端功能套件：`tools/jetson-e2e.sh --smoke --file --language`；单元为 `cargo test --workspace -- --test-threads=1`（链接 `libmozart_aec3.so`）与 `ctest --test-dir build/jetson-release`。
- 服务管理：测试前板端运行时挂在 SSH session scope（前次会话遗留），已归回 `mozart.service`（systemd，enabled）；功能套件由脚本自动停服/恢复。

## 本机验证

`tools/ci-local.sh all` 通过（exit 0）：

| 阶段 | 结果 |
| --- | --- |
| repo / lint | 通过（379 个跟踪文件） |
| native（host-debug） | CTest 13/13 通过 |
| rust / adapter | 全部通过（含 `has_aec3` 真库用例） |
| web | Vitest 77/77 通过，`vue-tsc --noEmit && vite build` 通过 |

提交经 pre-commit hook（`ci-local.sh --staged`）复核通过。

## 板端验证

- CTest（jetson-release）：13/13 通过（含 `aec3_wrapper`）。
- 冒烟（`jetson-smoke.sh`）：通过。三进程拉起、RVC 路由、kill -9 自愈、直出切回、停服无残留均断言通过。
- 单元（`cargo test --workspace --no-fail-fast -- --test-threads=1 --skip same_size_identical_rewrite_resets`，链接真库）：42 个测试二进制共 843 项通过、0 失败、1 项既有忽略（`uploads::tests::mock_peer`）；journal 竞态用例按设计跳过。含 `mozart-pre` 120 单元 + 8 集成（`real_aec3_cancels_65ms_echo_with_100ppm_drift`、`real_aec3_recovers_after_midstream_delay_jump`）、`mozart-daemon` 115 单元 + 18 + 11 集成（含 `file_worker` 17 项）、`ctl` 15 + 37、`bridge` 159。
- 文件链路（`jetson-file-test.sh`）：8/9 通过，`d-cancel` 失败（见下）。
- 语言链路（`jetson-language-test.sh`，smoke 模式）：通过，`required=9/9`；advisory 1/12：`G1_over_split_short_frag` 短碎片率 18.2%（2/11，限 15%），属 ASR 断句口径，与本轮 AEC 改动无关。
- 控制面契约（`GET /api/v1/status`）：`pre.aec` 上报 `erl_db`、`residual_echo_likelihood`、`stream_delay_ms` 等新字段（发散/延时中位数/标准差在不可用时按设计省略）；`pre.denoise_backend=ulunas`、`denoise_backends` 动态目录（wola/ulunas 均 available）正常；`degraded=[]`。

### 既有测试竞态（非本轮回归，相关文件均不在 diff 内）

1. `web-adapter` `journal::tests::same_size_identical_rewrite_resets`：内核时间戳粒度问题，见下节。
2. `daemon` `file_worker::pre_restart_during_file_job_recovers_and_completes`：用例自身 daemon 收尾与状态轮询竞态，失败断言为 `file status: daemon is shutting down`（`daemon/tests/common/mod.rs:261`）。前次会话同代码板端通过；本轮单独重跑通过（1 passed，15.30 s）。
3. 文件套件 `d-cancel`：job 已达 `cancelled`（终态断言通过），但 GPU 仲裁释放滞后于终态——控制面恢复耗时 6 s（rvc 停在整段推理检查点才退出），脚本在 `wait_job` 返回后立即采样 `arbiter.gpu.owner` 断言为空，读到 `file`（`st-d-after.json` `file_return_pending=true`）。本轮两次运行同一断言同一结果，属确定性时序问题而非偶发；取消功能本身（终态、残留清理、控制面恢复）断言均通过。前次会话同一断言同一瞬态。

### journal 单测竞态（既有问题，非本轮回归）

`web-adapter/src/journal.rs` 的 `same_size_identical_rewrite_resets` 在板端稳定失败。该文件不在本轮 diff 内。

- 机制：`unchanged()` 以 dev/ino/len/mtime（秒+纳秒）/ctime（秒+纳秒）判定同长重写；内核文件时间戳走粗粒度时钟，每 tick 更新一次。板端 `CONFIG_HZ=250`（4 ms tick），两次写入落在同一 tick 时时间戳完全相同，generation 不复位。
- 证据：板端实测 200 次同长快速重写，192 次 mtime+ctime 纳秒完全一致；该用例单独重跑仍失败（断言两侧 generation 哈希相同）。
- 影响：250 HZ 内核上 4 ms 内的同长重写不被字幕 journal 察觉。属既有设计限制，修复需改测试或判定逻辑，本轮按指示不改代码。

## 声学实录

### 前两轮（2026-11-28 15:30 / 15:37，前次会话）

| 指标 | 15:30 轮 | 15:37 轮 |
| --- | ---: | ---: |
| 稳态 RMS，AEC 关 | 2.2e-05 | 2.4e-05 |
| 稳态 RMS，AEC 开 | 2.5e-05 | 2.5e-05 |
| 实测声学 ERLE | -1.07 dB | -0.66 dB |
| 采集覆盖率 | 99.83% | 99.82% |
| 采播相对漂移 | -0.353 ppm | +0.317 ppm |
| 互相关估计声学延时 | 176.5 ms | 193.83 ms |
| 运行时 stream_delay_ms | 43 ms | 44 ms |
| erl_db | -30.0 dB | -30.0 dB |

两轮输出峰值约 -73 dBFS，接近芯片底噪；`erl_db = -30 dB` 为下限钳位值，表明参考与采集之间没有可测回声耦合。负 ERLE 无物理意义，仅反映底噪波动。

### 物理耦合排查（前次会话）

- ALSA 混音器未静音：播放 PCM 80% [on]，采集 Mic 100% [on]，AGC [on]。
- 原生对照：`aplay` 外放 3 s 440 Hz 正弦（-6 dBFS）同时 `arecord` 直录，录到 RMS 4.47e-05、峰值 0.00027，仍为底噪。排除 mozart-pre 与驱动因素。
- 结论：问题在物理外设侧——外放音箱未通电/音量关闭/误插耳机口，或 HK MIC 物理静音键闭合，或两者未摆放在一起。需人在设备旁确认。第三轮（20:05）耦合已建立，外设侧问题已解除。

### 第三轮（本轮，2026-11-28 20:05）

物理耦合已建立（扬声器外放被麦克风拾取），取得首份有效声学样本。输出：`assets/out/20261128-200520-aec-acoustic/`。

| 指标 | 数值 |
| --- | ---: |
| 稳态 RMS，AEC 关（4 s 至结尾） | 0.00783 |
| 稳态 RMS，AEC 开（4 s 至结尾） | 0.000906 |
| 实测声学 ERLE（远端单讲） | 18.73 dB |
| erl_db（终值） | +20.0 dB（上限钳位） |
| 采集覆盖率 | 99.83% |
| 采播相对漂移 | -0.618 ppm |
| 时钟拟合残差 P95，采集／播放 | 0.844／0.626 ms |
| 互相关估计物理声学延时 | 71.08 ms |
| 注入 stream_delay_ms | 45 ms |
| WebRTC 内部 delay_ms（终值） | 112 ms |
| ref_dropped ／ ref_underruns ／ resets | 0 ／ 1（首帧）／ 1 |
| playback_underruns ／ capture_clips | 0 ／ 0 |

收敛过程（`aec_on_timeline`，1 s 采样）：`erl_db` 自 8 s 起 4.89 → 8.82（9 s）→ 17.89（10 s）→ 20.0 dB（11 s 起），约 10 s 建立耦合估计；内部 `erle_db` 在 0.18–4.57 dB 间波动，终值 1.66 dB。

口径与发现：

- 实测 ERLE 18.73 dB 是输出轨消除前后稳态能量比，含 AEC3 非线性抑制器作用；WebRTC `GetStatistics` 的 `erle_db` 只反映线性滤波器估计，两者口径不同，不能互替（推断，基于字段语义与本轮数据差异）。
- 延时三个口径不一致：注入 `stream_delay_ms=45 ms`（ALSA 硬件路径配对）、互相关 71.08 ms（含声传播与外设缓冲）、WebRTC 内部估计 112 ms。差值表明硬件路径配对未覆盖扬声器/麦克风附加缓冲与声传播延时；AEC3 内部延迟估计器自行补偿后仍取得 18.73 dB 衰减。此为步骤 1 调参输入，本轮不改代码。
- 本轮为远端单讲（脚本仅播放 TTS，无人声近端），不构成双讲近端保真证据；单一房间单一摆放，不覆盖换房/挪动重收敛。
- `divergent_fraction` 全程不可用（WebRTC 未置 has 标志），面板发散提示在该场景无数据。

## 审查遗留项

前次会话三个只读审查子代理的结论已逐项核对当前代码：

- AEC 审查 2 High / 5 Medium：已修复。High：`pre/src/main.rs:308` 工作项代次改为 `epoch.wrapping_add(captured.capture_epoch)`，与 `denoise_epoch()` 口径对齐；参考环稳态水位 `REF_TARGET_BACKLOG=1` / `REF_MAX_BACKLOG=2`（`pre/src/aec.rs:16-17`），单帧漂移温和抽丢、突发积压回退重置。Medium：xrun 先 `discontinuity` 再推帧（`pre/src/playback.rs:501-503`）；采集 overrun 清半帧（`pre/src/capture.rs:169`）；AEC 故障只记 `aec.last_error`，回调仅打日志（`pre/src/main.rs:295-297`）；ci 路径含 `aec3/` 与两个板端脚本（`tools/ci-local.sh:12-13`）；增益按基线累计跳变判定且读序对称（`pre/src/main.rs:277-279`，`pre/src/control.rs` 单测覆盖）。Low 项 i128 延时差、`last_stream_delay_ms` 欠载回退亦已落地。
- 降噪审查遗留 1 项（Medium，诊断性）：`pre/src/denoise/ulunas.rs:81` 在 `[enhance]` 配置下，Wola/Off 模式的队列丢帧/跳变仍会记录 `enhancement input gap; session reset` 到 `enhance.last_error`；模式切换已清旧故障（`ulunas.rs:84-86`），但本地模式运行期间的队列抖动仍可能留下误导性告警。本轮不改。

## 未验证项与阻塞

| 事项 | 阻塞原因 | 解除条件 |
| --- | --- | --- |
| SPEC-02 步骤 1 多房间声学实录与调参 | 本轮仅单一房间单一摆放远端单讲；双讲、换房/挪动重收敛、SpeexDSP 对照与延时口径调参未做 | 按步骤 1 录 2–3 种摆放与双讲样本，核验延时三口径差异并定参 |
| SPEC-02 步骤 4 主观听感验收 | 需人耳双讲/换房实听 | 声学样本补齐后人工试听 |
| 硬件 xrun 注错瞬态恢复 | 未人为注入硬件溢出 | 板端独占声卡窗口注错并观察重置计数 |
| 降噪三档热切换板端实听 | 需真实麦克风与人耳 | 部署后 Direct/RVC 路由三档热切换实听 |
| 播放缓冲余量 30 s 欠载为 0 | 本轮未重测 | 同一输入 30 s 板端采播窗口重测 |

Loopback 纯 AEC3 ERLE 32.27 dB、待机免虚计、热接入重置、负增益削波统计为前次会话（2026-11-28 15:20 部署验证）实机实测，本轮未重复；环回不替代声学验收。

## 适用范围

- 本机 x86_64（host-debug）与 Jetson ARM64（jetson-release），提交 `d48fcfa`，单次运行。
- 声学实录 3 轮 × 2 轨 × 12 s：前两轮同一房间同一摆放均为底噪样本，不构成 ERLE 证据；第三轮耦合建立，远端单讲、单一摆放，ERLE 18.73 dB 仅代表该配置稳态能量比。
- journal 竞态证据为 200 次重写采样，板端 ext4 / `CONFIG_HZ=250`。
- 未验证项见上表；主观听感、双讲与多房间覆盖不在本轮范围。