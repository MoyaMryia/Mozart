//! 受管存储：源副本复制入、结果/临时路径、按旧 ModeController::evict_cache 口径驱逐。
//! 命名布局（D4）：源 `<job_id>.input<ext>`、结果 `<job_id>.output.wav`、
//! 临时 `<job_id>.decoded.f32` / `<job_id>.output.f32` / `<job_id>.output.wav.part`。

use mozart_core::config::Config;
use mozart_core::ctl;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 入口扩展名白名单（SPEC-04 文件转换入口），唯一定义处。
const SOURCE_EXTENSIONS: [&str; 6] = ["wav", "mp3", "m4a", "aac", "flac", "ogg"];

/// 驱逐目标：超上限后删至上限的 80%（沿用旧 evict_cache 口径）。
const EVICT_TARGET_PERCENT: u64 = 80;

/// 队列快照文件名，驱逐永不触碰。
pub(crate) const QUEUE_FILE: &str = "queue.json";

#[derive(Debug)]
pub enum StorageError {
    InvalidExtension(String),
    TooLarge { size: u64, limit: u64 },
    SourceUnreadable(io::Error),
    Io(io::Error),
}

impl StorageError {
    pub fn into_rejection(self) -> (String, String) {
        match self {
            StorageError::InvalidExtension(ext) => (
                ctl::CODE_INVALID_ARGUMENT.to_string(),
                format!("unsupported file extension: {ext}"),
            ),
            StorageError::TooLarge { size, limit } => (
                ctl::CODE_INVALID_ARGUMENT.to_string(),
                format!("source size {size} exceeds upload limit {limit}"),
            ),
            StorageError::SourceUnreadable(err) => (
                ctl::CODE_INVALID_ARGUMENT.to_string(),
                format!("source not readable: {err}"),
            ),
            StorageError::Io(err) => (ctl::CODE_INTERNAL.to_string(), err.to_string()),
        }
    }
}

pub struct Storage {
    dir: PathBuf,
    max_upload_bytes: u64,
    storage_limit_bytes: u64,
}

impl Storage {
    pub fn new(config: &Config) -> io::Result<Self> {
        let dir = config.file_storage_dir();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            max_upload_bytes: config.file.max_upload_bytes,
            storage_limit_bytes: config.file.storage_limit_bytes,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 入口校验一次：扩展名白名单、大小预检，然后复制到 `<job_id>.input<ext>`。
    pub fn copy_in(&self, job_id: &str, source: &Path) -> Result<PathBuf, StorageError> {
        let ext = source
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_default();
        if !SOURCE_EXTENSIONS.contains(&ext.as_str()) {
            return Err(StorageError::InvalidExtension(ext));
        }
        let meta = fs::metadata(source).map_err(StorageError::SourceUnreadable)?;
        if !meta.is_file() {
            return Err(StorageError::SourceUnreadable(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            )));
        }
        if meta.len() > self.max_upload_bytes {
            return Err(StorageError::TooLarge {
                size: meta.len(),
                limit: self.max_upload_bytes,
            });
        }
        let dest = self.dir.join(format!("{job_id}.input.{ext}"));
        if let Err(err) = fs::copy(source, &dest) {
            // 半成品副本不留（可能已写入部分字节）。
            let _ = fs::remove_file(&dest);
            return Err(StorageError::Io(err));
        }
        Ok(dest)
    }

    /// 源副本路径（扩展名不定，按前缀找）。
    pub fn source_path(&self, job_id: &str) -> Option<PathBuf> {
        let prefix = format!("{job_id}.input");
        let entries = fs::read_dir(&self.dir).ok()?;
        entries.flatten().find_map(|entry| {
            let name = entry.file_name();
            name.to_str()
                .filter(|name| name.starts_with(&prefix))
                .map(|_| entry.path())
        })
    }

    pub fn result_path(&self, job_id: &str) -> PathBuf {
        self.dir.join(format!("{job_id}.output.wav"))
    }

    pub fn decoded_path(&self, job_id: &str) -> PathBuf {
        self.dir.join(format!("{job_id}.decoded.f32"))
    }

    pub fn output_raw_path(&self, job_id: &str) -> PathBuf {
        self.dir.join(format!("{job_id}.output.f32"))
    }

    pub fn result_part_path(&self, job_id: &str) -> PathBuf {
        self.dir.join(format!("{job_id}.output.wav.part"))
    }

    /// 删源副本（取消排队任务时）。文件已不在视为已删；
    /// 其余删除失败留给驱逐兜底（任务已终态，不再受保护）。
    pub fn remove_source(&self, job_id: &str) {
        if let Some(path) = self.source_path(job_id) {
            let _ = fs::remove_file(path);
        }
    }

    /// 清临时文件（取消/失败/收尾）。已不在的跳过。
    pub fn cleanup_temp(&self, job_id: &str) {
        for path in [
            self.decoded_path(job_id),
            self.output_raw_path(job_id),
            self.result_part_path(job_id),
        ] {
            let _ = fs::remove_file(path);
        }
    }

