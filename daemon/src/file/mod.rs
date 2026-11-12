//! 文件转换协调层：队列 + 受管存储 + GPU 仲裁 + 命令入口。
//! 数据面（解码/喂帧/编码）在 worker/decode/encode 子模块，跑在独立 worker
//! 线程：阻塞 I/O 期间不持 State 锁；GPU 归属的每次变更都在 Services
//! transition 锁内，与实时路由切换互斥。

pub mod arbitrate;
mod decode;
mod encode;
mod ffmpeg;
mod queue;
mod storage;
pub mod worker;

use crate::services::{self, Services};
use crate::state::{lock_state, State};
use mozart_core::config::Config;
use mozart_core::ctl;
use mozart_core::ctl::schema::{FileJob, FileJobState, FileProgress, FileSubmitRequest};
use queue::{FileQueue, QueueError};
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use storage::Storage;

/// 契约帧时间轴步长（20 ms）：数据面合成 pts_ns 用，取 core 的 FRAME_MS。
pub(crate) const FRAME_NS: u64 = mozart_core::FRAME_MS as u64 * 1_000_000;

/// 文件数据面钩子。run 在独立 worker 线程调用：阻塞 I/O 期间不得持 State 锁；
/// 各阶段以 ctx.is_cancelled() 作取消检查点，关停时也经它退出。
pub trait Worker: Send + Sync {
    fn run(&self, ctx: JobContext) -> Outcome;
}

/// worker 收尾结果，协调器据此落终态、清临时、驱逐并释放 GPU。
pub enum Outcome {
    Completed,
    Failed(String),
    Cancelled,
}

/// 数据面步骤错误：取消与失败分开表达，收尾据此选 Outcome。
#[derive(Debug)]
pub(crate) enum StepError {
    Cancelled,
    Failed(String),
}

impl From<StepError> for Outcome {
    fn from(err: StepError) -> Self {
        match err {
            StepError::Cancelled => Outcome::Cancelled,
            StepError::Failed(reason) => Outcome::Failed(reason),
        }
    }
}

/// 传给 worker 的句柄集合。协调器在启动任务时快照任务信息，
/// worker 经 queue 更新进度/查取消，经 services 下发 rvc 控制命令，
/// 经 state 记录可查询的降级原因。
pub struct JobContext {
    pub job: FileJob,
    pub source: PathBuf,
    pub storage: Arc<Storage>,
    pub queue: Arc<Mutex<FileQueue>>,
    pub services: Arc<Services>,
    pub state: Arc<Mutex<State>>,
    pub config: Arc<Config>,
    pub running: Arc<AtomicBool>,
}

impl JobContext {
    /// 用户取消（cancelling）或 daemon 关停。
    pub fn is_cancelled(&self) -> bool {
        !self.running.load(Ordering::Acquire)
            || lock_queue(&self.queue).state_of(&self.job.job_id) == Some(FileJobState::Cancelling)
    }

    /// 阶段进度更新（D5），随队列落盘。
    pub fn set_progress(&self, progress: FileProgress) -> Result<(), (String, String)> {
        lock_queue(&self.queue)
            .set_progress(&self.job.job_id, progress)
            .map_err(QueueError::into_rejection)
    }

    /// 非致命降级（停流超时、进度落盘失败等）：记原因，status 可查。
    pub fn push_degraded(&self, reason: String) {
        lock_state(&self.state).push_degraded(reason);
    }
}

fn lock_queue(queue: &Mutex<FileQueue>) -> MutexGuard<'_, FileQueue> {
    queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct FileCoordinator {
    queue: Arc<Mutex<FileQueue>>,
    storage: Arc<Storage>,
    worker: Arc<dyn Worker>,
    state: Arc<Mutex<State>>,
    services: Arc<Services>,
    config: Arc<Config>,
    running: Arc<AtomicBool>,
    /// 串行化 submit 与 finish 驱逐：submit 全程（reserve_job_id → 复制 → 入队）
    /// 持此锁，finish 快照保护集合并驱逐时也持此锁。两者互斥，新排队任务的
    /// 源副本不会落在驱逐保护集合之外。不用队列锁，复制期间 list/status 仍可应答。
    submit_lock: Mutex<()>,
    /// 在跑任务的 id 与线程句柄：panic 时 finish 不会执行，reap 据 id 落终态。
    job_thread: Mutex<Option<(String, JoinHandle<()>)>>,
}

