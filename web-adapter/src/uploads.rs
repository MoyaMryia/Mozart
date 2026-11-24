use std::{collections::HashMap, fs::{self, File, OpenOptions}, io::Write, os::fd::AsRawFd, os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt}, path::PathBuf, sync::{Arc, Mutex}, time::Duration};

use axum::{extract::{FromRequest, Multipart, Request}, http::{header, HeaderValue, StatusCode}, response::{IntoResponse, Response}, Json};
use mozart_core::{config::WebAdapterConfig, ctl::{self, MAX_BODY_BYTES}};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::{error::ApiError, rpc::RpcClient};

#[derive(Clone, Serialize, Deserialize)]
struct Peer { boot_id: String, pid: i32, starttime: String, source: String }
struct Retained { peer: Option<Peer>, reason: String, initializing: Option<((u64, u64), bool)>, identity_missing: bool }
#[derive(Default)]
struct State { active: usize, retained: HashMap<PathBuf, Retained> }

pub struct UploadStore {
    dir: PathBuf,
    _lock: File,
    max: usize,
    timeout: Duration,
    rpc: RpcClient,
    state: Mutex<State>,
}

fn internal(e: impl std::fmt::Display) -> ApiError { ApiError::code("Internal", format!("上传存储失败：{e}")) }
fn bad(e: impl std::fmt::Display) -> ApiError { ApiError::code("BadRequest", format!("multipart 无效：{e}")) }
const OWNER_FILE: &str = ".mozart-upload-owner";
const OWNER_MARKER: &[u8] = b"mozart-web-adapter-upload-v1\n";

// A directory name alone is not evidence that it belongs to the adapter.
fn owned(path: &std::path::Path) -> std::io::Result<bool> {
    let meta = match fs::symlink_metadata(path.join(OWNER_FILE)) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_file() { return Ok(false); }
    Ok(fs::read(path.join(OWNER_FILE))? == OWNER_MARKER)
}

fn cleanup(path: &std::path::Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(meta) if !meta.file_type().is_dir() => return Err(std::io::Error::other("上传路径不是目录")),
        Ok(_) => {},
    }
    if !owned(path)? { return Err(std::io::Error::other("上传目录缺少有效的所有权标记")); }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if name == "source" && kind.is_dir() {
            let mut count = 0;
            for source in fs::read_dir(entry.path())? {
                let source = source?;
                count += 1;
                if count > 1 || !source.file_type()?.is_file() {
                    return Err(std::io::Error::other("上传源目录包含未知内容"));
                }
            }
        } else if !(kind.is_file() && (name == OWNER_FILE || name == "manifest.json" || name == "manifest.tmp")) {
            return Err(std::io::Error::other("上传目录包含未知内容"));
        }
    }
    // Keep the marker until every other entry is gone, including after partial failures.
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == OWNER_FILE { continue; }
        if entry.file_type()?.is_dir() { fs::remove_dir_all(entry.path())?; }
        else { fs::remove_file(entry.path())?; }
    }
    fs::remove_file(path.join(OWNER_FILE))?;
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Preserve ownership evidence if removing the now-empty directory fails.
            let mut marker = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path.join(OWNER_FILE))?;
            marker.write_all(OWNER_MARKER)?;
            marker.sync_all()?;
            File::open(path)?.sync_all()?;
            Err(e)
        }
    }
}

// Only directories created by this process can use this path. Never recurse or
// remove entries other than the marker created during initialization.
fn cleanup_initializing(path: &std::path::Path, identity: (u64, u64), marker_created: bool) -> std::io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_dir() || (meta.dev(), meta.ino()) != identity {
        return Err(std::io::Error::other("初始化上传目录的身份已改变"));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !marker_created || entry.file_name() != OWNER_FILE || !entry.file_type()?.is_file() {
            return Err(std::io::Error::other("初始化上传目录包含未知内容"));
        }
    }
    match fs::remove_file(path.join(OWNER_FILE)) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e),
    }
    fs::remove_dir(path)
}

