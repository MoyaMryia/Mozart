use std::{convert::Infallible, path::{Path, PathBuf}, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}}, time::Duration};
use async_stream::stream;
use axum::{body::{Body, Bytes}, http::header, response::Response};
use mozart_core::{config::WebAdapterConfig, ctl::DAEMON_GET_STATUS};
use notify::{event::{AccessKind, AccessMode}, EventKind, RecursiveMode, Watcher};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use crate::{error::ApiError, journal::{Journal, Snapshot}, rpc::RpcClient, transport::Transport};

#[derive(Clone)]
struct Publication<T> { epoch: u64, result: Result<T, ApiError> }
type Latest<T> = watch::Receiver<Option<Publication<T>>>;
#[derive(Clone)]
pub struct Streams { inner: Arc<Inner> }
struct Inner {
    rpc: RpcClient, config: WebAdapterConfig,
    stop: watch::Sender<bool>,
    status_wake: watch::Sender<u64>, journal_wake: watch::Sender<u64>,
    status: Mutex<Option<Latest<Value>>>, journal: Mutex<Option<Latest<Snapshot>>>,
}
fn internal(error: impl std::fmt::Display) -> ApiError { ApiError::code("Internal", error.to_string()) }
fn source(error: impl std::fmt::Display) -> ApiError { ApiError::code("SourceUnavailable", error.to_string()) }
fn wake(sender: &watch::Sender<u64>) -> u64 {
    let mut epoch = 0;
    sender.send_modify(|value| { *value = value.wrapping_add(1); epoch = *value; }); epoch
}
fn event(name: &str, data: &str, id: Option<&str>) -> Bytes {
    let prefix = id.map(|id| format!("id: {id}\n")).unwrap_or_default();
    Bytes::from(format!("{prefix}event: {name}\ndata: {data}\n\n"))
}
fn reset(generation: &str) -> Bytes { event("reset", &json!({"generation":generation}).to_string(), None) }
fn error_frame(error: &ApiError) -> Bytes { event("error", &error.body().to_string(), None) }
fn fit(frame: Bytes, limit: usize) -> Result<Bytes, ApiError> {
    if frame.len() > limit { Err(source("SSE event exceeds client buffer")) } else { Ok(frame) }
}
async fn first<T: Clone>(receiver: &mut Latest<T>, stop: &mut watch::Receiver<bool>, requested: u64) -> Result<T, ApiError> {
    loop {
        if *stop.borrow() { return Err(source("adapter is shutting down")); }
        if let Some(publication) = receiver.borrow_and_update().clone() {
            if publication.epoch >= requested { return publication.result; }
        }
        tokio::select! {
            _ = stop.changed() => return Err(source("adapter is shutting down")),
            changed = receiver.changed() => { changed.map_err(source)?; },
        }
    }
}

// Count encoded frames, including the frame currently handed to the HTTP writer.
// Producers never await a slow socket. Overflow cancels the body and closes its TCP connection.
struct Queued { bytes: Bytes, pending: Arc<AtomicUsize>, weight: usize }
impl Drop for Queued { fn drop(&mut self) { self.pending.fetch_sub(self.weight, Ordering::AcqRel); } }
struct CancelOnDrop(watch::Sender<bool>);
impl Drop for CancelOnDrop { fn drop(&mut self) { self.0.send_replace(true); } }
struct Client {
    sender: mpsc::UnboundedSender<Queued>, pending: Arc<AtomicUsize>, limit: usize,
    cancel: watch::Sender<bool>, stopped: watch::Receiver<bool>, transport: Option<Transport>,
}
impl Client {
    fn send(&self, bytes: Bytes) -> bool {
        if *self.stopped.borrow() { return false; }
        let weight = bytes.len();
        let mut pending = self.pending.load(Ordering::Acquire);
        loop {
            let Some(next) = pending.checked_add(weight).filter(|sum| *sum <= self.limit) else {
                eprintln!("SSE client buffer exceeded (limit={} bytes)", self.limit);
                self.cancel.send_replace(true);
                if let Some(transport) = &self.transport { transport.close(); }
                return false;
            };
            match self.pending.compare_exchange_weak(pending, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(current) => pending = current,
            }
        }
        self.sender.send(Queued { bytes, pending: self.pending.clone(), weight }).is_ok()
    }
    fn fail(&self, error: &ApiError) { eprintln!("SSE source: {error}"); self.send(error_frame(error)); }
}
fn channel(limit: usize, transport: Option<Transport>, mut stop: watch::Receiver<bool>) -> (Client, Response) {
    let (sender, mut receiver) = mpsc::unbounded_channel::<Queued>();
    let (cancel, mut cancelled) = watch::channel(false);
    let client = Client { sender, pending: Arc::new(AtomicUsize::new(0)), limit, cancel: cancel.clone(), stopped: cancel.subscribe(), transport };
    let guard = CancelOnDrop(cancel);
    let frames = stream! {
        let _guard = guard;
        loop {
            if *cancelled.borrow() || *stop.borrow() { break; }
            tokio::select! {
                _ = cancelled.changed() => break,
                _ = stop.changed() => break,
                queued = receiver.recv() => match queued {
                    Some(mut frame) => { yield Ok::<Bytes, Infallible>(std::mem::take(&mut frame.bytes)); drop(frame); },
                    None => break,
                },
            }
        }
    };
    let response = Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store").header("x-accel-buffering", "no")
        .body(Body::from_stream(frames)).expect("static SSE response headers");
    (client, response)
}