    /// 每任务后驱逐：目录总大小超上限时按文件名时间戳从旧到新删至 80%。
    /// 未完成任务（queued/processing/cancelling）的全部文件受保护；
    /// queue.json 永不驱逐。单文件删除失败跳过（可能已被并发删除），
    /// 留待下一轮，不阻断队列。
    pub fn evict(&self, unfinished_ids: &[String]) -> io::Result<()> {
        let mut entries: Vec<(String, u64)> = Vec::new();
        let mut total: u64 = 0;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let meta = match entry.metadata() {
                Ok(meta) if meta.is_file() => meta,
                _ => continue,
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == QUEUE_FILE {
                continue;
            }
            total += meta.len();
            entries.push((name, meta.len()));
        }
        if total <= self.storage_limit_bytes {
            return Ok(());
        }
        let target = self.storage_limit_bytes * EVICT_TARGET_PERCENT / 100;
        entries.sort();
        for (name, size) in entries {
            if total <= target {
                break;
            }
            if unfinished_ids
                .iter()
                .any(|id| name.starts_with(&format!("{id}.")))
            {
                continue;
            }
            if fs::remove_file(self.dir.join(&name)).is_ok() {
                total -= size;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::testutil::temp_dir;

    fn storage(dir: &Path, limit: u64) -> Storage {
        Storage {
            dir: dir.to_path_buf(),
            max_upload_bytes: 1024,
            storage_limit_bytes: limit,
        }
    }

    fn write(path: &Path, bytes: usize) {
        fs::write(path, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn copy_in_checks_extension_and_size_once() {
        let dir = temp_dir("copy-in");
        let store = storage(&dir, 1_000_000);
        let src = dir.join("src.wav");
        write(&src, 16);
        let dest = store.copy_in("job_1", &src).unwrap();
        assert_eq!(dest, dir.join("job_1.input.wav"));
        assert!(dest.is_file());

        // 大小写不敏感；白名单外拒绝。
        let upper = dir.join("src.WAV");
        write(&upper, 16);
        assert!(store.copy_in("job_2", &upper).is_ok());
        let bad = dir.join("src.txt");
        write(&bad, 16);
        let (code, _) = store.copy_in("job_3", &bad).unwrap_err().into_rejection();
        assert_eq!(code, ctl::CODE_INVALID_ARGUMENT);

        // 大小预检：超限拒绝，不产生副本。
        let big = dir.join("big.mp3");
        write(&big, 2048);
        let (code, reason) = store.copy_in("job_4", &big).unwrap_err().into_rejection();
        assert_eq!(code, ctl::CODE_INVALID_ARGUMENT);
        assert!(reason.contains("1024"));
        assert!(store.source_path("job_4").is_none());

        // 不存在/目录拒绝。
        let (code, _) = store
            .copy_in("job_5", &dir.join("missing.wav"))
            .unwrap_err()
            .into_rejection();
        assert_eq!(code, ctl::CODE_INVALID_ARGUMENT);
        let (code, _) = store.copy_in("job_6", &dir).unwrap_err().into_rejection();
        assert_eq!(code, ctl::CODE_INVALID_ARGUMENT);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn source_and_temp_paths_follow_layout() {
        let dir = temp_dir("paths");
        let store = storage(&dir, 1_000_000);
        assert_eq!(store.result_path("job_9"), dir.join("job_9.output.wav"));
        assert_eq!(store.decoded_path("job_9"), dir.join("job_9.decoded.f32"));
        assert_eq!(store.output_raw_path("job_9"), dir.join("job_9.output.f32"));
        assert_eq!(
            store.result_part_path("job_9"),
            dir.join("job_9.output.wav.part")
        );
        write(&dir.join("job_9.input.flac"), 8);
        assert_eq!(store.source_path("job_9"), Some(dir.join("job_9.input.flac")));
        assert_eq!(store.source_path("job_8"), None);

        write(&store.decoded_path("job_9"), 8);
        write(&store.output_raw_path("job_9"), 8);
        write(&store.result_part_path("job_9"), 8);
        store.cleanup_temp("job_9");
        assert!(!store.decoded_path("job_9").exists());
        assert!(!store.output_raw_path("job_9").exists());
        assert!(!store.result_part_path("job_9").exists());
        assert!(store.source_path("job_9").is_some());

        store.remove_source("job_9");
        assert!(store.source_path("job_9").is_none());
        // 重复删除不报错。
        store.remove_source("job_9");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn evict_deletes_oldest_to_target_and_protects_unfinished() {
        let dir = temp_dir("evict");
        let store = storage(&dir, 1000);
        // 已完成任务时间戳从旧到新；job_4000 未完成，其全部文件受保护。
        write(&dir.join("job_1000.output.wav"), 400);
        write(&dir.join("job_2000.output.wav"), 400);
        write(&dir.join("job_3000.output.wav"), 200);
        write(&dir.join("job_4000.output.wav"), 100);
        write(&dir.join("job_4000.input.wav"), 100);
        write(&dir.join("job_4000.decoded.f32"), 100);
        // queue.json 是元数据，不计入缓存总大小，也永不驱逐。
        write(&dir.join(QUEUE_FILE), 100);
        // 总 1300 > 1000，目标 800：删两个最旧（各 400）后 500 达标。
        store.evict(&["job_4000".to_string()]).unwrap();
        assert!(!dir.join("job_1000.output.wav").exists());
        assert!(!dir.join("job_2000.output.wav").exists());
        assert!(dir.join("job_3000.output.wav").exists());
        assert!(dir.join("job_4000.output.wav").exists());
        assert!(dir.join("job_4000.input.wav").exists());
        assert!(dir.join("job_4000.decoded.f32").exists());
        assert!(dir.join(QUEUE_FILE).exists());

        // 未超上限不动。
        let before = fs::read_dir(&dir).unwrap().count();
        store.evict(&[]).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn evict_keeps_all_when_protected_covers_everything() {
        let dir = temp_dir("evict-protected");
        let store = storage(&dir, 100);
        write(&dir.join("job_1000.output.wav"), 400);
        store.evict(&["job_1000".to_string()]).unwrap();
        assert!(dir.join("job_1000.output.wav").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}