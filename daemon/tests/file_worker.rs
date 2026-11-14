//! 文件数据面集成测试：真实 daemon + 真 ffmpeg + rvc 桩整段批处理。
//! 覆盖端到端发布（内容可核对）、降噪帧数、时长上限、ffmpeg 缺失/超时、
//! 执行中取消。桩的批处理延迟旋钮让取消用例稳定停在 infer 段。

mod common;

use common::{
    call, file_job, status, submit_file, wait_job_state, wait_ready, wait_until, DaemonProcess,
};
use mozart_core::ctl::schema::FileJobState;
use mozart_core::ctl::{self, Seqpacket};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 数据面用例等待上限：真 ffmpeg + DSP + 桩批处理，主机上远小于此。
const JOB_TIMEOUT: Duration = Duration::from_secs(20);

/// 状态收敛等待（轮询间隔 100 ms）。
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// 喂帧停住的观察窗：大于 worker 的进度节流间隔（250 ms），
/// 仍在推进的喂帧必然在窗内落盘一次进度。
const FEED_STALL_WINDOW: Duration = Duration::from_millis(800);

/// 取消打断阻塞喂帧后的收尾上限：打断本身在一个等待分片（100 ms）量级。
const CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

struct Harness {
    daemon: DaemonProcess,
    conn: Seqpacket,
    storage: PathBuf,
}

impl Harness {
    fn new(tag: &str, file_extra: &str, extra_env: &[(&str, String)]) -> Self {
        let storage = std::env::temp_dir().join(format!(
            "mozart-file-worker-{}-{tag}",
            std::process::id()
        ));
        let daemon = DaemonProcess::start_file(tag, &storage, file_extra, true, extra_env);
        let conn = daemon.connect();
        wait_ready(&conn);
        Self {
            daemon,
            conn,
            storage,
        }
    }

    fn submit(&self, name: &str, seconds: u32, sample_rate: u32) -> (String, Vec<i16>) {
        let source = self.daemon.dir.join(name);
        let pcm = write_wav(&source, seconds, sample_rate);
        let reply = submit_file(&self.conn, &source, Some("demo"));
        assert_eq!(reply.status, "ok", "submit: {}", reply.reason);
        (reply.data["job_id"].as_str().unwrap().to_string(), pcm)
    }

    fn result(&self, job_id: &str) -> PathBuf {
        self.storage.join(format!("{job_id}.output.wav"))
    }

    /// 临时文件（decoded.f32 / output.f32 / .part）在任何收尾后都不留。
    fn assert_temp_clean(&self, job_id: &str) {
        for name in [
            format!("{job_id}.decoded.f32"),
            format!("{job_id}.output.f32"),
            format!("{job_id}.output.wav.part"),
        ] {
            assert!(!self.storage.join(&name).exists(), "临时文件残留：{name}");
        }
    }

    fn assert_gpu_free(&self) {
        assert!(
            wait_until(POLL_TIMEOUT, || status(&self.conn).arbiter.gpu.owner.is_none()),
            "任务收尾后 GPU 必须释放"
        );
    }

    /// 桩的批处理计数经 daemon 每秒轮询汇总到 status.rvc。
    fn assert_batch_counts(&self, frames_in: u64, frames_out: u64) {
        assert!(
            wait_until(POLL_TIMEOUT, || {
                status(&self.conn).rvc.is_some_and(|rvc| {
                    rvc.frames_in == frames_in && rvc.frames_out == frames_out
                })
            }),
            "桩批处理计数应为 {frames_in} 入 / {frames_out} 出：{:?}",
            status(&self.conn).rvc
        );
    }

    /// 喂帧已阻塞在满缓冲上的证据：stage 进入 feed 后进度不再推进。
    /// 桩持有输入 socket 但永不读帧，缓冲填满后发送方只能等待。
    fn assert_feed_stalled(&self, job_id: &str) {
        assert!(
            wait_until(JOB_TIMEOUT, || {
                file_job(&self.conn, job_id).progress.stage == "feed"
            }),
            "任务应进入 feed 段"
        );
        let percent = file_job(&self.conn, job_id).progress.percent;
        let moved = wait_until(FEED_STALL_WINDOW, || {
            file_job(&self.conn, job_id).progress.percent != percent
        });
        assert!(!moved, "喂帧应阻塞在满缓冲上，进度不应推进");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.storage);
    }
}

