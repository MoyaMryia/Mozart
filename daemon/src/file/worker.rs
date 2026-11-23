//! 文件数据面 worker（SPEC-04 中间层）：解码 → 喂帧 → 收帧 → 编码发布。
//! 协调器为每个任务起独立线程跑 `run`：离线，不在实时路径上，阻塞 I/O 期间
//! 不持 State 锁。GPU 已由协调器 claim File，终态/驱逐/释放由 finish 收尾。
//!
//! 数据面复用实时 socket（SPEC-03「复用 + 状态门控交接」）：起流前先经
//! PRE_SET_FILE_MODE 让 pre 暂停 rvc AI 发送并释放 rvc-out.sock，daemon 再
//! 绑定同一输出 socket、喂帧到 rvc-in.sock；收尾先停流、解绑，再恢复 pre。
//!
//! 取消与失败次序：各阶段以 `ctx.is_cancelled()` 为检查点；起流后的任何出口
//! 都经 FileHandoff 守卫按「停流 → 解绑 → 恢复 pre」收尾（Drop 兜底 panic）。

use super::decode::{self, ContractFrame};
use super::{encode, JobContext, Outcome, ReturnedHandoff, StepError, Worker, FRAME_NS};
use mozart_core::ctl::schema::FileProgress;
use mozart_core::dgram::FrameSocket;
use mozart_core::{FrameMeta, InputFrame, OutputFrame, OUTPUT_SAMPLES};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// 数据面 socket 缓冲帧数：SPEC-02「接收缓冲 4–8 帧」取上沿。
const QUEUE_FRAMES: usize = 8;

/// 阶段进度带（D5）：percent 是整任务粗进度，decode 从 0 起，各阶段依次占一段。
const FEED_START: u8 = 20;
const INFER_START: u8 = 40;
const PUBLISH_START: u8 = 90;

/// 进度落盘节流：queue.json 每次变更原子写 + fsync，逐帧写会拖慢数据面。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// 收帧轮询粒度，也是收帧阶段的取消检查点间隔：
/// 整段推理可能很久，关停 join 不得被它拖住。
const RECV_POLL: Duration = Duration::from_millis(200);

/// 首帧前等待上限：整段推理在回帧前完成（SPEC-03 离线批处理），
/// 300 s 时长上限下按两倍实时兜底。
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(600);

/// 回帧阶段的帧间隔上限：C++ 逐帧阻塞发送，正常为毫秒级。
const FRAME_GAP_TIMEOUT: Duration = Duration::from_secs(30);

/// infer 段读 rvc blocks 的间隔（D5 细化进度）。
const STATUS_POLL: Duration = Duration::from_millis(500);

/// 真实文件数据面 worker。无状态：每个任务的上下文由 JobContext 带入。
pub struct FileWorker;

impl Worker for FileWorker {
    fn run(&self, ctx: JobContext) -> Outcome {
        let result = process(&ctx);
        // 临时文件（decoded.f32 / output.f32 / .part）统一在此清理：
        // 成功、失败、取消同一出口；协调器 finish 再兜底一次（含 panic 路径）。
        ctx.storage.cleanup_temp(&ctx.job.job_id);
        match result {
            Ok(()) => Outcome::Completed,
            Err(err) => err.into(),
        }
    }
}

