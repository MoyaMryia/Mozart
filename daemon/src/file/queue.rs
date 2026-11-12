//! 文件任务队列：六态状态机、容量准入、原子落盘（D3）。
//! 每次变更写 `<storage_dir>/queue.json`（临时文件 + rename）；启动读回，
//! 未完成任务（processing/cancelling）重置为 queued 从头重跑。
//! 状态流转由 FileCoordinator 编排，队列内部信任调用方给定的流转。

use crate::file::storage::{Storage, QUEUE_FILE};
use crate::state::now_unix_ms;
use mozart_core::ctl;
use mozart_core::ctl::schema::{FileJob, FileJobState, FileProgress};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum QueueError {
    Full { capacity: u32 },
    UnknownJob(String),
    AlreadyFinished(String),
    Io(io::Error),
}

impl QueueError {
    pub fn into_rejection(self) -> (String, String) {
        match self {
            // 队列满是容量准入规则（SPEC-04），独立于冲突裁决，用 Busy。
            QueueError::Full { capacity } => (
                ctl::CODE_BUSY.to_string(),
                format!("file queue is full (capacity {capacity})"),
            ),
            QueueError::UnknownJob(job_id) => (
                ctl::CODE_INVALID_ARGUMENT.to_string(),
                format!("unknown job_id: {job_id}"),
            ),
            QueueError::AlreadyFinished(job_id) => (
                ctl::CODE_INVALID_ARGUMENT.to_string(),
                format!("job already finished: {job_id}"),
            ),
            QueueError::Io(err) => (ctl::CODE_INTERNAL.to_string(), err.to_string()),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    paused: bool,
    jobs: Vec<FileJob>,
}

pub struct FileQueue {
    dir: PathBuf,
    capacity: u32,
    jobs: Vec<FileJob>,
    paused: bool,
}

/// 新任务初始进度：尚未开始，粗阶段从 decode 起。
fn initial_progress() -> FileProgress {
    FileProgress {
        stage: "decode".to_string(),
        percent: 0,
        blocks_done: None,
        blocks_total: None,
    }
}

fn is_unfinished(state: FileJobState) -> bool {
    matches!(
        state,
        FileJobState::Queued | FileJobState::Processing | FileJobState::Cancelling
    )
}

impl FileQueue {
    /// 启动读回。快照缺失按空队列；损坏则报错停启动（不静默丢任务）。
    /// processing/cancelling 重置为 queued：推理状态不保留，从头重跑（D3）。
    pub fn load(dir: &Path, capacity: u32) -> io::Result<Self> {
        let path = dir.join(QUEUE_FILE);
        let mut queue = Self {
            dir: dir.to_path_buf(),
            capacity,
            jobs: Vec::new(),
            paused: false,
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(queue),
            Err(err) => return Err(err),
        };
        let snapshot: Snapshot = serde_json::from_str(&text)
            .map_err(|err| io::Error::other(format!("parse {}: {err}", path.display())))?;
        queue.paused = snapshot.paused;
        queue.jobs = snapshot.jobs;
        let mut reset = false;
        for job in &mut queue.jobs {
            if matches!(job.state, FileJobState::Processing | FileJobState::Cancelling) {
                job.state = FileJobState::Queued;
                job.progress = initial_progress();
                job.error = None;
                job.result_ready = false;
                reset = true;
            }
        }
        if reset {
            queue.persist()?;
        }
        Ok(queue)
    }

    /// 生成候选 job_id（`job_<epoch_us>`）。并发提交由协调器串行化，
    /// 这里只避开与既有任务的碰撞。
    pub fn reserve_job_id(&self) -> String {
        let mut micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros() as u64)
            .unwrap_or(0);
        loop {
            let candidate = format!("job_{micros}");
            if !self.jobs.iter().any(|job| job.job_id == candidate) {
                return candidate;
            }
            micros += 1;
        }
    }

    /// 入队。容量按未完成任务数（queued+processing+cancelling）准入。
    pub fn submit(
        &mut self,
        job_id: String,
        name: String,
        model_id: String,
    ) -> Result<FileJob, QueueError> {
        if self.unfinished_count() >= self.capacity as usize {
            return Err(QueueError::Full {
                capacity: self.capacity,
            });
        }
        let job = FileJob {
            job_id,
            name,
            model_id,
            state: FileJobState::Queued,
            progress: initial_progress(),
            error: None,
            result_ready: false,
            created_unix_ms: now_unix_ms(),
        };
        self.jobs.push(job.clone());
        self.persist().map_err(QueueError::Io)?;
        Ok(job)
    }

    pub fn pause(&mut self) -> Result<bool, QueueError> {
        if !self.paused {
            self.paused = true;
            self.persist().map_err(QueueError::Io)?;
        }
        Ok(self.paused)
    }

    pub fn resume(&mut self) -> Result<bool, QueueError> {
        if self.paused {
            self.paused = false;
            self.persist().map_err(QueueError::Io)?;
        }
        Ok(self.paused)
    }

    /// queued → cancelled 并删源副本；processing → cancelling 交 worker 收尾；
    /// cancelling 幂等；终态拒绝。
    pub fn cancel(
        &mut self,
        storage: &Storage,
        job_id: &str,
    ) -> Result<FileJobState, QueueError> {
        let job = self.find(job_id)?;
        match job.state {
            FileJobState::Queued => {
                job.state = FileJobState::Cancelled;
                self.persist().map_err(QueueError::Io)?;
                storage.remove_source(job_id);
                Ok(FileJobState::Cancelled)
            }
            FileJobState::Processing => {
                job.state = FileJobState::Cancelling;
                self.persist().map_err(QueueError::Io)?;
                Ok(FileJobState::Cancelling)
            }
            FileJobState::Cancelling => Ok(FileJobState::Cancelling),
            _ => Err(QueueError::AlreadyFinished(job_id.to_string())),
        }
    }

    pub fn status(&self, job_id: &str) -> Option<FileJob> {
        self.jobs.iter().find(|job| job.job_id == job_id).cloned()
    }

    /// 按提交顺序（旧到新）。
    pub fn list(&self) -> Vec<FileJob> {
        self.jobs.clone()
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn state_of(&self, job_id: &str) -> Option<FileJobState> {
        self.jobs.iter().find(|job| job.job_id == job_id).map(|job| job.state)
    }

    pub fn next_queued(&self) -> Option<String> {
        self.jobs
            .iter()
            .find(|job| job.state == FileJobState::Queued)
            .map(|job| job.job_id.clone())
    }

    pub fn queued_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| job.state == FileJobState::Queued)
            .count()
    }