#[test]
fn plain_job_publishes_wav_with_converted_audio() {
    let h = Harness::new("worker-plain", "", &[]);
    // 1 s 16 kHz：解码不重采样，样本值可逐一对拍。
    let (job_id, pcm) = h.submit("tone.wav", 1, 16000);
    let job = wait_job_state(&h.conn, &job_id, FileJobState::Completed, JOB_TIMEOUT);
    assert!(job.result_ready, "completed 必须附结果可取标记");
    assert_eq!(job.progress.stage, "publish");
    assert_eq!(job.progress.percent, 100);
    assert!(job.error.is_none());

    let wav = read_wav(&h.result(&job_id));
    assert_eq!(wav.format, 3, "输出固定 IEEE float WAV");
    assert_eq!(wav.channels, 1);
    assert_eq!(wav.sample_rate, 48000);
    assert_eq!(wav.samples.len(), pcm.len() * 3, "输出时长与输入一致");
    // 桩为零阶保持 3:1 上采样：内容逐样本核对，证明帧真的过了数据面。
    for (i, sample) in wav.samples.iter().enumerate().take(960) {
        let expect = f32::from(pcm[i / 3]) / 32768.0;
        assert!(
            (sample - expect).abs() < 1e-6,
            "样本 {i}: {sample} != {expect}"
        );
    }
    // 1 s = 50 个 320 样本契约帧；桩收 50 发 50。
    h.assert_batch_counts(50, 50);
    h.assert_temp_clean(&job_id);
    h.assert_gpu_free();

    // 取结果：daemon 回受管结果绝对路径与缺省名（源文件名主干 + .wav），不复制。
    let body = json!({"job_id": job_id});
    let reply = call(&h.conn, ctl::DAEMON_FILE_FETCH, body.to_string().as_bytes());
    assert_eq!(reply.status, "ok", "fetch: {}", reply.reason);
    assert_eq!(reply.data["job_id"], job_id);
    assert_eq!(reply.data["name"], "tone.wav");
    let output_path = PathBuf::from(reply.data["output_path"].as_str().unwrap());
    assert!(output_path.is_absolute());
    assert_eq!(output_path, h.result(&job_id).canonicalize().unwrap());

    // 结果被驱逐（文件不在）→ NotReady，即便 result_ready 仍为 true。
    std::fs::remove_file(h.result(&job_id)).unwrap();
    let reply = call(&h.conn, ctl::DAEMON_FILE_FETCH, body.to_string().as_bytes());
    assert_eq!(reply.code, ctl::CODE_NOT_READY);
    assert!(reply.reason.contains("missing"), "{}", reply.reason);
}

#[test]
fn denoise_job_packs_16k_contract_frames() {
    let h = Harness::new("worker-denoise", "denoise = true", &[]);
    // 48 kHz 源：denoise 路径解码不重采样，主干帧数确定。
    let (job_id, _pcm) = h.submit("tone.wav", 1, 48000);
    let job = wait_job_state(&h.conn, &job_id, FileJobState::Completed, JOB_TIMEOUT);
    assert!(job.result_ready);

    let wav = read_wav(&h.result(&job_id));
    assert_eq!(wav.sample_rate, 48000);
    // 48000 个 48 kHz 样本 → 50 个 960 样本主干帧 → 50 个 320 样本契约帧
    // → 50 个 960 样本输出帧 = 48000 样本。
    assert_eq!(wav.samples.len(), 48000);
    h.assert_batch_counts(50, 50);
    h.assert_temp_clean(&job_id);
    h.assert_gpu_free();
}

#[test]
fn over_duration_source_is_rejected_after_decode() {
    let h = Harness::new("worker-limit", "max_duration_seconds = 1", &[]);
    let (job_id, _pcm) = h.submit("long.wav", 2, 16000);
    let job = wait_job_state(&h.conn, &job_id, FileJobState::Failed, JOB_TIMEOUT);
    let error = job.error.expect("失败必须带可查询原因");
    assert!(error.contains("duration limit"), "{error}");
    assert!(!job.result_ready);
    assert!(!h.result(&job_id).exists());
    h.assert_temp_clean(&job_id);
    h.assert_gpu_free();
}