fn initialize(store: &UploadStore, dir: tempfile::TempDir) -> Result<PathBuf, ApiError> {
    let path = dir.path().to_owned();
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) => {
            let path = dir.keep();
            store.state.lock().unwrap().retained.insert(path.clone(), Retained {
                peer: None, initializing: None, identity_missing: true,
                reason: format!("读取初始化目录身份失败：{error}；尚未接收源文件，请检查并移除该空目录后重试：{}", path.display()),
            });
            return Err(internal(error));
        }
    };
    let identity = (meta.dev(), meta.ino());
    let mut marker_created = false;
    let result = (|| -> std::io::Result<()> {
        let mut marker = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path.join(OWNER_FILE))?;
        marker_created = true;
        marker.write_all(OWNER_MARKER)?;
        marker.sync_all()?;
        File::open(&path)?.sync_all()?;
        File::open(&store.dir)?.sync_all()?;
        Ok(())
    })();
    // Disable TempDir's implicit cleanup: failures must be recorded and retried.
    let path = dir.keep();
    match result {
        Ok(()) => Ok(path),
        Err(error) => {
            if let Err(cleanup_error) = cleanup_initializing(&path, identity, marker_created) {
                store.state.lock().unwrap().retained.insert(path, Retained {
                    peer: None, initializing: Some((identity, marker_created)), identity_missing: false,
                    reason: format!("初始化上传目录失败：{error}；清理上传目录失败：{cleanup_error}"),
                });
            }
            Err(internal(error))
        }
    }
}

fn read_boot() -> std::io::Result<String> { Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_owned()) }
fn starttime(pid: i32) -> std::io::Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let tail = stat.rsplit_once(')').ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid proc stat"))?.1;
    tail.split_whitespace().nth(19).map(str::to_owned).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing process start time"))
}
fn peer_for(fd: i32) -> std::io::Result<Peer> {
    // SAFETY: ucred contains integer fields, for which zero is valid.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: cred and len are writable buffers of the declared size.
    if unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) } != 0 { return Err(std::io::Error::last_os_error()); }
    Ok(Peer { boot_id: read_boot()?, pid: cred.pid, starttime: starttime(cred.pid)?, source: String::new() })
}
fn exited(peer: &Peer) -> std::io::Result<bool> {
    if read_boot()? != peer.boot_id { return Ok(true); }
    match starttime(peer.pid) {
        Ok(start) => Ok(start != peer.starttime),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

fn ensure_private_upload_dir(raw: &std::path::Path) -> Result<PathBuf, ApiError> {
    use std::os::unix::fs::DirBuilderExt;
    let dir: PathBuf = raw.components().collect();
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    let created = match builder.create(&dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)
                    .map_err(|err| internal(format!("创建上传父目录 {} 失败：{err}", parent.display())))?;
            }
            match builder.create(&dir) {
                Ok(()) => true,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(err) => return Err(internal(format!("创建上传目录 {} 失败：{err}", dir.display()))),
            }
        }
        Err(e) => return Err(internal(format!("创建上传目录 {} 失败：{e}", dir.display()))),
    };
    if created {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| internal(format!("设置上传目录 {} 权限失败：{e}", dir.display())))?;
    }
    let meta = fs::symlink_metadata(&dir)
        .map_err(|e| internal(format!("读取上传目录 {} 元数据失败：{e}", dir.display())))?;
    if meta.file_type().is_symlink() {
        return Err(internal(format!("上传目录 {} 不能是符号链接", dir.display())));
    }
    if !meta.file_type().is_dir() {
        return Err(internal(format!("上传路径 {} 不是目录", dir.display())));
    }
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(internal(format!(
            "上传目录 {} 属主 uid={} 与当前用户 uid={} 不一致",
            dir.display(),
            meta.uid(),
            euid
        )));
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(internal(format!(
            "上传目录 {} 权限为 {:04o}，必须为当前用户私有 0700（请执行 chmod 700 后重试）",
            dir.display(),
            mode
        )));
    }
    Ok(dir)
}