fn process(ctx: &JobContext) -> Result<(), StepError> {
    let cancel = || ctx.is_cancelled();
    if cancel() {
        return Err(StepError::Cancelled);
    }
    let config = ctx.config.as_ref();
    let job_id = ctx.job.job_id.as_str();
    let mut progress = Reporter::new(ctx);

    // ---- decode：ffmpeg 解码 + 时长校验 + 打包 16 kHz 契约帧 ----
    progress.stage("decode", 0);
    let (frames, total_samples) = decode::decode(
        config,
        &ctx.source,
        &ctx.storage.decoded_path(job_id),
        &cancel,
        &mut |percent| {
            progress.throttled("decode", stage_percent(0, FEED_START, u64::from(percent), 100));
        },
    )?;
    let total = frames.len();
    // total_frames 是 u32 契约（D1）；时长上限换算出的帧数可能超出。
    let total_frames = u32::try_from(total).map_err(|_| {
        StepError::Failed(format!(
            "decoded {total} frames exceeds total_frames limit (u32)"
        ))
    })?;

    // ---- feed：pre 交接 → 预设读校验 → 绑 socket → LoadModel → 预设写入 → StartStream(file) ----
    progress.stage("feed", FEED_START);
    // SPEC-03 状态门控交接：先让 pre 暂停 rvc AI 发送并释放 rvc-out.sock
    // （pre 确认释放完成才应答），daemon 再绑定同一 socket。次序不能反，
    // 否则撞绑；实时帧若未停发会污染整段攒帧（长文件死锁根因）。
    let mut handoff = FileHandoff::begin(ctx)?;
    // 预设先读校验、LoadModel 成功后再写基线（SPEC-11）：同模型跳过保留调参，
    // 预设损坏与模型加载失败都在写入前退出，不留 live 参数与已加载模型错位。
    let resolution = ctx
        .presets
        .resolve_for_model(&ctx.job.model_id)
        .map_err(|(code, reason)| StepError::Failed(format!("preset load: {code}: {reason}")))?;
    handoff.bind_output()?;
    let handle = ctx
        .services
        .load_model(&ctx.job.model_id)
        .map_err(|(code, reason)| StepError::Failed(format!("load model: {code}: {reason}")))?;
    let stream_config = ctx
        .presets
        .commit_for_model(&ctx.job.model_id, resolution)
        .map_err(|(code, reason)| StepError::Failed(format!("preset load: {code}: {reason}")))?;
    // 起流意图发出后任何出口都停流：StartStream 可能已在 C++ 生效而应答超时，
    // 不停会留下一个等帧的文件流，下一个任务撞 Busy。
    handoff.stream_active = true;
    ctx.services
        .file_start_stream(&stream_config, handle, total_frames, total_samples)
        .map_err(|(code, reason)| StepError::Failed(format!("start file stream: {code}: {reason}")))?;

    let input_path = config.rvc_input_socket();
    let exchanged = feed(&frames, &input_path, &mut progress, &cancel)
        .and_then(|()| receive(ctx, handoff.output(), total, &mut progress, &cancel));
    // 把交接交还给协调器。文件发布不等待控制面归还应答。
    drop(handoff);
    let samples = exchanged?;

    // ---- publish：48 kHz 裸流 → WAV → 原子改名 ----
    progress.stage("publish", PUBLISH_START);
    encode::publish(
        config,
        &samples,
        &ctx.storage.output_raw_path(job_id),
        &ctx.storage.result_part_path(job_id),
        &ctx.storage.result_path(job_id),
        &cancel,
    )
}

/// 喂帧：阻塞发送不丢（SPEC-03 文件帧），逐帧查取消。
/// 对端活着但停止读帧时缓冲会填满，阻塞等待由同一个取消谓词打断
/// （用户取消或 daemon 关停），不会把本线程楔死在内核里。
/// 连接实时输入 socket rvc-in.sock（rvc 绑定）：pre 的 rvc AI 发送已被
/// PRE_SET_FILE_MODE 门控暂停，实时帧不会混入整段攒帧（SPEC-03 复用）。
/// meta 序号从 0 顺序，pts_ns 按 20 ms 节拍合成，vad_flag 取解码结果。
fn feed(
    frames: &[ContractFrame],
    input_path: &Path,
    progress: &mut Reporter<'_>,
    cancel: &dyn Fn() -> bool,
) -> Result<(), StepError> {
    let input = FrameSocket::<InputFrame>::connect(input_path, QUEUE_FRAMES)
        .map_err(|err| StepError::Failed(format!("connect file input socket: {err}")))?;
    let total = frames.len() as u64;
    for (idx, frame) in frames.iter().enumerate() {
        if cancel() {
            return Err(StepError::Cancelled);
        }
        let wire = InputFrame {
            meta: FrameMeta {
                pts_ns: idx as u64 * FRAME_NS,
                frame_idx: idx as u32,
                vad_flag: frame.vad_flag,
                energy_db: 0,
                conf: 0,
                segment_id: 0,
            },
            pcm: frame.pcm,
        };
        if let Err(err) = input.send_blocking_with(&wire, cancel) {
            // 可取消发送只在取消时返回 Interrupted：走取消收尾，不算失败。
            if err.kind() == io::ErrorKind::Interrupted {
                return Err(StepError::Cancelled);
            }
            return Err(StepError::Failed(format!("feed frame {idx}: {err}")));
        }
        progress.throttled(
            "feed",
            stage_percent(FEED_START, INFER_START, idx as u64 + 1, total),
        );
    }
    Ok(())
}