#[test]
fn missing_ffmpeg_fails_with_reason() {
    let h = Harness::new(
        "worker-no-ffmpeg",
        "ffmpeg_path = \"/nonexistent/mozart-ffmpeg\"",
        &[],
    );
    let (job_id, _pcm) = h.submit("tone.wav", 1, 16000);
    let job = wait_job_state(&h.conn, &job_id, FileJobState::Failed, JOB_TIMEOUT);
    let error = job.error.expect("失败必须带可查询原因");
    assert!(error.contains("mozart-ffmpeg"), "{error}");
    h.assert_temp_clean(&job_id);
    h.assert_gpu_free();
}

#[test]
fn ffmpeg_timeout_fails_with_reason() {
    // 1 ms 上限：ffmpeg 启动都来不及完成，必然走超时杀进程分支。
    let h = Harness::new("worker-ffmpeg-timeout", "ffmpeg_timeout_ms = 1", &[]);
    let (job_id, _pcm) = h.submit("tone.wav", 1, 16000);
    let job = wait_job_state(&h.conn, &job_id, FileJobState::Failed, JOB_TIMEOUT);
    let error = job.error.expect("失败必须带可查询原因");
    assert!(error.contains("timeout"), "{error}");
    h.assert_temp_clean(&job_id);
    h.assert_gpu_free();
}

#[test]
fn cancel_during_inference_stops_stream_and_cleans() {
    // 桩攒满帧后等 4 s 再回帧：取消稳定落在 infer 段。
    let h = Harness::new(
        "worker-cancel",
        "",
        &[("MOZART_RVC_STUB_FILE_DELAY_MS", "4000".to_string())],
    );
    let (job_id, _pcm) = h.submit("tone.wav", 1, 16000);
    assert!(
        wait_until(JOB_TIMEOUT, || {
            file_job(&h.conn, &job_id).progress.stage == "infer"
        }),
        "任务应进入 infer 段"
    );

    let body = json!({"job_id": job_id});
    let reply = call(&h.conn, ctl::DAEMON_FILE_CANCEL, body.to_string().as_bytes());
    assert_eq!(reply.status, "ok", "cancel: {}", reply.reason);
    assert_eq!(reply.data["state"], "cancelling");

    let job = wait_job_state(&h.conn, &job_id, FileJobState::Cancelled, JOB_TIMEOUT);
    assert!(!job.result_ready);
    assert!(!h.result(&job_id).exists());
    h.assert_temp_clean(&job_id);
    assert!(
        !h.storage.join(format!("{job_id}.input.wav")).exists(),
        "取消收尾必须删源副本"
    );
    h.assert_gpu_free();
    // 流已停：rvc 不再报 streaming（StopStream 已下发）。
    assert!(
        wait_until(POLL_TIMEOUT, || {
            status(&h.conn)
                .rvc
                .is_some_and(|rvc| rvc.state != "streaming")
        }),
        "取消后 rvc 流必须停下"
    );
}

/// 回归：rvc 活着但挂死（绑定输入 socket 却永不读帧），喂帧填满缓冲后阻塞。
/// 取消必须打断阻塞等待并走完收尾。旧实现 poll(-1) 无限等待，取消检查点只在
/// 帧边界，任务永远停在 cancelling，GPU 与队列被楔住。
#[test]
fn cancel_interrupts_feed_when_peer_hangs() {
    let h = Harness::new(
        "worker-feed-hang",
        "",
        &[("MOZART_RVC_STUB_FILE_HANG", "1".to_string())],
    );
    // 2 s = 100 契约帧，远多于数据面缓冲：喂帧必然停在满缓冲上。
    let (job_id, _pcm) = h.submit("tone.wav", 2, 16000);
    h.assert_feed_stalled(&job_id);

    let body = json!({"job_id": job_id});
    let reply = call(&h.conn, ctl::DAEMON_FILE_CANCEL, body.to_string().as_bytes());
    assert_eq!(reply.status, "ok", "cancel: {}", reply.reason);
    assert_eq!(reply.data["state"], "cancelling");

    let job = wait_job_state(&h.conn, &job_id, FileJobState::Cancelled, CANCEL_SETTLE_TIMEOUT);
    assert!(!job.result_ready);
    assert!(!h.result(&job_id).exists());
    h.assert_temp_clean(&job_id);
    assert!(
        !h.storage.join(format!("{job_id}.input.wav")).exists(),
        "取消收尾必须删源副本"
    );
    h.assert_gpu_free();
    // 停流先于解绑：rvc 不再报 streaming。
    assert!(
        wait_until(POLL_TIMEOUT, || {
            status(&h.conn)
                .rvc
                .is_some_and(|rvc| rvc.state != "streaming")
        }),
        "取消后 rvc 流必须停下"
    );
}