impl UploadStore {
    pub fn new(config: &WebAdapterConfig, rpc: RpcClient) -> Result<Arc<Self>, ApiError> {
        let dir = ensure_private_upload_dir(&config.web.upload_dir)?;
        let lock = OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(dir.join(".lock")).map_err(internal)?;
        // SAFETY: lock owns a valid descriptor during this call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 { return Err(internal("上传目录已由其他实例占用")); }
        let store = Arc::new(Self { dir, _lock: lock, max: config.web.max_concurrent_uploads, timeout: Duration::from_millis(config.web.upload_timeout_ms), rpc, state: Mutex::new(State::default()) });
        for entry in fs::read_dir(&store.dir).map_err(internal)? {
            let entry = entry.map_err(internal)?;
            if !entry.file_type().map_err(internal)?.is_dir() { continue; }
            let path = entry.path();
            if !owned(&path).map_err(internal)? { continue; }
            let manifest = path.join("manifest.json");
            match fs::read(&manifest) {
                Ok(bytes) => {
                    let peer = serde_json::from_slice::<Peer>(&bytes).map_err(|e| internal(format!("解析上传记录 {} 失败：{e}", manifest.display())))?;
                    store.state.lock().unwrap().retained.insert(path, Retained { peer: Some(peer), reason: "提交结果未知，等待原 daemon 实例退出".into(), initializing: None, identity_missing: false });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Err(e) = cleanup(&path) { store.state.lock().unwrap().retained.insert(path, Retained { peer: None, reason: format!("清理上传目录失败：{e}"), initializing: None, identity_missing: false }); }
                }
                Err(e) => return Err(internal(format!("读取上传记录 {} 失败：{e}", manifest.display()))),
            }
        }
        store.reap();
        Ok(store)
    }