/// 收帧：整段推理后才回帧，首帧前按 FIRST_FRAME_TIMEOUT 兜底，
/// 回帧阶段按帧间隔兜底；两者都以 RECV_POLL 粒度查取消。
fn receive(
    ctx: &JobContext,
    output: &FrameSocket<OutputFrame>,
    total: usize,
    progress: &mut Reporter<'_>,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<f32>, StepError> {
    let mut samples = Vec::with_capacity(total * OUTPUT_SAMPLES);
    let mut deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    let mut status_poll = Instant::now();
    let mut received = 0usize;
    progress.stage("infer", INFER_START);
    while received < total {
        if cancel() {
            return Err(StepError::Cancelled);
        }
        match output.recv(Some(RECV_POLL)) {
            Ok(frame) => {
                // packed 字段按值搬出，不取引用。
                let pcm = frame.pcm;
                samples.extend_from_slice(&pcm);
                received += 1;
                deadline = Instant::now() + FRAME_GAP_TIMEOUT;
                progress.throttled(
                    "infer",
                    stage_percent(INFER_START, PUBLISH_START, received as u64, total as u64),
                );
            }
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                if Instant::now() >= deadline {
                    return Err(StepError::Failed(format!(
                        "rvc output stalled after {received}/{total} frames"
                    )));
                }
            }
            Err(err) => return Err(StepError::Failed(format!("recv output frame: {err}"))),
        }
        if status_poll.elapsed() >= STATUS_POLL {
            status_poll = Instant::now();
            // blocks 细化（D5）。查询失败由每秒轮询记降级，此处退回粗阶段。
            if let Ok(status) = ctx.services.rvc_status() {
                progress.blocks(status.blocks);
            }
        }
    }
    Ok(samples)
}

/// 阶段内进度映射到整任务 percent 带。调用方保证 total > 0。
fn stage_percent(start: u8, end: u8, done: u64, total: u64) -> u8 {
    start + ((end - start) as u64 * done / total) as u8
}

/// 进度上报（D5）：阶段边界立即落盘，阶段内按 PROGRESS_INTERVAL 节流。
/// blocks_total 恒 None：Generator 窗长是 C++ 内部 preset，不在跨语言契约内，
/// daemon 不复制该常量，缺总数时只报 blocks_done（SPEC-04 退回粗阶段）。
struct Reporter<'a> {
    ctx: &'a JobContext,
    stage: String,
    percent: u8,
    blocks: Option<u64>,
    last: Instant,
}

impl<'a> Reporter<'a> {
    fn new(ctx: &'a JobContext) -> Self {
        Self {
            ctx,
            stage: String::new(),
            percent: 0,
            blocks: None,
            last: Instant::now(),
        }
    }

    fn stage(&mut self, stage: &str, percent: u8) {
        self.stage = stage.to_string();
        self.percent = percent;
        self.blocks = None;
        self.write();
    }

    fn throttled(&mut self, stage: &str, percent: u8) {
        self.stage = stage.to_string();
        self.percent = percent;
        if self.last.elapsed() >= PROGRESS_INTERVAL {
            self.write();
        }
    }

    /// 只刷新 blocks，不动阶段与 percent（收帧阶段的细化计数）。
    fn blocks(&mut self, blocks: u64) {
        self.blocks = Some(blocks);
        self.write();
    }

    fn write(&mut self) {
        self.last = Instant::now();
        let progress = FileProgress {
            stage: self.stage.clone(),
            percent: self.percent,
            blocks_done: self.blocks,
            blocks_total: None,
        };
        if let Err((code, reason)) = self.ctx.set_progress(progress) {
            self.ctx
                .push_degraded(format!("file progress: {code}: {reason}"));
        }
    }
}

/// 交接守卫：起流前取得 pre 应答，退出（含 unwind）时移交仍持有的资源。
struct FileHandoff<'a> {
    ctx: &'a JobContext,
    path: PathBuf,
    output: Option<FrameSocket<OutputFrame>>,
    stream_active: bool,
}

impl<'a> FileHandoff<'a> {
    /// 发送暂停前建立守卫；命令可能生效但丢失应答。
    fn begin(ctx: &'a JobContext) -> Result<Self, StepError> {
        let handoff = Self {
            ctx,
            path: ctx.config.rvc_output_socket(),
            output: None,
            stream_active: false,
        };
        handoff
            .ctx
            .services
            .pre_set_file_mode(true)
            .map_err(|(code, reason)| {
                StepError::Failed(format!("pre file mode pause: {code}: {reason}"))
            })?;
        // 交接已建立：此后到 finish 之间 pre.file_mode 必为 true，
        // 协调器据此检测 pre 重启失接管。
        handoff.ctx.handoff_active.store(true, Ordering::Release);
        Ok(handoff)
    }

    /// pre 已释放，daemon 绑定同一输出 socket（实时 rvc-out.sock）。
    fn bind_output(&mut self) -> Result<(), StepError> {
        let socket = FrameSocket::<OutputFrame>::bind(&self.path, QUEUE_FRAMES)
            .map_err(|err| StepError::Failed(format!("bind rvc output socket: {err}")))?;
        self.output = Some(socket);
        Ok(())
    }

    /// 数据面收帧用输出 socket；bind_output 先于 feed/receive，调用方保证已绑定。
    fn output(&self) -> &FrameSocket<OutputFrame> {
        self.output
            .as_ref()
            .expect("output socket bound before data plane")
    }

}