/// 回归（长文件数据面死锁）：pre 恒发实时帧到 rvc-in.sock（SPEC-02「AI 路径
/// 永不停」）。SPEC-03 复用帧数据面 + 状态门控交接：文件任务起流前 daemon 经
/// PRE_SET_FILE_MODE{active:true} 让 pre 暂停 rvc 发送并释放输出 socket，实时帧
/// 不混入整段攒帧；收尾恢复。本用例用带恒发旋钮的 pre 桩（MOZART_PRE_STUB_FLOOD）
/// 模拟实时灌帧：若 daemon 未暂停 pre，实时帧混入攒帧，任务死锁超时（板端 295 s
/// 实测形态）。旧独立 socket 方案已回退，本用例守护状态门控交接。
#[test]
fn realtime_flood_on_shared_input_does_not_wedge_file_job() {
    // 桩攒帧逐帧节流 2 ms：拉宽攒帧窗（1000 帧约 2 s）；pre 桩恒发旋钮模拟
    // 实时支路持续向共享 rvc-in.sock 灌帧（惰性重绑，跟随桩起流重绑）。
    let h = Harness::new(
        "worker-realtime-flood",
        "",
        &[
            ("MOZART_RVC_STUB_FILE_FEED_PACE_MS", "2".to_string()),
            ("MOZART_PRE_STUB_FLOOD", "1".to_string()),
        ],
    );

    // 20 s = 1000 契约帧：攒帧窗足够宽，未暂停 pre 时实时帧必然混入。
    let (job_id, pcm) = h.submit("tone.wav", 20, 16000);

    // 处理期间 pre 必须被暂停（file_mode=true）：状态门控交接的直接证据。
    assert!(
        wait_until(JOB_TIMEOUT, || {
            file_job(&h.conn, &job_id).state == FileJobState::Processing
                && status(&h.conn).pre.is_some_and(|pre| pre.file_mode)
        }),
        "文件任务处理期间 pre 必须暂停（file_mode=true）"
    );

    let job = wait_job_state(&h.conn, &job_id, FileJobState::Completed, JOB_TIMEOUT);
    assert!(job.result_ready, "completed 必须附结果可取标记");
    assert!(job.error.is_none());
    let wav = read_wav(&h.result(&job_id));
    assert_eq!(wav.samples.len(), pcm.len() * 3, "输出时长与输入一致");
    // 文件帧全数过数据面：实时帧未被计入（pre 已暂停，复用 socket 不污染）。
    h.assert_batch_counts(1000, 1000);
    h.assert_temp_clean(&job_id);
    h.assert_gpu_free();
    // 收尾后 pre 必须恢复（file_mode=false），实时支路重新恒发。
    assert!(
        wait_until(POLL_TIMEOUT, || {
            status(&h.conn).pre.is_some_and(|pre| !pre.file_mode)
        }),
        "文件任务收尾后 pre 必须恢复（file_mode=false）"
    );
}

/// 回归（Important #1）：PRE_SET_FILE_MODE{active:true} 已在 pre 生效而应答
/// 超时（桩延迟 450 ms > rpc 300 ms，且 < 2×rpc，恢复 HELLO 仍赶得上）。
/// 旧代码失败时不建守卫、无人恢复 → pre.file_mode 悬空 true，实时 AI 发送
/// 静默死亡。修复后守卫先于 RPC 建立，失败路径 Drop 恢复（active:false 幂等）。
#[test]
fn begin_rpc_timeout_restores_pre_and_fails_with_reason() {
    let h = Harness::new(
        "worker-begin-timeout",
        "",
        &[("MOZART_PRE_STUB_FILE_MODE_DELAY_MS", "450".to_string())],
    );
    let (job_id, _pcm) = h.submit("tone.wav", 1, 16000);
    let job = wait_job_state(&h.conn, &job_id, FileJobState::Failed, JOB_TIMEOUT);
    let error = job.error.expect("失败必须带可查询原因");
    assert!(error.contains("pre file mode pause"), "{error}");
    h.assert_temp_clean(&job_id);
    h.assert_gpu_free();
    // 恢复已发出：pre.file_mode 回到 false。begin 失败后 status.pre 可能仍是
    // 暂停前的陈旧 false 快照，故观察到 false 后再跨一个轮询周期复核：轮询
    // 重连重读后仍 false 才是真恢复（旧代码悬空 true，刷新后复核失败）。
    assert!(
        wait_until(POLL_TIMEOUT, || {
            status(&h.conn).pre.is_some_and(|pre| !pre.file_mode)
        }),
        "begin 失败必须恢复 pre"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        status(&h.conn).pre.is_some_and(|pre| !pre.file_mode),
        "跨轮询周期后 pre 必须保持恢复（旧代码留下悬空暂停，刷新后为 true）"
    );
}