    fn reap(&self) {
        let mut state = self.state.lock().unwrap();
        let mut paths = Vec::new();
        for (path, retained) in &mut state.retained {
            if retained.identity_missing {
                match fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => paths.push(path.clone()),
                    Err(error) => retained.reason = format!("检查初始化隔离目录失败：{error}；恢复访问后检查并移除目录：{}", path.display()),
                    Ok(_) => {},
                }
                continue;
            }
            if let Some(peer) = &retained.peer {
                match exited(peer) {
                    Ok(true) => paths.push(path.clone()),
                    Ok(false) => {},
                    Err(e) => retained.reason = format!("检查原 daemon 实例失败：{e}"),
                }
            } else {
                paths.push(path.clone());
            }
        }
        for path in paths {
            let retained = state.retained.get(&path).unwrap();
            let result = match retained.initializing {
                Some((identity, marker_created)) => cleanup_initializing(&path, identity, marker_created),
                None => cleanup(&path),
            };
            match result {
                Ok(()) => { state.retained.remove(&path); }
                Err(e) => { if let Some(r) = state.retained.get_mut(&path) { r.reason = format!("清理上传目录失败：{e}"); } }
            }
        }
    }

    pub fn health(&self) -> Value {
        self.reap();
        let state = self.state.lock().unwrap();
        json!({"active":state.active,"retained":state.retained.len(),"reasons":state.retained.values().map(|r| r.reason.clone()).collect::<Vec<_>>()})
    }

    pub async fn handle(self: Arc<Self>, request: Request) -> Result<Response, ApiError> {
        self.reap();
        {
            let mut state = self.state.lock().unwrap();
            // Initialization quarantines contain no source data and reserve no transfer slot.
            let reserved = state.retained.values().filter(|record| record.initializing.is_none() && !record.identity_missing).count();
            if state.active + reserved >= self.max { return Err(ApiError::code("UploadBusy", "上传并发额度已满")); }
            state.active += 1;
        }
        let mut guard = UploadGuard { store: self.clone(), path: None, retain: None };
        let receive = async {
            let status = self.rpc.data(ctl::DAEMON_GET_STATUS, json!({})).await?;
            let limit = status.get("file_limits").and_then(|v| v.get("max_upload_bytes")).and_then(Value::as_u64)
                .ok_or_else(|| ApiError::code("Unsupported", "daemon 未提供生效的上传大小上限"))?;
            let content_type = request.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
            if !content_type.split(';').next().is_some_and(|v| v.trim().eq_ignore_ascii_case("multipart/form-data")) {
                return Err(ApiError::code("UnsupportedMediaType", "需要 multipart/form-data"));
            }
            let mut multipart = Multipart::from_request(request, &()).await.map_err(|e| {
                let status = e.status();
                if status == StatusCode::UNSUPPORTED_MEDIA_TYPE { ApiError::code("UnsupportedMediaType", e.to_string()) } else { bad(e) }
            })?;
            let mut file_path = None;
            let mut model_id: Option<String> = None;
            let mut text_bytes = 0usize;
            while let Some(mut field) = multipart.next_field().await.map_err(bad)? {
                match field.name() {
                    Some("file") => {
                        if file_path.is_some() { return Err(bad("重复 file 字段")); }
                        let name = field.file_name().filter(|n| !n.is_empty() && *n != "." && *n != ".." && !n.contains(['/', '\\', '\0', '\r', '\n']))
                            .ok_or_else(|| bad("非法文件名"))?.to_owned();
                        let dir = tempfile::Builder::new().prefix("upload-").tempdir_in(&self.dir).map_err(internal)?;
                        let path = initialize(&self, dir)?;
                        guard.path = Some(path.clone());
                        let source_dir = path.join("source");
                        fs::create_dir(&source_dir).map_err(internal)?;
                        fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o700)).map_err(internal)?;
                        let target = source_dir.join(name);
                        let mut out = tokio::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&target).await.map_err(internal)?;
                        let mut size = 0u64;
                        while let Some(chunk) = field.chunk().await.map_err(bad)? {
                            size = size.checked_add(chunk.len() as u64).ok_or_else(|| ApiError::code("PayloadTooLarge", "文件超过上传上限"))?;
                            if size > limit { return Err(ApiError::code("PayloadTooLarge", "文件超过上传上限")); }
                            out.write_all(&chunk).await.map_err(internal)?;
                        }
                        out.flush().await.map_err(internal)?;
                        drop(out);
                        file_path = Some(target);
                    }
                    Some("model_id") => {
                        if model_id.is_some() { return Err(bad("重复 model_id 字段")); }
                        let mut bytes = Vec::new();
                        while let Some(chunk) = field.chunk().await.map_err(bad)? {
                            text_bytes = text_bytes.saturating_add(chunk.len());
                            if text_bytes > MAX_BODY_BYTES { return Err(ApiError::code("PayloadTooLarge", "文本字段超过上限")); }
                            bytes.extend_from_slice(&chunk);
                        }
                        model_id = Some(String::from_utf8(bytes).map_err(bad)?);
                    }
                    _ => return Err(bad("未知字段")),
                }
            }
            Ok((file_path.ok_or_else(|| bad("缺少 file 字段"))?, model_id))
        };
        let (file, model) = tokio::time::timeout(self.timeout, receive).await.map_err(|_| ApiError::code("UploadTimeout", "接收上传超时"))??;
        // Move the entire guard into the blocking task, independent of HTTP cancellation.
        tokio::task::spawn_blocking(move || guard.submit(file, model))
            .await.map_err(|e| ApiError::code("Internal", format!("上传提交任务失败：{e}")))?
    }
}