impl Drop for FileHandoff<'_> {
    fn drop(&mut self) {
        self.ctx.handoff_active.store(false, Ordering::Release);
        let output = self.output.take().map(|socket| (std::mem::take(&mut self.path), Some(socket)));
        *super::lock_return(&self.ctx.returned_handoff) = Some(ReturnedHandoff {
            stop_needed: self.stream_active,
            output,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::lock_queue;
    use crate::file::queue::FileQueue;
    use crate::file::storage::Storage;
    use crate::file::testutil::temp_dir;
    use crate::preset::PresetManager;
    use crate::runtime::SharedConfig;
    use crate::services::Services;
    use crate::state::State;
    use crate::state_file::StateFile;
    use mozart_core::config::Config;
    use mozart_core::INPUT_SAMPLES;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;

    /// 喂帧停住的观察窗：大于进度节流间隔 PROGRESS_INTERVAL，
    /// 仍在推进的喂帧必然在窗内落盘一次进度。
    const STALL_WINDOW: Duration = Duration::from_millis(800);

    /// 回归：daemon 关停（running=false）时喂帧正阻塞在挂死对端的满缓冲上，
    /// 必须在一个等待分片内返回取消，协调器 shutdown 的 join 才不会被楔住。
    /// 旧实现 poll(-1) 无限等待，取消检查点只在帧边界，关停永远等不到退出。
    #[test]
    fn feed_exits_on_shutdown_when_peer_hangs() {
        let dir = temp_dir("feed-shutdown");
        let mut config = Config::default();
        config.daemon.run_dir = dir.clone();
        config.file.storage_dir = dir.clone();
        let config = Arc::new(config);
        let storage = Arc::new(Storage::new(&config).unwrap());
        let queue = Arc::new(Mutex::new(FileQueue::load(storage.dir(), 8).unwrap()));
        let job = {
            let mut queue = lock_queue(&queue);
            let job_id = queue.reserve_job_id();
            let job = queue
                .submit(job_id, "a.wav".to_string(), "demo".to_string())
                .unwrap();
            queue.mark_processing(&job.job_id).unwrap();
            job
        };
        let job_id = job.job_id.clone();
        let running = Arc::new(AtomicBool::new(true));
        let shared = SharedConfig::new(config.as_ref().clone());
        let services = Arc::new(Services::new(&shared));
        let state = Arc::new(Mutex::new(State::new()));
        let state_file = Arc::new(Mutex::new(
            StateFile::load(dir.join("state.json")).unwrap(),
        ));
        let presets = Arc::new(PresetManager::new(
            shared,
            state.clone(),
            services.clone(),
            state_file,
            None,
            Arc::new(Mutex::new(())),
        ));
        let ctx = JobContext {
            job,
            source: dir.join("a.wav"),
            storage,
            queue: queue.clone(),
            services,
            state,
            config: config.clone(),
            presets,
            running: running.clone(),
            handoff_active: Arc::new(AtomicBool::new(false)),
            returned_handoff: Arc::new(std::sync::Mutex::new(None)),
        };

        // 对端绑定输入 socket 但永不读帧（推理线程挂死）：缓冲填满后只能等待。
        let input_path = config.rvc_input_socket();
        let peer = FrameSocket::<InputFrame>::bind(&input_path, 1).unwrap();
        // 帧数远多于缓冲容量：不打断就送不完。
        let frames: Vec<ContractFrame> = (0..200)
            .map(|_| ContractFrame {
                pcm: [0.0; INPUT_SAMPLES],
                vad_flag: 1,
            })
            .collect();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut progress = Reporter::new(&ctx);
            progress.stage("feed", FEED_START);
            let _ = done_tx.send(feed(
                &frames,
                &input_path,
                &mut progress,
                &|| ctx.is_cancelled(),
            ));
        });

        // 阻塞证据：stage 落到 feed 后，进度在观察窗内不再推进。
        let deadline = Instant::now() + Duration::from_secs(5);
        let percent = loop {
            let progress = lock_queue(&queue).status(&job_id).unwrap().progress;
            if progress.stage == "feed" {
                break progress.percent;
            }
            assert!(Instant::now() < deadline, "喂帧阶段应已开始");
            thread::sleep(Duration::from_millis(5));
        };
        thread::sleep(STALL_WINDOW);
        let settled = lock_queue(&queue).status(&job_id).unwrap().progress.percent;
        assert_eq!(settled, percent, "喂帧应阻塞在满缓冲上，进度不应推进");

        running.store(false, Ordering::Release);
        let result = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("关停必须打断阻塞喂帧，旧实现会在此挂死");
        worker.join().unwrap();
        assert!(
            matches!(result, Err(StepError::Cancelled)),
            "关停应走取消收尾：{result:?}"
        );
        drop(peer);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