    pub fn unfinished_count(&self) -> usize {
        self.jobs.iter().filter(|job| is_unfinished(job.state)).count()
    }

    pub fn unfinished_ids(&self) -> Vec<String> {
        self.jobs
            .iter()
            .filter(|job| is_unfinished(job.state))
            .map(|job| job.job_id.clone())
            .collect()
    }

    /// 仅 queued → processing。与取消并发（任务已离开 queued）时让位，
    /// 返回 false；调用方不得再 claim GPU。
    pub fn mark_processing(&mut self, job_id: &str) -> Result<bool, QueueError> {
        let job = self.find(job_id)?;
        if job.state != FileJobState::Queued {
            return Ok(false);
        }
        job.state = FileJobState::Processing;
        self.persist().map_err(QueueError::Io)?;
        Ok(true)
    }

    pub fn mark_completed(
        &mut self,
        job_id: &str,
        result_ready: bool,
    ) -> Result<FileJobState, QueueError> {
        self.settle(job_id, FileJobState::Completed, None, result_ready)
    }

    pub fn mark_failed(&mut self, job_id: &str, reason: String) -> Result<FileJobState, QueueError> {
        self.settle(job_id, FileJobState::Failed, Some(reason), false)
    }

    pub fn mark_cancelled(&mut self, job_id: &str) -> Result<FileJobState, QueueError> {
        self.settle(job_id, FileJobState::Cancelled, None, false)
    }

    /// 终态落盘。任务处于 cancelling 时取消意图获胜，一律落 cancelled；
    /// 返回实际终态，调用方据此清理源副本。
    fn settle(
        &mut self,
        job_id: &str,
        state: FileJobState,
        error: Option<String>,
        result_ready: bool,
    ) -> Result<FileJobState, QueueError> {
        let job = self.find(job_id)?;
        let state = if job.state == FileJobState::Cancelling {
            FileJobState::Cancelled
        } else {
            state
        };
        job.state = state;
        job.error = error;
        job.result_ready = result_ready && state == FileJobState::Completed;
        if state == FileJobState::Completed {
            job.progress = FileProgress {
                stage: "publish".to_string(),
                percent: 100,
                blocks_done: None,
                blocks_total: None,
            };
        }
        self.persist().map_err(QueueError::Io)?;
        Ok(state)
    }