struct UploadGuard { store: Arc<UploadStore>, path: Option<PathBuf>, retain: Option<Retained> }
impl UploadGuard {
    fn submit(mut self, file: PathBuf, model: Option<String>) -> Result<Response, ApiError> {
            let mut session = self.store.rpc.connect(ctl::DAEMON_FILE_SUBMIT)?;
            let mut peer = peer_for(session.conn.as_raw_fd()).map_err(internal)?;
            peer.source = file.strip_prefix(self.path.as_ref().unwrap()).map_err(internal)?.to_string_lossy().into_owned();
            let dir = self.path.as_ref().unwrap();
            let temporary = dir.join("manifest.tmp");
            let mut manifest = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary).map_err(internal)?;
            serde_json::to_writer(&mut manifest, &peer).map_err(internal)?;
            manifest.flush().map_err(internal)?;
            manifest.sync_all().map_err(internal)?;
            fs::rename(temporary, dir.join("manifest.json")).map_err(internal)?;
            File::open(dir).and_then(|f| f.sync_all()).map_err(internal)?;
            let mut body = json!({"path":file});
            if let Some(model) = model { body["model_id"] = json!(model); }
            self.retain = Some(Retained { peer: Some(peer), reason: "提交结果未知，等待原 daemon 实例退出".into(), initializing: None, identity_missing: false });
            let response = match session.call(ctl::DAEMON_FILE_SUBMIT, body) {
                Ok(response) => response,
                Err((error, unknown)) => { if !unknown { self.retain = None; } return Err(error); }
            };
            if response.status == "error" { self.retain = None; return Err(ApiError::remote(response)); }
            let invalid = || ApiError::code("BadGateway", "受理响应结构无效；提交结果未知，请查询状态");
            let job = response.data.get("job_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(invalid)?;
            response.data.get("state").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(invalid)?;
            response.data.get("queue_position").and_then(Value::as_u64).ok_or_else(invalid)?;
            let encoded: String = url::form_urlencoded::byte_serialize(job.as_bytes()).collect();
            let location = HeaderValue::from_str(&format!("/api/v1/files/{encoded}")).map_err(internal)?;
            self.retain = None;
            Ok((StatusCode::ACCEPTED, [(header::LOCATION, location)], Json(response)).into_response())
    }
}
impl Drop for UploadGuard {
    fn drop(&mut self) {
        let mut state = self.store.state.lock().unwrap();
        state.active -= 1;
        if let Some(path) = self.path.take() {
            let retained = self.retain.take().or_else(|| match cleanup(&path) {
                Ok(()) => None,
                Err(e) => Some(Retained { peer: None, reason: format!("清理上传目录失败：{e}"), initializing: None, identity_missing: false }),
            });
            if let Some(retained) = retained { state.retained.insert(path, retained); }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use super::*;
    use axum::{body::Body, extract::DefaultBodyLimit};
    use mozart_core::ctl::Seqpacket;
    use std::{process::{Child, Command, Stdio}, time::Instant};

    const LIMIT: usize = 3 * 1024 * 1024;

    #[test]
    fn upload_directory_rejects_symlink_and_nonprivate_without_chmod() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut config = WebAdapterConfig::load_from(None).unwrap();
        config.web.upload_dir = link.clone();
        let link_err = UploadStore::new(&config, RpcClient::new(None, config.clone())).err().unwrap();
        assert!(link_err.reason.contains(&link.display().to_string()), "{}", link_err.reason);
        config.web.upload_dir = target.clone();
        let mode_err = UploadStore::new(&config, RpcClient::new(None, config.clone())).err().unwrap();
        assert!(mode_err.reason.contains(&target.display().to_string()), "{}", mode_err.reason);
        assert!(mode_err.reason.contains("0755"), "{}", mode_err.reason);
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o755);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        config.web.upload_dir = link;
        assert!(UploadStore::new(&config, RpcClient::new(None, config.clone())).is_err());
        config.web.upload_dir = target;
        assert!(UploadStore::new(&config, RpcClient::new(None, config.clone())).is_ok());
    }

    #[test]
    fn upload_directory_creates_missing_parents_with_0700() {
        let root = tempfile::tempdir().unwrap();
        let upload_dir = root.path().join("nested/state/web-uploads");
        let mut config = WebAdapterConfig::load_from(None).unwrap();
        config.web.upload_dir = upload_dir.clone();
        assert!(UploadStore::new(&config, RpcClient::new(None, config.clone())).is_ok());
        let meta = fs::symlink_metadata(&upload_dir).unwrap();
        assert!(meta.file_type().is_dir());
        assert_eq!(meta.mode() & 0o7777, 0o700);
    }