impl Streams {
    pub fn new(rpc: RpcClient, config: WebAdapterConfig) -> Self {
        let (stop, _) = watch::channel(false);
        let (status_wake, _) = watch::channel(0); let (journal_wake, _) = watch::channel(0);
        Self { inner: Arc::new(Inner { rpc, config, stop, status_wake, journal_wake, status: Mutex::new(None), journal: Mutex::new(None) }) }
    }
    fn status_receiver(&self) -> Result<Latest<Value>, ApiError> {
        let mut slot = self.inner.status.lock().map_err(internal)?;
        if let Some(receiver) = &*slot { return Ok(receiver.clone()); }
        let (sender, receiver) = watch::channel(None); *slot = Some(receiver.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut stop = inner.stop.subscribe(); let mut signal = inner.status_wake.subscribe();
            loop {
                if *stop.borrow() { break; }
                let epoch = *signal.borrow_and_update();
                let result = tokio::select! { _ = stop.changed() => break, result = inner.rpc.data(DAEMON_GET_STATUS, json!({})) => result };
                let (result, delay) = match result {
                    Ok(data) => match data.pointer("/daemon/poll_interval_ms").and_then(Value::as_u64).filter(|v| *v > 0) {
                        Some(ms) => (Ok(data), Duration::from_millis(ms)),
                        None => (Err(ApiError::code("Unsupported", "daemon does not expose its effective poll interval")), Duration::from_millis(inner.config.web.sse_heartbeat_ms)),
                    },
                    Err(error) => (Err(error), Duration::from_millis(inner.config.web.sse_heartbeat_ms)),
                };
                sender.send_replace(Some(Publication { epoch, result }));
                tokio::select! { _ = stop.changed() => break, _ = signal.changed() => {}, _ = tokio::time::sleep(delay) => {} }
            }
        });
        Ok(receiver)
    }
    fn journal_receiver(&self) -> Result<Latest<Snapshot>, ApiError> {
        let mut slot = self.inner.journal.lock().map_err(internal)?;
        if let Some(receiver) = &*slot { return Ok(receiver.clone()); }
        let (sender, receiver) = watch::channel(None); *slot = Some(receiver.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut stop = inner.stop.subscribe(); let mut signal = inner.journal_wake.subscribe();
            let path = if inner.config.subtitles_path.is_absolute() { inner.config.subtitles_path.clone() }
                else { match std::env::current_dir() { Ok(dir) => dir.join(&inner.config.subtitles_path), Err(e) => { sender.send_replace(Some(Publication { epoch: *signal.borrow(), result: Err(source(e)) })); return; } } };
            let journal = Arc::new(Mutex::new(Journal::new(path.clone(), inner.config.web.sse_buffer_bytes)));
            let force = Arc::new(AtomicBool::new(false)); let rearm = Arc::new(AtomicBool::new(false));
            let mut watcher: Option<notify::RecommendedWatcher> = None; let mut watched: Option<PathBuf> = None;
            loop {
                if *stop.borrow() { break; }
                let epoch = *signal.borrow_and_update();
                let result = match nearest_parent(&path) {
                    Err(error) => Err(error),
                    Ok(parent) => {
                        if watcher.is_none() || watched.as_ref() != Some(&parent) || rearm.swap(false, Ordering::AcqRel) {
                            if watched.is_some() { force.store(true, Ordering::Release); }
                            watcher = None;
                            let target = path.clone(); let watched_parent = parent.clone();
                            let notification = inner.journal_wake.clone(); let reset = force.clone(); let rebuild = rearm.clone();
                            let created = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                                let relevant = match result {
                                    Ok(event) => {
                                        let close_write = matches!(event.kind, EventKind::Access(AccessKind::Close(AccessMode::Write)));
                                        if matches!(event.kind, EventKind::Access(_)) && !close_write { return; }
                                        let relevant = event.need_rescan() || event.paths.iter().any(|p| p == &target || target.starts_with(p));
                                        // A closed writer may have truncated and rebuilt an identical prefix.
                                        // The bridge keeps its append descriptor open; reads never trigger this reset.
                                        if relevant && close_write { reset.store(true, Ordering::Release); }
                                        if event.need_rescan() { reset.store(true, Ordering::Release); rebuild.store(true, Ordering::Release); }
                                        if event.paths.iter().any(|p| p == &watched_parent) { rebuild.store(true, Ordering::Release); }
                                        relevant
                                    }
                                    Err(error) => { eprintln!("subtitle watch invalidated: {error}"); reset.store(true, Ordering::Release); rebuild.store(true, Ordering::Release); true }
                                };
                                if relevant { wake(&notification); }
                            });
                            match created.and_then(|mut new| { new.watch(&parent, RecursiveMode::NonRecursive)?; Ok(new) }) {
                                Ok(new) => { watcher = Some(new); watched = Some(parent); }
                                Err(error) => eprintln!("subtitle watch: {error}"),
                            }
                        }
                        if watcher.is_none() { Err(source("cannot watch subtitle directory")) }
                        else {
                            let shared = journal.clone(); let reset = force.clone();
                            match tokio::task::spawn_blocking(move || refresh_with_reset(&shared, &reset)).await { Ok(result) => result, Err(error) => Err(internal(error)) }
                        }
                    }
                };
                let retry = result.as_ref().err().is_some_and(|error| error.needs_refresh);
                if !retry { sender.send_replace(Some(Publication { epoch, result })); }
                if retry {
                    // A bounded scan raced a writer. Retry the shared reader, not each subscriber.
                    tokio::select! { _ = stop.changed() => break, _ = tokio::task::yield_now() => {} }
                    continue;
                }
                tokio::select! { _ = stop.changed() => break, _ = signal.changed() => {}, _ = tokio::time::sleep(Duration::from_millis(inner.config.web.sse_heartbeat_ms)) => {} }
            }
            drop(watcher);
        });
        Ok(receiver)
    }
    pub async fn status(&self) -> Result<Response, ApiError> { self.status_for(None).await }
    pub async fn status_for(&self, transport: Option<Transport>) -> Result<Response, ApiError> {
        let mut receiver = self.status_receiver()?; let mut stop = self.inner.stop.subscribe();
        let initial = first(&mut receiver, &mut stop, wake(&self.inner.status_wake)).await?;
        let limit = self.inner.config.web.sse_buffer_bytes;
        let initial = fit(event("status", &initial.to_string(), None), limit)?;
        let (mut client, response) = channel(limit, transport, self.inner.stop.subscribe());
        let heartbeat = Duration::from_millis(self.inner.config.web.sse_heartbeat_ms);
        tokio::spawn(async move {
            if !client.send(initial) { return; }
            let mut timer = tokio::time::interval(heartbeat); timer.tick().await;
            loop {
                if *stop.borrow() || *client.stopped.borrow() { break; }
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = client.stopped.changed() => break,
                    changed = receiver.changed() => {
                        if changed.is_err() { break; }
                        let current = receiver.borrow_and_update().clone();
                        if let Some(publication) = current { match publication.result {
                            Ok(data) => { if !client.send(event("status", &data.to_string(), None)) { break; } },
                            Err(error) => { client.fail(&error); break; },
                        } }
                    }
                    _ = timer.tick() => { if !client.send(Bytes::from_static(b": heartbeat\n\n")) { break; } },
                }
            }
        });
        Ok(response)
    }
    pub async fn subtitles(&self, cursor: Option<String>) -> Result<Response, ApiError> { self.subtitles_for(cursor, None).await }
    pub async fn subtitles_for(&self, cursor: Option<String>, transport: Option<Transport>) -> Result<Response, ApiError> {
        let mut receiver = self.journal_receiver()?; let mut stop = self.inner.stop.subscribe();
        let mut initial = first(&mut receiver, &mut stop, wake(&self.inner.journal_wake)).await?;
        let (offset, needs_reset) = loop {
            let copy = initial.clone(); let cursor = cursor.clone();
            match tokio::task::spawn_blocking(move || copy.cursor(cursor.as_deref())).await.map_err(internal)? {
                Ok(position) => break position,
                Err(error) if error.needs_refresh => { initial = first(&mut receiver, &mut stop, wake(&self.inner.journal_wake)).await?; },
                Err(error) => return Err(error),
            }
        };
        let limit = self.inner.config.web.sse_buffer_bytes;
        let initial_reset = if needs_reset { Some(fit(reset(&initial.generation), limit)?) } else { None };
        let (client, response) = channel(limit, transport, self.inner.stop.subscribe());
        let heartbeat = Duration::from_millis(self.inner.config.web.sse_heartbeat_ms);
        let signal = self.inner.journal_wake.clone();
        tokio::spawn(async move {
            let mut client = client;
            let mut snapshot = initial; let mut offset = offset; let mut generation = snapshot.generation.clone();
            let mut pending_reset = initial_reset;
            let mut timer = tokio::time::interval(heartbeat); timer.tick().await;
            'connection: loop {
                if *stop.borrow() || *client.stopped.borrow() { break; }
                if let Some(frame) = pending_reset.take() { if !client.send(frame) { break; } }
                while offset < snapshot.len {
                    if *stop.borrow() || *client.stopped.borrow() { break 'connection; }
                    if receiver.has_changed().unwrap_or(true) { break; }
                    let copy = snapshot.clone();
                    let result = tokio::task::spawn_blocking(move || copy.read_line(offset)).await;
                    match result {
                        Ok(Ok(Some((line, next)))) => {
                            if !client.send(event("subtitle", &line, Some(&format!("{generation}:{next}")))) { break 'connection; }
                            offset = next;
                        }
                        Ok(Ok(None)) => break,
                        Ok(Err(error)) if error.needs_refresh => { wake(&signal); break; },
                        Ok(Err(error)) => { client.fail(&error); break 'connection; },
                        Err(error) => { client.fail(&internal(error)); break 'connection; },
                    }
                }
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = client.stopped.changed() => break,
                    changed = receiver.changed() => {
                        if changed.is_err() { break; }
                        let current = receiver.borrow_and_update().clone();
                        if let Some(publication) = current { match publication.result {
                            Ok(next) => {
                                if next.generation != generation { generation = next.generation.clone(); offset = 0; pending_reset = Some(reset(&generation)); }
                                snapshot = next;
                            }
                            Err(error) => { client.fail(&error); break; },
                        } }
                    }
                    _ = timer.tick() => { if !client.send(Bytes::from_static(b": heartbeat\n\n")) { break; } },
                }
            }
        });
        Ok(response)
    }
    pub fn shutdown(&self) { self.inner.stop.send_replace(true); }
}
fn refresh_with_reset(journal: &Mutex<Journal>, force: &AtomicBool) -> Result<Snapshot, ApiError> {
    let reset = force.swap(false, Ordering::AcqRel);
    let result = journal.lock().map_err(internal).and_then(|mut journal| journal.refresh(reset));
    if reset && result.is_err() { force.store(true, Ordering::Release); }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn failed_refresh_preserves_forced_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        std::fs::create_dir(&path).unwrap();
        let journal = Mutex::new(Journal::new(path.clone(), 4096));
        let force = AtomicBool::new(true);
        assert!(refresh_with_reset(&journal, &force).is_err());
        assert!(force.load(Ordering::Acquire));
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(path, b"{}\n").unwrap();
        refresh_with_reset(&journal, &force).unwrap();
        assert!(!force.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn budget_includes_queued_and_inflight_frames() {
        let (_stop, receiver) = watch::channel(false);
        let (client, response) = channel(32, None, receiver);
        let frame = Bytes::from_static(b"12345678901234567890");
        assert!(client.send(frame.clone()));
        let mut body = response.into_body();
        assert_eq!(body.frame().await.unwrap().unwrap().into_data().unwrap().len(), 20);
        assert_eq!(client.pending.load(Ordering::Acquire), 20);
        assert!(!client.send(frame));
        assert!(*client.stopped.borrow());
        assert!(body.frame().await.is_none());
        drop(body);
        assert_eq!(client.pending.load(Ordering::Acquire), 0);
    }
}

fn nearest_parent(path: &Path) -> Result<PathBuf, ApiError> {
    let mut parent = path.parent().ok_or_else(|| source("subtitle path has no parent"))?.to_path_buf();
    while !parent.is_dir() { if !parent.pop() { return Err(source("subtitle parent directory is unavailable")); } }
    Ok(parent)
}