    /// worker 阶段进度更新（D5）。
    pub fn set_progress(&mut self, job_id: &str, progress: FileProgress) -> Result<(), QueueError> {
        self.find(job_id)?.progress = progress;
        self.persist().map_err(QueueError::Io)
    }

    fn find(&mut self, job_id: &str) -> Result<&mut FileJob, QueueError> {
        self.jobs
            .iter_mut()
            .find(|job| job.job_id == job_id)
            .ok_or_else(|| QueueError::UnknownJob(job_id.to_string()))
    }

    fn persist(&self) -> io::Result<()> {
        let snapshot = Snapshot {
            paused: self.paused,
            jobs: self.jobs.clone(),
        };
        let text = serde_json::to_vec_pretty(&snapshot)
            .map_err(|err| io::Error::other(format!("serialize queue: {err}")))?;
        let tmp = self.dir.join(format!("{QUEUE_FILE}.tmp"));
        let final_path = self.dir.join(QUEUE_FILE);
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&text)?;
        file.sync_all()?;
        fs::rename(&tmp, &final_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::testutil::temp_dir;
    use mozart_core::config::Config;

    fn setup(tag: &str, capacity: u32) -> (PathBuf, FileQueue, Storage) {
        let dir = temp_dir(tag);
        let mut config = Config::default();
        config.file.storage_dir = dir.clone();
        let storage = Storage::new(&config).unwrap();
        let queue = FileQueue::load(&dir, capacity).unwrap();
        (dir, queue, storage)
    }

    fn submit(queue: &mut FileQueue, name: &str) -> FileJob {
        let id = queue.reserve_job_id();
        queue.submit(id, name.to_string(), "demo".to_string()).unwrap()
    }

    #[test]
    fn submit_assigns_id_state_and_created() {
        let (dir, mut queue, _storage) = setup("submit", 50);
        let job = submit(&mut queue, "a.wav");
        assert!(job.job_id.starts_with("job_"));
        assert!(job.job_id[4..].parse::<u64>().is_ok());
        assert_eq!(job.state, FileJobState::Queued);
        assert_eq!(job.name, "a.wav");
        assert_eq!(job.model_id, "demo");
        assert!(job.created_unix_ms > 0);
        assert_eq!(queue.next_queued().as_deref(), Some(job.job_id.as_str()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn capacity_counts_unfinished_jobs() {
        let (dir, mut queue, storage) = setup("capacity", 2);
        let a = submit(&mut queue, "a.wav");
        let _b = submit(&mut queue, "b.wav");
        let err = queue.submit(queue.reserve_job_id(), "c.wav".into(), "demo".into());
        let (code, reason) = err.unwrap_err().into_rejection();
        assert_eq!(code, ctl::CODE_BUSY);
        assert!(reason.contains('2'));

        // processing 与 cancelling 都占容量。
        queue.mark_processing(&a.job_id).unwrap();
        assert!(queue
            .submit(queue.reserve_job_id(), "c.wav".into(), "demo".into())
            .is_err());
        queue.cancel(&storage, &a.job_id).unwrap();
        assert!(queue
            .submit(queue.reserve_job_id(), "c.wav".into(), "demo".into())
            .is_err());

        // 终态释放容量。
        queue.mark_cancelled(&a.job_id).unwrap();
        assert!(queue
            .submit(queue.reserve_job_id(), "c.wav".into(), "demo".into())
            .is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn persistence_round_trip_resets_processing() {
        let (dir, mut queue, storage) = setup("persist", 50);
        let a = submit(&mut queue, "a.wav");
        let b = submit(&mut queue, "b.wav");
        queue.mark_processing(&a.job_id).unwrap();
        queue.set_progress(
            &a.job_id,
            FileProgress {
                stage: "infer".to_string(),
                percent: 60,
                blocks_done: Some(3),
                blocks_total: Some(5),
            },
        )
        .unwrap();
        queue.mark_processing(&b.job_id).unwrap();
        queue.cancel(&storage, &b.job_id).unwrap(); // b → cancelling
        queue.pause().unwrap();
        drop(queue);

        let reloaded = FileQueue::load(&dir, 50).unwrap();
        assert!(reloaded.paused());
        let a = reloaded.status(&a.job_id).unwrap();
        assert_eq!(a.state, FileJobState::Queued);
        assert_eq!(a.progress.stage, "decode");
        assert_eq!(a.progress.percent, 0);
        assert!(a.progress.blocks_done.is_none());
        let b = reloaded.status(&b.job_id).unwrap();
        assert_eq!(b.state, FileJobState::Queued);
        assert_eq!(reloaded.unfinished_count(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_missing_snapshot_is_empty_and_corrupt_fails() {
        let (dir, queue, _storage) = setup("load-missing", 50);
        assert!(queue.list().is_empty());
        assert!(!queue.paused());
        fs::write(dir.join(QUEUE_FILE), "not json").unwrap();
        assert!(FileQueue::load(&dir, 50).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancel_queued_deletes_source_and_is_idempotent_by_state() {
        let (dir, mut queue, storage) = setup("cancel", 50);
        let src = dir.join("src.wav");
        fs::write(&src, b"riff").unwrap();
        let job = submit(&mut queue, "src.wav");
        storage.copy_in(&job.job_id, &src).unwrap();
        assert!(storage.source_path(&job.job_id).is_some());

        assert_eq!(queue.cancel(&storage, &job.job_id).unwrap(), FileJobState::Cancelled);
        assert!(storage.source_path(&job.job_id).is_none());
        assert_eq!(queue.status(&job.job_id).unwrap().state, FileJobState::Cancelled);

        // 终态再取消拒绝；未知 job 拒绝。
        let (code, _) = queue.cancel(&storage, &job.job_id).unwrap_err().into_rejection();
        assert_eq!(code, ctl::CODE_INVALID_ARGUMENT);
        let (code, _) = queue.cancel(&storage, "job_42").unwrap_err().into_rejection();
        assert_eq!(code, ctl::CODE_INVALID_ARGUMENT);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancel_processing_marks_cancelling_and_keeps_source() {
        let (dir, mut queue, storage) = setup("cancel-processing", 50);
        let src = dir.join("src.wav");
        fs::write(&src, b"riff").unwrap();
        let job = submit(&mut queue, "src.wav");
        storage.copy_in(&job.job_id, &src).unwrap();
        queue.mark_processing(&job.job_id).unwrap();

        assert_eq!(queue.cancel(&storage, &job.job_id).unwrap(), FileJobState::Cancelling);
        assert!(storage.source_path(&job.job_id).is_some());
        // cancelling 幂等。
        assert_eq!(queue.cancel(&storage, &job.job_id).unwrap(), FileJobState::Cancelling);
        // cancelling 仍占容量、受驱逐保护。
        assert_eq!(queue.unfinished_count(), 1);
        assert_eq!(queue.unfinished_ids(), vec![job.job_id.clone()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancel_intent_wins_over_worker_result() {
        let (dir, mut queue, storage) = setup("cancel-wins", 50);
        let job = submit(&mut queue, "a.wav");
        queue.mark_processing(&job.job_id).unwrap();
        queue.cancel(&storage, &job.job_id).unwrap(); // → cancelling

        // worker 结果晚到：cancelling 一律落 cancelled。
        assert_eq!(
            queue.mark_failed(&job.job_id, "decode failed".into()).unwrap(),
            FileJobState::Cancelled
        );
        assert_eq!(queue.status(&job.job_id).unwrap().state, FileJobState::Cancelled);

        // 已离开 queued 的任务不能再启动。
        let other = submit(&mut queue, "b.wav");
        queue.cancel(&storage, &other.job_id).unwrap(); // queued → cancelled
        assert!(!queue.mark_processing(&other.job_id).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pause_resume_persist_and_list_order() {
        let (dir, mut queue, _storage) = setup("pause", 50);
        let a = submit(&mut queue, "a.wav");
        let b = submit(&mut queue, "b.wav");
        assert!(queue.pause().unwrap());
        assert!(queue.pause().unwrap()); // 幂等
        assert!(!queue.resume().unwrap());
        assert!(queue.pause().unwrap());
        drop(queue);
        let reloaded = FileQueue::load(&dir, 50).unwrap();
        assert!(reloaded.paused());
        let ids: Vec<String> = reloaded.list().into_iter().map(|job| job.job_id).collect();
        assert_eq!(ids, vec![a.job_id, b.job_id]);
        fs::remove_dir_all(&dir).unwrap();
    }
}