    // This test is invoked only as a separate process so SO_PEERCRED identifies it.
    #[test]
    #[ignore]
    fn mock_peer() {
        let socket = std::env::var_os("UPLOAD_MOCK_SOCKET").unwrap();
        let notice = PathBuf::from(std::env::var_os("UPLOAD_MOCK_NOTICE").unwrap());
        let mode = std::env::var("UPLOAD_MOCK_MODE").unwrap();
        let listener = Seqpacket::bind(Path::new(&socket)).unwrap();
        listener.listen(16).unwrap();
        fs::write(notice.with_extension("ready"), b"ready").unwrap();
        loop {
            let conn = listener.accept().unwrap();
            let notice = notice.clone();
            let mode = mode.clone();
            std::thread::spawn(move || {
                let request = conn.recv(Some(Duration::from_secs(5))).unwrap();
                let data = match request.header.type_ {
                    ctl::DAEMON_GET_STATUS => json!({"file_limits":{"max_upload_bytes":LIMIT}}),
                    ctl::DAEMON_FILE_SUBMIT => {
                        let body: Value = serde_json::from_slice(&request.body).unwrap();
                        let temp = tempfile::NamedTempFile::new_in(notice.parent().unwrap()).unwrap();
                        fs::write(temp.path(), serde_json::to_vec(&body).unwrap()).unwrap();
                        temp.persist(&notice).unwrap();
                        if mode == "hold" { std::thread::sleep(Duration::from_secs(60)); return; }
                        if mode == "invalid" { json!({"job_id":"job"}) }
                        else { json!({"job_id":"job", "state":"queued", "queue_position":1}) }
                    }
                    _ => panic!("unexpected command"),
                };
                let reply = json!({"status":"ok", "code":"Ok", "reason":"", "data":data});
                conn.send(request.header.type_, request.header.request_id, &serde_json::to_vec(&reply).unwrap()).unwrap();
            });
        }
    }