impl FileCoordinator {
    pub fn new(
        config: Arc<Config>,
        state: Arc<Mutex<State>>,
        services: Arc<Services>,
        running: Arc<AtomicBool>,
        worker: Arc<dyn Worker>,
    ) -> io::Result<Self> {
        let storage = Arc::new(Storage::new(&config)?);
        let queue = FileQueue::load(storage.dir(), config.file.queue_capacity)?;
        Ok(Self {
            queue: Arc::new(Mutex::new(queue)),
            storage,
            worker,
            state,
            services,
            config,
            running,
            submit_lock: Mutex::new(()),
            job_thread: Mutex::new(None),
        })
    }

    /// poller 每 tick 驱动一次仲裁。
    pub fn tick(self: &Arc<Self>) {
        self.reap_job_thread();
        let _transition = self.services.transition();
        let (owner, desired, active, rvc_ready) = {
            let state = lock_state(&self.state);
            (
                state.gpu_owner(),
                state.route_desired(),
                state.route_active(),
                state.rvc_ready(),
            )
        };
        let (paused, next) = {
            let queue = lock_queue(&self.queue);
            (queue.paused(), queue.next_queued())
        };
        match arbitrate::next_action(owner, paused, next.as_deref(), desired, active, rvc_ready) {
            arbitrate::Action::StartJob(job_id) => self.start_job(&job_id),
            arbitrate::Action::ApplyRoute(route) => {
                if let Err((code, reason)) =
                    services::route_set_locked(&self.state, &self.services, &self.config, route)
                {
                    lock_state(&self.state)
                        .push_degraded(format!("file arbiter route apply: {code}: {reason}"));
                }
            }
            arbitrate::Action::Idle => {}
        }
    }

    /// 关停：running 已置 false，worker 经取消检查点退出；
    /// 任务保持 processing 落盘，重启读回重跑（D3）。
    /// 此处不落终态，故 join 的 panic 也留给重启重跑，不在关停路径改写状态。
    pub fn shutdown(&self) {
        let handle = self
            .job_thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some((_, handle)) = handle {
            let _ = handle.join();
        }
    }

