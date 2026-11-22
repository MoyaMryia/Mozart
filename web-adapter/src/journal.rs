use crate::error::ApiError;
use sha2::{Digest, Sha256};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const BUFFER_SIZE: usize = 16 * 1024;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

type Hash = [u8; 32];
type Identity = (u64, u64);

fn unavailable(reason: impl std::fmt::Display) -> ApiError {
    ApiError::code("SourceUnavailable", reason.to_string())
}

fn changed(reason: impl std::fmt::Display) -> ApiError {
    ApiError::source_changed(reason.to_string())
}

fn read_error(error: io::Error) -> ApiError {
    if error.kind() == io::ErrorKind::UnexpectedEof { changed(error) } else { unavailable(error) }
}

fn bad_cursor() -> ApiError {
    ApiError::code("BadRequest", "invalid subtitle cursor")
}

fn generation() -> String {
    let mut hash = Sha256::new();
    hash.update(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().to_le_bytes());
    hash.update(std::process::id().to_le_bytes());
    hash.update(NEXT_GENERATION.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn identity(meta: &Metadata) -> Identity { (meta.dev(), meta.ino()) }

// Compare both timestamps and length to detect writes while a scan is in progress.
fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    identity(a) == identity(b) && a.len() == b.len()
        && a.mtime() == b.mtime() && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime() && a.ctime_nsec() == b.ctime_nsec()
}

fn open(path: &Path) -> Result<Option<File>, ApiError> {
    match OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {
        Ok(file) => {
            if !file.metadata().map_err(unavailable)?.is_file() {
                return Err(unavailable("subtitle source is not a regular file"));
            }
            Ok(Some(file))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(unavailable(e)),
    }
}

fn check_path(path: &Path, before: &Metadata, file: &File) -> Result<(), ApiError> {
    let after = file.metadata().map_err(unavailable)?;
    let current = std::fs::metadata(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound { changed(e) } else { unavailable(e) }
    })?;
    if !current.is_file() || !unchanged(before, &after) || !unchanged(&after, &current) {
        return Err(changed("subtitle source changed during read"));
    }
    Ok(())
}

// Hash exactly len bytes, also hashing the first prefix_len bytes separately.
fn hashes(file: &mut File, len: u64, prefix_len: u64) -> Result<(Hash, Hash), ApiError> {
    file.seek(SeekFrom::Start(0)).map_err(unavailable)?;
    let mut full = Sha256::new();
    let mut prefix = Sha256::new();
    let mut pos = 0_u64;
    let mut buf = [0_u8; BUFFER_SIZE];
    while pos < len {
        let n = ((len - pos).min(BUFFER_SIZE as u64)) as usize;
        file.read_exact(&mut buf[..n]).map_err(read_error)?;
        full.update(&buf[..n]);
        let p = prefix_len.saturating_sub(pos).min(n as u64) as usize;
        prefix.update(&buf[..p]);
        pos += n as u64;
    }
    Ok((full.finalize().into(), prefix.finalize().into()))
}

pub struct Journal {
    path: PathBuf,
    max_line_bytes: usize,
    generation: String,
    identity: Option<Identity>,
    len: u64,
    hash: Hash,
    captured: Option<Metadata>,
}

#[derive(Clone)]
pub struct Snapshot {
    pub generation: String,
    pub len: u64,
    path: PathBuf,
    max_line_bytes: usize,
    identity: Option<Identity>,
    captured: Option<Metadata>,
}

impl Journal {
    pub fn new(path: PathBuf, max_line_bytes: usize) -> Self {
        Self { path, max_line_bytes, generation: generation(), identity: None, len: 0, hash: Sha256::digest([]).into(), captured: None }
    }

    pub fn refresh(&mut self, force_reset: bool) -> Result<Snapshot, ApiError> {
        let (id, len, hash, prefix_matches, captured) = match open(&self.path)? {
            None => (None, 0, Sha256::digest([]).into(), self.identity.is_none(), None),
            Some(mut file) => {
                let meta = file.metadata().map_err(unavailable)?;
                let len = meta.len();
                let id = Some(identity(&meta));
                let cached = !force_reset && self.len == len
                    && self.captured.as_ref().is_some_and(|old| unchanged(old, &meta));
                let cached = if cached {
                    match check_path(&self.path, &meta, &file) {
                        Ok(()) => true,
                        Err(error) if error.needs_refresh => false,
                        Err(error) => return Err(error),
                    }
                } else { false };
                if cached {
                    (id, len, self.hash, true, Some(meta))
                } else {
                    let (hash, prefix) = hashes(&mut file, len, self.len.min(len))?;
                    let after = file.metadata().map_err(unavailable)?;
                    if identity(&after) != identity(&meta) || after.len() < len {
                        return Err(changed("subtitle source changed during scan"));
                    }
                    if !unchanged(&meta, &after) {
                        let (confirm, _) = hashes(&mut file, len, 0)?;
                        if confirm != hash { return Err(changed("subtitle source prefix changed during scan")); }
                    }
                    let captured = file.metadata().map_err(unavailable)?;
                    if !unchanged(&after, &captured) {
                        return Err(changed("subtitle source changed during digest confirmation"));
                    }
                    check_path(&self.path, &captured, &file)?;
                    let same_size_rewrite = self.captured.as_ref().is_some_and(|old| {
                        id == self.identity && len == self.len && !unchanged(old, &meta)
                    });
                    (id, len, hash, id == self.identity && len >= self.len && prefix == self.hash && !same_size_rewrite, Some(captured))
                }
            }
        };
        if force_reset || id != self.identity || !prefix_matches {
            self.generation = generation();
        }
        self.identity = id;
        self.len = len;
        self.hash = hash;
        self.captured = captured.clone();
        Ok(Snapshot { generation: self.generation.clone(), len, path: self.path.clone(), max_line_bytes: self.max_line_bytes, identity: id, captured })
    }
}

impl Snapshot {
    fn verify(&self, file: &File) -> Result<(), ApiError> {
        let captured = self.captured.as_ref().ok_or_else(|| changed("subtitle source changed"))?;
        check_path(&self.path, captured, file)
    }

    fn verified_file(&self) -> Result<Option<File>, ApiError> {
        let mut file = open(&self.path)?;
        match (&mut file, self.identity) {
            (Some(f), Some(_)) => self.verify(f)?,
            (None, None) => {},
            _ => return Err(changed("subtitle source changed")),
        }
        Ok(file)
    }

    pub fn cursor(&self, cursor: Option<&str>) -> Result<(u64, bool), ApiError> {
        let Some(cursor) = cursor else { return Ok((0, true)); };
        let (generation, digits) = cursor.split_once(':').ok_or_else(bad_cursor)?;
        if generation.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad_cursor());
        }
        let offset: u64 = digits.parse().map_err(|_| bad_cursor())?;
        if generation != self.generation { return Ok((0, true)); }
        if offset > self.len { return Err(bad_cursor()); }
        if offset != 0 {
            let mut file = self.verified_file()?.ok_or_else(|| changed("subtitle source disappeared"))?;
            file.seek(SeekFrom::Start(offset - 1)).map_err(unavailable)?;
            let mut byte = [0];
            file.read_exact(&mut byte).map_err(read_error)?;
            self.verify(&mut file)?;
            if byte[0] != b'\n' { return Err(bad_cursor()); }
        }
        Ok((offset, false))
    }

    pub fn read_line(&self, offset: u64) -> Result<Option<(String, u64)>, ApiError> {
        let Some(mut file) = self.verified_file()? else { return Ok(None); };
        file.seek(SeekFrom::Start(offset)).map_err(unavailable)?;
        let mut reader = BufReader::new((&mut file).take(self.len - offset));
        let mut line = Vec::new();
        let mut complete = false;
        loop {
            let buf = reader.fill_buf().map_err(unavailable)?;
            if buf.is_empty() { break; }
            let end = buf.iter().position(|&b| b == b'\n');
            let count = end.unwrap_or(buf.len());
            if count > self.max_line_bytes.saturating_sub(line.len()) {
                drop(reader);
                self.verify(&file)?;
                return Err(unavailable("subtitle line exceeds maximum length"));
            }
            line.extend_from_slice(&buf[..count]);
            reader.consume(count + usize::from(end.is_some()));
            if end.is_some() { complete = true; break; }
        }
        drop(reader);
        self.verify(&mut file)?;
        if !complete { return Ok(None); }
        let line = String::from_utf8(line).map_err(unavailable)?;
        let value: serde_json::Value = serde_json::from_str(&line).map_err(unavailable)?;
        if !value.is_object() { return Err(unavailable("subtitle line is not a JSON object")); }
        let next = offset + line.len() as u64 + 1;
        // JSON 仅解析一次；规范化编码避免 JSON 空白中的 CR 破坏 SSE 分行。
        Ok(Some((value.to_string(), next)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn setup(content: &[u8]) -> (tempfile::TempDir, PathBuf, Journal) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        std::fs::write(&path, content).unwrap();
        let journal = Journal::new(path.clone(), 128);
        (dir, path, journal)
    }

    #[test]
    fn append_keeps_generation_and_half_line_waits() {
        let (_dir, path, mut journal) = setup(b"{}\n{\"a\":");
        let a = journal.refresh(false).unwrap();
        let (_, offset) = a.read_line(0).unwrap().unwrap();
        assert!(a.read_line(offset).unwrap().is_none());
        OpenOptions::new().append(true).open(path).unwrap().write_all(b"1}\n").unwrap();
        let b = journal.refresh(false).unwrap();
        assert_eq!(a.generation, b.generation);
        assert_eq!(b.read_line(offset).unwrap().unwrap().0, "{\"a\":1}");
    }

    #[test]
    fn idle_refresh_reuses_digest() {
        let (_dir, _, mut journal) = setup(b"{}\n");
        let first = journal.refresh(false).unwrap();
        // A sentinel digest makes reuse observable without instrumentation.
        journal.hash = [42; 32];
        let second = journal.refresh(false).unwrap();
        assert_eq!(journal.hash, [42; 32]);
        assert_eq!(first.generation, second.generation);
    }

    #[test]
    fn old_snapshot_requests_refresh_after_append() {
        let (_dir, path, mut journal) = setup(b"{}\n");
        let snapshot = journal.refresh(false).unwrap();
        OpenOptions::new().append(true).open(path).unwrap().write_all(b"{}\n").unwrap();
        let error = snapshot.read_line(0).unwrap_err();
        assert_eq!(error.code, "SourceUnavailable");
        assert!(error.needs_refresh);
        assert!(snapshot.cursor(Some(&format!("{}:3", snapshot.generation))).unwrap_err().needs_refresh);
    }

    #[test]
    fn same_size_identical_rewrite_resets() {
        let (_dir, path, mut journal) = setup(b"{}\n");
        let first = journal.refresh(false).unwrap();
        std::fs::write(path, b"{}\n").unwrap();
        assert_ne!(first.generation, journal.refresh(false).unwrap().generation);
    }

    #[test]
    fn concurrent_append_refresh_replays_all_lines() {
        let (_dir, path, mut journal) = setup(b"");
        let writer = std::thread::spawn(move || {
            let mut file = OpenOptions::new().append(true).open(path).unwrap();
            for _ in 0..2000 { file.write_all(b"{}\n").unwrap(); std::thread::yield_now(); }
        });
        while !writer.is_finished() {
            if let Err(error) = journal.refresh(false) { assert!(error.needs_refresh, "{error}"); }
            std::thread::yield_now();
        }
        writer.join().unwrap();
        let snapshot = journal.refresh(false).unwrap();
        let mut offset = 0;
        let mut count = 0;
        while let Some((_, next)) = snapshot.read_line(offset).unwrap() { offset = next; count += 1; }
        assert_eq!(count, 2000);
        assert_eq!(offset, snapshot.len);
    }

    #[test]
    fn invalid_json_is_unavailable() {
        let (_dir, _, mut journal) = setup(b"[]\n");
        let s = journal.refresh(false).unwrap();
        let error = s.read_line(0).unwrap_err();
        assert_eq!(error.code, "SourceUnavailable");
        assert!(!error.needs_refresh);
    }

    #[test]
    fn cursor_boundaries_and_unknown_generation() {
        let (_dir, _, mut journal) = setup(b"{}\n{}\n");
        let s = journal.refresh(false).unwrap();
        assert_eq!(s.cursor(None).unwrap(), (0, true));
        assert_eq!(s.cursor(Some("unknown:3")).unwrap(), (0, true));
        assert_eq!(s.cursor(Some(&format!("{}:3", s.generation))).unwrap(), (3, false));
        assert_eq!(s.cursor(Some(&format!("{}:1", s.generation))).unwrap_err().code, "BadRequest");
        assert_eq!(s.cursor(Some(&format!("{}:7", s.generation))).unwrap_err().code, "BadRequest");
    }

    #[test]
    fn truncate_and_rewrite_same_inode_changes_generation() {
        let (_dir, path, mut journal) = setup(b"{\"old\":1}\n");
        let a = journal.refresh(false).unwrap();
        std::fs::write(path, b"{\"new\":123456}\n").unwrap();
        assert_eq!(a.read_line(0).unwrap_err().code, "SourceUnavailable");
        let b = journal.refresh(false).unwrap();
        assert_ne!(a.generation, b.generation);
    }

    #[test]
    fn forced_reset_changes_generation() {
        let (_dir, _, mut journal) = setup(b"{}\n");
        let a = journal.refresh(false).unwrap();
        let b = journal.refresh(true).unwrap();
        assert_ne!(a.generation, b.generation);
    }

    #[test]
    fn missing_file_waits_and_creation_resets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing");
        let mut journal = Journal::new(path.clone(), 128);
        let a = journal.refresh(false).unwrap();
        assert!(a.read_line(0).unwrap().is_none());
        std::fs::write(path, b"{}\n").unwrap();
        assert_eq!(a.read_line(0).unwrap_err().code, "SourceUnavailable");
        assert_ne!(a.generation, journal.refresh(false).unwrap().generation);
    }
}