/// 回归（Important #1 + #3）：暂停与恢复的应答都超时（桩延迟 800 ms > 2×rpc，
/// 恢复 HELLO 也赶不上）→ 恢复失败必须留下可查询降级原因，且不被后续轮询抹掉。
/// 旧代码 push_degraded 写入唯一 vec，下一秒 set_degraded 整体替换即丢失。
#[test]
fn begin_restore_failure_reason_survives_polls() {
    let h = Harness::new(
        "worker-restore-fail",
        "",
        &[("MOZART_PRE_STUB_FILE_MODE_DELAY_MS", "800".to_string())],
    );
    let (job_id, _pcm) = h.submit("tone.wav", 1, 16000);
    let job = wait_job_state(&h.conn, &job_id, FileJobState::Failed, JOB_TIMEOUT);
    let error = job.error.expect("失败必须带可查询原因");
    assert!(error.contains("pre file mode pause"), "{error}");
    h.assert_gpu_free();
    assert!(
        wait_until(POLL_TIMEOUT, || {
            status(&h.conn)
                .degraded
                .iter()
                .any(|reason| reason.contains("pre file mode restore"))
        }),
        "恢复失败必须留下可查询降级原因"
    );
    // 数轮轮询（100 ms 间隔）后原因仍在：持久通道不被 poll 覆写。
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        status(&h.conn)
            .degraded
            .iter()
            .any(|reason| reason.contains("pre file mode restore")),
        "恢复失败原因必须跨轮询存活（旧代码下一秒即被抹掉）"
    );
}

/// 根因治愈（SPEC-04 单一权威）：文件任务期间 kill pre，监管重启后 pre 处于
/// 未断言静默态（启动门控：不恒发污染整段攒帧、不 reclaim 输出 socket）。
/// daemon 检测失接管重新断言恢复交接，任务继续完成（不再误中止）；GPU 收尾
/// 释放、队列继续。旧代码 pre 重启无条件恒发→污染→楔死，本用例验证治愈。
#[test]
fn pre_restart_during_file_job_recovers_and_completes() {
    let h = Harness::new(
        "worker-pre-restart",
        "",
        &[
            ("MOZART_PRE_STUB_FLOOD", "1".to_string()),
            ("MOZART_RVC_STUB_FILE_FEED_PACE_MS", "2".to_string()),
            ("MOZART_RVC_STUB_FILE_DELAY_MS", "6000".to_string()),
        ],
    );
    // 20 s = 1000 契约帧，攒帧窗约 2 s：kill 落在 feed 段。
    let (job_id, _pcm) = h.submit("tone.wav", 20, 16000);
    assert!(
        wait_until(JOB_TIMEOUT, || {
            let job = file_job(&h.conn, &job_id);
            job.state == FileJobState::Processing
                && job.progress.stage == "feed"
                && status(&h.conn).pre.is_some_and(|pre| pre.file_mode)
        }),
        "任务应在 feed 段建立交接（pre.file_mode=true）"
    );
    let pre_pid = status(&h.conn).processes.pre.pid.expect("pre 应在运行");
    let killed = std::process::Command::new("kill")
        .arg("-9")
        .arg(pre_pid.to_string())
        .status()
        .unwrap();
    assert!(killed.success(), "kill pre 失败");

    // 根因治愈：pre 重启静默不污染，daemon 重断言恢复交接，任务继续完成。
    let job = wait_job_state(&h.conn, &job_id, FileJobState::Completed, JOB_TIMEOUT);
    assert!(job.result_ready, "pre 重启后任务应恢复并完成（旧代码楔死/误中止）");
    h.assert_temp_clean(&job_id);
    h.assert_gpu_free();

    // 队列继续：下一任务正常建立交接并完成（恒发再次被门控暂停）。
    let (next_id, _pcm) = h.submit("next.wav", 1, 16000);
    let next = wait_job_state(&h.conn, &next_id, FileJobState::Completed, JOB_TIMEOUT);
    assert!(next.result_ready, "后续任务应正常完成");
    h.assert_gpu_free();
}