    pub fn submit(&self, request: FileSubmitRequest) -> Result<Value, (String, String)> {
        let _submit = self
            .submit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let model_id = self.resolve_model(request.model_id)?;
        let source = Path::new(&request.path);
        let name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| request.path.clone());
        let job_id = lock_queue(&self.queue).reserve_job_id();
        // submit 全程持 submit_lock，finish 驱逐也持此锁：复制与入队期间
        // 不会有并发驱逐，新任务源副本无需额外的在途保护标记。
        let copy = self
            .storage
            .copy_in(&job_id, source)
            .map_err(|err| err.into_rejection())?;
        let result = lock_queue(&self.queue).submit(job_id.clone(), name, model_id);
        match result {
            Ok(job) => {
                let position = lock_queue(&self.queue).queued_count();
                Ok(json!({
                    "job_id": job.job_id,
                    "state": job.state,
                    "queue_position": position,
                }))
            }
            Err(err) => {
                // 容量拒收：副本不留。
                let _ = fs::remove_file(copy);
                Err(err.into_rejection())
            }
        }
    }

    pub fn pause(&self) -> Result<Value, (String, String)> {
        let paused = lock_queue(&self.queue)
            .pause()
            .map_err(QueueError::into_rejection)?;
        Ok(json!({"paused": paused}))
    }

    pub fn resume(&self) -> Result<Value, (String, String)> {
        let paused = lock_queue(&self.queue)
            .resume()
            .map_err(QueueError::into_rejection)?;
        Ok(json!({"paused": paused}))
    }

    pub fn cancel(&self, job_id: &str) -> Result<Value, (String, String)> {
        let state = lock_queue(&self.queue)
            .cancel(&self.storage, job_id)
            .map_err(QueueError::into_rejection)?;
        Ok(json!({"job_id": job_id, "state": state}))
    }

    pub fn status(&self, job_id: &str) -> Result<Value, (String, String)> {
        let job = lock_queue(&self.queue).status(job_id).ok_or_else(|| {
            QueueError::UnknownJob(job_id.to_string()).into_rejection()
        })?;
        serde_json::to_value(job).map_err(|err| (ctl::CODE_INTERNAL.to_string(), err.to_string()))
    }

    pub fn list(&self) -> Result<Value, (String, String)> {
        let queue = lock_queue(&self.queue);
        Ok(json!({"jobs": queue.list(), "paused": queue.paused()}))
    }

    /// 显式 --model 经 ListModels 校验存在；缺省用当前已加载模型。
    fn resolve_model(&self, requested: Option<String>) -> Result<String, (String, String)> {
        match requested {
            Some(model_id) => {
                let models = services::list_models(&self.services)?;
                if models.iter().any(|model| model.id == model_id && model.exists) {
                    Ok(model_id)
                } else {
                    Err((
                        ctl::CODE_INVALID_ARGUMENT.to_string(),
                        format!("model not found: {model_id}"),
                    ))
                }
            }
            None => self
                .services
                .rvc_status()?
                .model_id
                .ok_or_else(|| {
                    (
                        ctl::CODE_NOT_READY.to_string(),
                        "no model loaded; submit with model_id".to_string(),
                    )
                }),
        }
    }

    /// 启动任务（调用方持 transition 锁，仲裁已确认 GPU 空闲、路由 direct）。
    fn start_job(self: &Arc<Self>, job_id: &str) {
        // 队列锁内检查并流转：与取消并发时让位，不 claim GPU。
        // 先绑定结果再匹配：match  scrutinee 的临时锁守卫会活到整个 match 结束。
        let marked = lock_queue(&self.queue).mark_processing(job_id);
        let job = match marked {
            Ok(true) => lock_queue(&self.queue)
                .status(job_id)
                .expect("job just marked processing"),
            Ok(false) => return,
            Err(err) => {
                self.report_queue_error("mark processing", err);
                return;
            }
        };
        let source = match self.storage.source_path(job_id) {
            Some(path) => path,
            None => {
                self.settle_failed(job_id, "source copy missing".to_string());
                return;
            }
        };
        lock_state(&self.state).claim_gpu_file();

        let ctx = JobContext {
            job,
            source,
            storage: self.storage.clone(),
            queue: self.queue.clone(),
            services: self.services.clone(),
            state: self.state.clone(),
            config: self.config.clone(),
            running: self.running.clone(),
        };
        let worker = self.worker.clone();
        let coordinator = Arc::clone(self);
        let id = job_id.to_string();
        let spawned = thread::Builder::new()
            .name(format!("mozart-file-{job_id}"))
            .spawn(move || {
                let outcome = worker.run(ctx);
                coordinator.finish(&id, outcome);
            });
        match spawned {
            Ok(handle) => {
                *self
                    .job_thread
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((job_id.to_string(), handle));
            }
            Err(err) => {
                lock_state(&self.state).release_gpu_file();
                self.settle_failed(job_id, format!("spawn file worker: {err}"));
            }
        }
    }

    /// worker 线程收尾：落终态、清临时、驱逐、释放 GPU（transition 锁内）。
    fn finish(&self, job_id: &str, outcome: Outcome) {
        let _transition = self.services.transition();
        if self.running.load(Ordering::Acquire) {
            // 与 submit 串行：快照保护集合 → 驱逐 全程持 submit_lock，
            // 排除并发 submit 的复制/入队，杜绝新排队任务源副本被驱逐。
            let _submit = self
                .submit_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let result_ready =
                matches!(outcome, Outcome::Completed) && self.storage.result_path(job_id).is_file();
            let (settled, protected) = {
                let mut queue = lock_queue(&self.queue);
                let settled = match &outcome {
                    Outcome::Completed => queue.mark_completed(job_id, result_ready),
                    Outcome::Failed(reason) => queue.mark_failed(job_id, reason.clone()),
                    Outcome::Cancelled => queue.mark_cancelled(job_id),
                };
                (settled, queue.unfinished_ids())
            };
            match settled {
                // 取消意图获胜时（含 worker 结果晚到）删源副本。
                Ok(FileJobState::Cancelled) => self.storage.remove_source(job_id),
                Ok(_) => {}
                Err(err) => self.report_queue_error("finish", err),
            }
            self.storage.cleanup_temp(job_id);
            if let Err(err) = self.storage.evict(&protected) {
                lock_state(&self.state).push_degraded(format!("file storage evict: {err}"));
            }
        }
        // 关停时不落终态：任务保持 processing，重启读回重跑（D3）。
        lock_state(&self.state).release_gpu_file();
    }

    fn settle_failed(&self, job_id: &str, reason: String) {
        match lock_queue(&self.queue).mark_failed(job_id, reason) {
            Ok(FileJobState::Cancelled) => self.storage.remove_source(job_id),
            Ok(_) => {}
            Err(err) => self.report_queue_error("mark failed", err),
        }
    }

    fn report_queue_error(&self, op: &str, err: QueueError) {
        let (code, reason) = err.into_rejection();
        lock_state(&self.state).push_degraded(format!("file queue {op}: {code}: {reason}"));
    }

    /// 回收已结束的 worker 线程。正常路径 finish 已落终态；worker panic 时
    /// finish 不会执行，join 拿到 Err，在此落 failed 并释放 GPU——否则任务
    /// 永远停在 processing、GPU 永远归 File，整个队列被卡死。
    fn reap_job_thread(&self) {
        let finished = {
            let mut slot = self
                .job_thread
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slot.as_ref() {
                Some((_, handle)) if handle.is_finished() => slot.take(),
                _ => None,
            }
        };
        let Some((job_id, handle)) = finished else {
            return;
        };
        if let Err(panic) = handle.join() {
            self.recover_panicked_job(&job_id, &panic_payload(&panic));
        }
    }

    /// panic 收尾：落 failed 带原因、清临时、释放 GPU。驱逐仍归 finish
    /// （下一个任务收尾时执行），此处不复制那份逻辑。
    fn recover_panicked_job(&self, job_id: &str, reason: &str) {
        let _transition = self.services.transition();
        self.settle_failed(job_id, format!("file worker panicked: {reason}"));
        self.storage.cleanup_temp(job_id);
        lock_state(&self.state).release_gpu_file();
    }
}