    struct Fixture { _dir: tempfile::TempDir, config: WebAdapterConfig, child: Child, notice: PathBuf }
    impl Fixture {
        fn new(mode: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut config = WebAdapterConfig::load_from(None).unwrap();
            config.daemon.run_dir = dir.path().to_owned();
            config.daemon.socket = "mock.sock".into();
            config.daemon.rpc_timeout_ms = 700;
            config.web.upload_dir = dir.path().join("uploads");
            let notice = dir.path().join("notice.json");
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "uploads::tests::mock_peer"])
                .env("UPLOAD_MOCK_SOCKET", config.daemon_socket())
                .env("UPLOAD_MOCK_NOTICE", &notice).env("UPLOAD_MOCK_MODE", mode)
                .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let fixture = Self { _dir: dir, config, child, notice };
            let start = Instant::now();
            while !fixture.notice.with_extension("ready").exists() {
                assert!(start.elapsed() < Duration::from_secs(5), "mock peer did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            fixture
        }
        fn store(&self) -> Arc<UploadStore> {
            UploadStore::new(&self.config, RpcClient::new(None, self.config.clone())).unwrap()
        }
        fn stop(&mut self) { self.child.kill().unwrap(); self.child.wait().unwrap(); }
        async fn submitted(&self) -> Value {
            let start = Instant::now();
            loop {
                if let Ok(data) = fs::read(&self.notice) { return serde_json::from_slice(&data).unwrap(); }
                assert!(start.elapsed() < Duration::from_secs(5), "submit not received");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
    }

    fn request(parts: Vec<(&str, Option<&str>, Vec<u8>)>) -> Request {
        let mut body = Vec::new();
        for (name, filename, data) in parts {
            body.extend_from_slice(format!("--test-boundary\r\nContent-Disposition: form-data; name=\"{name}\"{}\r\n\r\n", filename.map(|f| format!("; filename=\"{f}\"")).unwrap_or_default()).as_bytes());
            body.extend_from_slice(&data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--test-boundary--\r\n");
        let mut request = Request::builder().header(header::CONTENT_TYPE, "multipart/form-data; boundary=test-boundary").body(Body::from(body)).unwrap();
        DefaultBodyLimit::disable().apply(&mut request);
        request
    }
    fn file(data: Vec<u8>) -> (&'static str, Option<&'static str>, Vec<u8>) { ("file", Some("voice.wav"), data) }
    fn model() -> (&'static str, Option<&'static str>, Vec<u8>) { ("model_id", None, b"voice".to_vec()) }

    #[test]
    fn recovery_preserves_unknown_and_cleans_owned_receiving_directory() {
        let fixture = Fixture::new("ack");
        let root = &fixture.config.web.upload_dir;
        let unknown = root.join("upload-preexisting");
        fs::create_dir_all(&unknown).unwrap();
        fs::set_permissions(&fixture.config.web.upload_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(unknown.join("valuable"), b"keep").unwrap();
        let receiving = root.join("upload-receiving");
        fs::create_dir(&receiving).unwrap();
        fs::write(receiving.join(OWNER_FILE), OWNER_MARKER).unwrap();
        fs::create_dir(receiving.join("source")).unwrap();
        fs::write(receiving.join("source/partial.wav"), b"partial").unwrap();
        let store = fixture.store();
        assert_eq!(store.health()["retained"], 0);
        assert_eq!(fs::read(unknown.join("valuable")).unwrap(), b"keep");
        assert!(!receiving.exists());
    }

    #[test]
    fn failed_cleanup_is_visible_and_retried() {
        let mut fixture = Fixture::new("ack");
        fixture.config.web.max_concurrent_uploads = 1;
        let store = fixture.store();
        let path = store.dir.join("upload-retry");
        fs::create_dir(&path).unwrap();
        fs::write(path.join(OWNER_FILE), OWNER_MARKER).unwrap();
        let source = path.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("partial.wav"), b"partial").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
        // Verify that this process actually enforces the permissions used by the test.
        assert!(fs::read_dir(&source).is_err(), "test requires effective directory permissions");
        store.state.lock().unwrap().active = 1;
        drop(UploadGuard { store: store.clone(), path: Some(path.clone()), retain: None });
        let health = store.health();
        assert_eq!(health["retained"], 1);
        assert!(health["reasons"][0].as_str().unwrap().contains("清理上传目录失败"));
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(store.health()["retained"], 0);
        let state = store.state.lock().unwrap();
        assert!(state.active + state.retained.len() < store.max);
        assert!(!path.exists());
    }

    #[test]
    fn marker_io_failure_cleans_or_retains_for_retry() {
        let mut fixture = Fixture::new("ack");
        fixture.config.web.max_concurrent_uploads = 1;
        let store = fixture.store();
        for block_cleanup in [false, true] {
            let dir = tempfile::Builder::new().prefix("upload-").tempdir_in(&store.dir).unwrap();
            let path = dir.path().to_owned();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
            assert!(File::create(path.join("permission-probe")).is_err(), "test requires effective directory permissions");
            if block_cleanup { fs::set_permissions(&store.dir, fs::Permissions::from_mode(0o500)).unwrap(); }
            store.state.lock().unwrap().active = 1;
            let guard = UploadGuard { store: store.clone(), path: None, retain: None };
            assert!(initialize(&store, dir).is_err());
            drop(guard);
            let health = store.health();
            assert_eq!(health["active"], 0);
            assert_eq!(health["retained"], usize::from(block_cleanup));
            if block_cleanup {
                assert!(health["reasons"][0].as_str().unwrap().contains("清理上传目录失败"));
                assert!(path.exists());
                fs::set_permissions(&store.dir, fs::Permissions::from_mode(0o700)).unwrap();
                assert_eq!(store.health()["retained"], 0);
            }
            assert!(!path.exists());
            let state = store.state.lock().unwrap();
            assert!(state.active + state.retained.len() < store.max);
        }
    }

    #[test]
    fn recovery_preserves_unexpected_contents_in_owned_directory() {
        let fixture = Fixture::new("ack");
        let path = fixture.config.web.upload_dir.join("upload-owned");
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&fixture.config.web.upload_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join(OWNER_FILE), OWNER_MARKER).unwrap();
        fs::create_dir(path.join("unexpected")).unwrap();
        fs::write(path.join("unexpected/valuable"), b"keep").unwrap();
        let store = fixture.store();
        assert_eq!(store.health()["retained"], 1);
        assert_eq!(fs::read(path.join("unexpected/valuable")).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn multipart_both_orders_submit_and_cleanup() {
        let fixture = Fixture::new("ack");
        let store = fixture.store();
        for parts in [vec![file(vec![7; 2 * 1024 * 1024 + 1]), model()], vec![model(), file(b"audio".to_vec())]] {
            let response = store.clone().handle(request(parts)).await.unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert_eq!(response.headers()[header::LOCATION], "/api/v1/files/job");
            let submitted = fixture.submitted().await;
            assert_eq!(submitted["model_id"], "voice");
            assert!(!Path::new(submitted["path"].as_str().unwrap()).exists());
            fs::remove_file(&fixture.notice).unwrap();
            assert_eq!(store.health()["active"], 0);
            assert_eq!(store.health()["retained"], 0);
        }
    }

    #[tokio::test]
    async fn oversized_and_duplicate_fields_cleanup() {
        let fixture = Fixture::new("ack");
        let store = fixture.store();
        for (parts, code) in [(vec![file(vec![0; LIMIT + 1])], "PayloadTooLarge"), (vec![file(vec![1]), file(vec![2])], "BadRequest"), (vec![file(vec![1]), model(), model()], "BadRequest")] {
            assert_eq!(store.clone().handle(request(parts)).await.unwrap_err().code, code);
            assert_eq!(store.health()["active"], 0);
            assert_eq!(store.health()["retained"], 0);
        }
        assert!(!fixture.notice.exists());
        assert_eq!(fs::read_dir(&fixture.config.web.upload_dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cancellation_timeout_recovery_and_peer_exit() {
        let mut fixture = Fixture::new("hold");
        let store = fixture.store();
        let task = tokio::spawn(store.clone().handle(request(vec![file(b"audio".to_vec())])));
        let submitted = fixture.submitted().await;
        let source = PathBuf::from(submitted["path"].as_str().unwrap());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(source.exists());
        assert_eq!(store.health()["active"], 1);
        let start = Instant::now();
        while store.health()["active"] != 0 {
            assert!(start.elapsed() < Duration::from_secs(5));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(store.health()["retained"], 1);
        drop(store);
        let store = fixture.store();
        assert_eq!(store.health()["retained"], 1);
        assert!(source.exists());
        fixture.stop();
        assert_eq!(store.health()["retained"], 0);
        assert!(!source.exists());
    }

    #[tokio::test]
    async fn invalid_ack_retains_and_corrupt_manifest_stops_recovery() {
        let fixture = Fixture::new("invalid");
        let store = fixture.store();
        assert_eq!(store.clone().handle(request(vec![file(b"audio".to_vec())])).await.unwrap_err().code, "BadGateway");
        let submitted = fixture.submitted().await;
        let source = PathBuf::from(submitted["path"].as_str().unwrap());
        assert!(source.exists());
        assert_eq!(store.health()["retained"], 1);
        let manifest = source.parent().unwrap().parent().unwrap().join("manifest.json");
        drop(store);
        fs::write(&manifest, b"not json").unwrap();
        let result = UploadStore::new(&fixture.config, RpcClient::new(None, fixture.config.clone()));
        assert!(result.is_err());
        assert!(manifest.exists());
        assert!(source.exists());
    }
}