/// 生成 PCM16 单声道正弦 WAV（测试源，留在临时目录不入库），返回样本值。
fn write_wav(path: &Path, seconds: u32, sample_rate: u32) -> Vec<i16> {
    let count = (seconds * sample_rate) as usize;
    let samples: Vec<i16> = (0..count)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            let v = 0.5 * 32767.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            v as i16
        })
        .collect();
    let data_bytes = (count * 2) as u32;
    let mut wav = Vec::with_capacity(44 + count * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in &samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, wav).unwrap();
    samples
}

struct Wav {
    format: u16,
    channels: u16,
    sample_rate: u32,
    samples: Vec<f32>,
}

/// 解析结果 WAV：按块走（ffmpeg 会插 fact/LIST 块），data 按 f32le 读。
fn read_wav(path: &Path) -> Wav {
    let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut fmt = None;
    let mut data = None;
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let body = &bytes[offset + 8..offset + 8 + size];
        match id {
            b"fmt " => {
                let raw_format = u16::from_le_bytes(body[0..2].try_into().unwrap());
                // WAVE_FORMAT_EXTENSIBLE（0xFFFE）：真实格式标签在 SubFormat
                // GUID 前 2 字节。不同 ffmpeg 版本对 float WAV 用普通头（3）
                // 或扩展头（65534），归一化后统一按真实格式判定。
                let format = if raw_format == 0xFFFE {
                    u16::from_le_bytes(body[24..26].try_into().unwrap())
                } else {
                    raw_format
                };
                fmt = Some((
                    format,
                    u16::from_le_bytes(body[2..4].try_into().unwrap()),
                    u32::from_le_bytes(body[4..8].try_into().unwrap()),
                    u16::from_le_bytes(body[14..16].try_into().unwrap()),
                ));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        offset += 8 + size + (size % 2);
    }
    let (format, channels, sample_rate, bits) = fmt.expect("WAV 缺 fmt 块");
    let data = data.expect("WAV 缺 data 块");
    assert_eq!(bits, 32, "结果固定 32 位 float");
    Wav {
        format,
        channels,
        sample_rate,
        samples: data
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect(),
    }
}

#[test]
fn read_wav_normalizes_extensible_float_header() {
    // 构造 WAVE_FORMAT_EXTENSIBLE（0xFFFE）float WAV：真实格式标签在 SubFormat
    // GUID 前 2 字节（IEEE float = 3）。验证 read_wav 归一化扩展头，不受 ffmpeg
    // 版本写普通头还是扩展头影响。
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&48000u32.to_le_bytes());
    fmt.extend_from_slice(&192000u32.to_le_bytes());
    fmt.extend_from_slice(&4u16.to_le_bytes());
    fmt.extend_from_slice(&32u16.to_le_bytes());
    fmt.extend_from_slice(&22u16.to_le_bytes());
    fmt.extend_from_slice(&32u16.to_le_bytes());
    fmt.extend_from_slice(&0u32.to_le_bytes());
    fmt.extend_from_slice(&[
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38,
        0x9B, 0x71,
    ]);
    let sample = 0.5f32.to_le_bytes();
    let mut body = Vec::new();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(sample.len() as u32).to_le_bytes());
    body.extend_from_slice(&sample);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(&body);

    let path = std::env::temp_dir().join(format!("mozart-extensible-{}.wav", std::process::id()));
    std::fs::write(&path, &bytes).unwrap();
    let wav = read_wav(&path);
    let _ = std::fs::remove_file(&path);
    assert_eq!(wav.format, 3, "扩展头应归一化为 IEEE float");
    assert_eq!(wav.channels, 1);
    assert_eq!(wav.sample_rate, 48000);
    assert_eq!(wav.samples.len(), 1);
}