/// panic 载荷取可读文本：&str / String 之外的载荷只记未知。
fn panic_payload(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub fn temp_dir(tag: &str) -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mozart-file-{}-{tag}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::testutil::temp_dir;
    use mozart_core::ctl::schema::{Activity, Route, RvcStatus};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// 可控桩 worker：报告启动后阻塞，等测试放行或观察取消。
    struct StubWorker {
        started: mpsc::Sender<String>,
        release: Mutex<mpsc::Receiver<Outcome>>,
    }

    impl Worker for StubWorker {
        fn run(&self, ctx: JobContext) -> Outcome {
            self.started.send(ctx.job.job_id.clone()).unwrap();
            loop {
                if ctx.is_cancelled() {
                    return Outcome::Cancelled;
                }
                match self
                    .release
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .try_recv()
                {
                    Ok(outcome) => return outcome,
                    Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(1)),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Outcome::Failed("stub control closed".to_string())
                    }
                }
            }
        }
    }

    fn fake_rvc_status() -> RvcStatus {
        serde_json::from_value(json!({
            "state": "loaded",
            "model_id": "demo",
            "model_version": null,
            "capabilities": [],
            "stream_kind": null,
            "frames_in": 0,
            "frames_out": 0,
            "frames_dropped": 0,
            "blocks": 0,
            "inference_ms_last": null,
            "inference_ms_avg": null,
            "inference_ms_p95": null,
            "queue_depth": 0,
            "last_error": null
        }))
        .unwrap()
    }

    /// rvc socket 不存在：任何误发的 RPC 都会以 NotReady 失败，
    /// 以此证明 File 占用期间的路由分流不碰实时链路。
    fn setup(tag: &str) -> (Arc<FileCoordinator>, mpsc::Receiver<String>, mpsc::Sender<Outcome>, PathBuf) {
        let dir = temp_dir(tag);
        let mut config = Config::default();
        config.file.storage_dir = dir.clone();
        let config = Arc::new(config);
        let state = Arc::new(Mutex::new(State::new()));
        lock_state(&state).set_rvc_status(Some(fake_rvc_status()));
        let services = Arc::new(Services::new(&config));
        let running = Arc::new(AtomicBool::new(true));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = Arc::new(StubWorker {
            started: started_tx,
            release: Mutex::new(release_rx),
        });
        let coordinator =
            Arc::new(FileCoordinator::new(config, state, services, running, worker).unwrap());
        (coordinator, started_rx, release_tx, dir)
    }

    fn enqueue(coordinator: &FileCoordinator, name: &str) -> String {
        let mut queue = lock_queue(&coordinator.queue);
        let job_id = queue.reserve_job_id();
        queue
            .submit(job_id.clone(), name.to_string(), "demo".to_string())
            .unwrap();
        drop(queue);
        let src = coordinator.storage.dir().join(format!("src-{name}"));
        fs::write(&src, b"riff").unwrap();
        coordinator.storage.copy_in(&job_id, &src).unwrap();
        job_id
    }

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn auto_start_claims_file_and_defers_route_until_finish() {
        let (coordinator, started, release, dir) = setup("arbiter-defer");
        let job_id = enqueue(&coordinator, "a.wav");
        coordinator.tick();
        assert_eq!(
            started.recv_timeout(Duration::from_secs(2)).unwrap(),
            job_id,
            "GPU 空闲应自动启动排队任务"
        );
        assert_eq!(lock_state(&coordinator.state).gpu_owner(), Some(Activity::File));
        assert_eq!(
            lock_queue(&coordinator.queue).state_of(&job_id),
            Some(FileJobState::Processing)
        );

        // route set rvc：接受意图、不抢 GPU、不发 RPC（socket 不存在）。
        let route = services::route_set(
            &coordinator.state,
            &coordinator.services,
            &coordinator.config,
            Route::Rvc,
        )
        .unwrap();
        assert_eq!(route, Route::Rvc);
        {
            let state = lock_state(&coordinator.state);
            assert_eq!(state.route_desired(), Route::Rvc);
            assert_eq!(state.route_active(), Route::Direct);
            assert_eq!(state.gpu_owner(), Some(Activity::File));
        }

        // route set direct：清等待意图，同样不碰 rvc 流。
        services::route_set(
            &coordinator.state,
            &coordinator.services,
            &coordinator.config,
            Route::Direct,
        )
        .unwrap();
        assert_eq!(lock_state(&coordinator.state).route_desired(), Route::Direct);
        assert_eq!(lock_state(&coordinator.state).gpu_owner(), Some(Activity::File));

        // 再挂 rvc，任务结束后由 tick 应用。
        services::route_set(
            &coordinator.state,
            &coordinator.services,
            &coordinator.config,
            Route::Rvc,
        )
        .unwrap();
        release.send(Outcome::Completed).unwrap();
        assert!(
            wait_until(Duration::from_secs(2), || {
                lock_state(&coordinator.state).gpu_owner().is_none()
            }),
            "任务结束应释放 GPU"
        );
        assert_eq!(
            lock_queue(&coordinator.queue).state_of(&job_id),
            Some(FileJobState::Completed)
        );

        // tick 应用等待槽路由：rvc 不可达 → 降级原因可查询，意图保留。
        coordinator.tick();
        let status = lock_state(&coordinator.state).status();
        assert_eq!(status.route.desired, Route::Rvc);
        assert!(
            status.degraded.iter().any(|reason| reason.contains("route apply")),
            "应用失败必须留下可查询原因：{:?}",
            status.degraded
        );
        coordinator.shutdown();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancel_processing_worker_observes_and_cleans_source() {
        let (coordinator, started, _release, dir) = setup("arbiter-cancel");
        let job_id = enqueue(&coordinator, "a.wav");
        coordinator.tick();
        started.recv_timeout(Duration::from_secs(2)).unwrap();

        let reply = coordinator.cancel(&job_id).unwrap();
        assert_eq!(reply["state"], "cancelling");
        assert!(
            wait_until(Duration::from_secs(2), || {
                lock_queue(&coordinator.queue).state_of(&job_id) == Some(FileJobState::Cancelled)
            }),
            "worker 应观察 cancelling 并收尾"
        );
        assert!(coordinator.storage.source_path(&job_id).is_none());
        assert_eq!(lock_state(&coordinator.state).gpu_owner(), None);
        coordinator.shutdown();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shutdown_keeps_processing_for_restart_requeue() {
        let (coordinator, started, _release, dir) = setup("arbiter-shutdown");
        let job_id = enqueue(&coordinator, "a.wav");
        coordinator.tick();
        started.recv_timeout(Duration::from_secs(2)).unwrap();

        coordinator.running.store(false, Ordering::Release);
        coordinator.shutdown();
        assert_eq!(
            lock_queue(&coordinator.queue).state_of(&job_id),
            Some(FileJobState::Processing),
            "关停不落终态，重启从头重跑"
        );
        drop(coordinator);
        let reloaded = FileQueue::load(&dir, 50).unwrap();
        assert_eq!(reloaded.state_of(&job_id), Some(FileJobState::Queued));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pause_blocks_new_jobs_only() {
        let (coordinator, started, release, dir) = setup("arbiter-pause");
        let job_id = enqueue(&coordinator, "a.wav");
        coordinator.pause().unwrap();
        coordinator.tick();
        assert_eq!(
            lock_queue(&coordinator.queue).state_of(&job_id),
            Some(FileJobState::Queued)
        );
        assert_eq!(lock_state(&coordinator.state).gpu_owner(), None);
        assert!(started.try_recv().is_err());

        coordinator.resume().unwrap();
        coordinator.tick();
        assert_eq!(
            started.recv_timeout(Duration::from_secs(2)).unwrap(),
            job_id
        );
        release.send(Outcome::Completed).unwrap();
        coordinator.shutdown();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_source_fails_job_and_frees_gpu() {
        let (coordinator, _started, _release, dir) = setup("arbiter-missing");
        let job_id = enqueue(&coordinator, "a.wav");
        coordinator.storage.remove_source(&job_id);
        coordinator.tick();
        let job = lock_queue(&coordinator.queue).status(&job_id).unwrap();
        assert_eq!(job.state, FileJobState::Failed);
        assert!(job.error.unwrap().contains("source"));
        assert_eq!(lock_state(&coordinator.state).gpu_owner(), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 回归：worker panic 时 finish 不会执行，reap 必须落 failed 并释放 GPU。
    /// 旧代码忽略 join Err，任务永远停在 processing、GPU 永远归 File，队列卡死。
    struct PanickingWorker;

    impl Worker for PanickingWorker {
        fn run(&self, _ctx: JobContext) -> Outcome {
            panic!("file worker boom");
        }
    }

    #[test]
    fn worker_panic_fails_job_and_releases_gpu() {
        let dir = temp_dir("worker-panic");
        let mut config = Config::default();
        config.file.storage_dir = dir.clone();
        let config = Arc::new(config);
        let state = Arc::new(Mutex::new(State::new()));
        lock_state(&state).set_rvc_status(Some(fake_rvc_status()));
        let services = Arc::new(Services::new(&config));
        let running = Arc::new(AtomicBool::new(true));
        let coordinator = Arc::new(
            FileCoordinator::new(config, state, services, running, Arc::new(PanickingWorker))
                .unwrap(),
        );
        let job_id = enqueue(&coordinator, "a.wav");
        // 临时文件残留：panic 收尾也要清掉。
        fs::write(coordinator.storage.decoded_path(&job_id), b"junk").unwrap();
        coordinator.tick();
        assert!(
            wait_until(Duration::from_secs(2), || {
                coordinator.tick();
                lock_queue(&coordinator.queue).state_of(&job_id) == Some(FileJobState::Failed)
            }),
            "panic 后 reap 必须落 failed"
        );
        let job = lock_queue(&coordinator.queue).status(&job_id).unwrap();
        assert!(
            job.error.unwrap().contains("file worker boom"),
            "panic 原因必须可查询"
        );
        assert_eq!(lock_state(&coordinator.state).gpu_owner(), None);
        assert!(
            !coordinator.storage.decoded_path(&job_id).exists(),
            "panic 收尾也要清临时文件"
        );
        coordinator.shutdown();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 最小 rvc 控制桩：只应答 HELLO/GET_STATUS/LIST_MODELS，让 coordinator.submit
    /// 的模型校验在单测内可达，无需外部进程。绑定与 listen 同步完成后再起 accept 循环。
    fn spawn_rvc_stub(socket: &Path) {
        let listener = ctl::Seqpacket::bind(socket).unwrap();
        listener.listen(4).unwrap();
        thread::spawn(move || {
            while let Ok(conn) = listener.accept() {
                while let Ok(msg) = conn.recv(None) {
                    let type_ = msg.header.type_;
                    let data = match type_ {
                        ctl::RVC_HELLO => json!({"version": "0.1.0", "stub": true}),
                        ctl::RVC_LIST_MODELS => {
                            json!({"models": [{"id": "demo", "exists": true, "loaded": true}]})
                        }
                        ctl::RVC_GET_STATUS => json!({
                            "state": "loaded", "model_id": "demo", "model_version": null,
                            "capabilities": [], "stream_kind": null,
                            "frames_in": 0, "frames_out": 0, "frames_dropped": 0, "blocks": 0,
                            "inference_ms_last": null, "inference_ms_avg": null,
                            "inference_ms_p95": null, "queue_depth": 0, "last_error": null
                        }),
                        _ => json!({}),
                    };
                    let reply =
                        json!({"status": "ok", "code": ctl::CODE_OK, "reason": "", "data": data})
                            .to_string()
                            .into_bytes();
                    if conn.send(type_, msg.header.request_id, &reply).is_err() {
                        break;
                    }
                }
            }
        });
    }

    /// 回归（Important #1）：finish 快照保护集合与驱逐必须和 submit 串行。
    /// 旧代码 finish 不持 submit_lock：submit 的“入队 + 清在途标记”若落在 finish
    /// 快照之后、读在途标记之前，新排队任务的源副本既不在保护集合也不在在途标记里，
    /// 超限驱逐会删掉它，任务启动时报 source copy missing。
    /// 本测试持 submit_lock 模拟一个进行中的 submit，再放行 worker 触发 finish：
    /// 修复后 finish 必须阻塞在 submit_lock 上（任务保持 processing），旧代码则会
    /// 立即落终态并驱逐——据此断言串行化；随后释放锁，验证新任务源副本在驱逐后存活。
    #[test]
    fn finish_eviction_serializes_with_submit_and_protects_new_source() {
        let dir = temp_dir("evict-submit-race");
        let src_dir = temp_dir("evict-submit-race-src");
        let mut config = Config::default();
        config.daemon.run_dir = dir.clone();
        config.rvc.socket = "rvc.sock".to_string();
        config.file.storage_dir = dir.clone();
        config.file.storage_limit_bytes = 100;
        let config = Arc::new(config);
        spawn_rvc_stub(&config.rvc_socket());

        let state = Arc::new(Mutex::new(State::new()));
        lock_state(&state).set_rvc_status(Some(fake_rvc_status()));
        let services = Arc::new(Services::new(&config));
        let running = Arc::new(AtomicBool::new(true));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = Arc::new(StubWorker {
            started: started_tx,
            release: Mutex::new(release_rx),
        });
        let coordinator =
            Arc::new(FileCoordinator::new(config, state, services, running, worker).unwrap());

        // 预置一个已完成任务的旧文件，确保驱逐必然触发并删到它。
        let old = dir.join("job_0000000000000.output.wav");
        fs::write(&old, vec![0u8; 200]).unwrap();

        // 任务 A 进入 processing，worker 阻塞等放行。
        let job_a = enqueue(&coordinator, "a.wav");
        coordinator.tick();
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            job_a
        );

        // 持 submit_lock 模拟进行中的 submit。新任务 B 的源文件较大且放在受管目录外，
        // 一旦失去保护就会被驱逐选中删除；放在目录外避免驱逐先删掉待复制的原始源。
        let submit_guard = coordinator
            .submit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let src_b = src_dir.join("b.wav");
        fs::write(&src_b, vec![0u8; 500]).unwrap();
        let submitter = {
            let coordinator = Arc::clone(&coordinator);
            let path = src_b.to_string_lossy().into_owned();
            thread::spawn(move || {
                coordinator.submit(FileSubmitRequest {
                    path,
                    model_id: Some("demo".to_string()),
                })
            })
        };

        // 放行 worker → finish 起跑。修复后它应阻塞在 submit_lock 上，A 保持 processing。
        release_tx.send(Outcome::Failed("done".to_string())).unwrap();
        let settled_early = wait_until(Duration::from_millis(300), || {
            lock_queue(&coordinator.queue).state_of(&job_a) != Some(FileJobState::Processing)
        });
        assert!(
            !settled_early,
            "finish 必须等 submit_lock 释放后才能落终态/驱逐（旧代码会在此抢先驱逐）"
        );
        assert_eq!(
            lock_state(&coordinator.state).gpu_owner(),
            Some(Activity::File)
        );

        // 释放 submit_lock：submit 与 finish 串行推进，B 入队后受未完成任务保护。
        drop(submit_guard);
        let reply = submitter.join().unwrap().unwrap();
        let job_b = reply["job_id"].as_str().unwrap().to_string();
        coordinator.shutdown();

        assert_eq!(
            lock_queue(&coordinator.queue).state_of(&job_b),
            Some(FileJobState::Queued)
        );
        assert!(
            coordinator.storage.source_path(&job_b).is_some(),
            "新排队任务的源副本必须在驱逐后存活"
        );
        assert!(!old.exists(), "驱逐应已删除最旧的已完成文件（证明驱逐确实运行）");
        drop(coordinator);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&src_dir).unwrap();
    }
}
