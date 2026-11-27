use std::{path::{PathBuf, Component}, sync::Arc};
use axum::{body::{Body, to_bytes}, extract::{DefaultBodyLimit, Request, State as AxumState}, http::{header, HeaderMap, HeaderValue, Method}, response::{IntoResponse, Response}, routing::any, Router};
use mozart_core::{config::WebAdapterConfig, ctl::{self, schema}};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio_util::io::ReaderStream;
use futures_util::TryStreamExt;
use crate::{error::{ApiError, control, ok}, rpc::RpcClient, streams::Streams, uploads::UploadStore};

#[derive(Clone)]
pub struct State {
    pub config: WebAdapterConfig,
    pub rpc: RpcClient,
    pub streams: Streams,
    pub uploads: Arc<UploadStore>,
    pub origins: Vec<url::Url>,
    pub logs_dir: Option<PathBuf>,
}

fn bad(reason: impl Into<String>) -> ApiError { ApiError::code("BadRequest", reason) }
fn forbidden() -> ApiError { ApiError::code("Forbidden", "origin or host is not allowed") }
fn parse_origin(s: &str) -> Option<url::Url> {
    let url = url::Url::parse(s).ok()?;
    if url.scheme() != "http" || url.host().is_none() || !url.username().is_empty() || url.password().is_some() || url.path() != "/" || url.query().is_some() || url.fragment().is_some() || s.contains('@') { return None; }
    // URL parsers normalize paths, so reject even syntactically present paths.
    let rest = s.strip_prefix("http://")?;
    if rest.contains('/') || rest.contains('?') || rest.contains('#') { return None; }
    Some(url)
}
pub fn origins(values: &[String]) -> Result<Vec<url::Url>, ApiError> {
    values.iter().map(|s| parse_origin(s).ok_or_else(|| bad("invalid allowed origin"))).collect()
}
fn guard(headers: &HeaderMap, origins: &[url::Url]) -> Result<(), ApiError> {
    let single = |name: header::HeaderName| -> Result<Option<&str>, ApiError> {
        let mut it = headers.get_all(name).iter();
        let v = it.next().map(|x| x.to_str().map_err(|_| forbidden())).transpose()?;
        if it.next().is_some() { return Err(forbidden()); }
        Ok(v)
    };
    let host = single(header::HOST)?.ok_or_else(forbidden)?;
    let request = parse_origin(&format!("http://{host}")).ok_or_else(forbidden)?;
    if !origins.iter().any(|x| x.host() == request.host() && x.port_or_known_default() == request.port_or_known_default()) { return Err(forbidden()); }
    if let Some(origin) = single(header::ORIGIN)? {
        let origin = parse_origin(origin).ok_or_else(forbidden)?;
        if origin != request || !origins.contains(&origin) { return Err(forbidden()); }
    }
    if single(header::HeaderName::from_static("sec-fetch-site"))?.is_some_and(|x| x.eq_ignore_ascii_case("cross-site")) { return Err(forbidden()); }
    Ok(())
}
pub fn router(state: State) -> Router { Router::new().fallback(any(dispatch)).layer(DefaultBodyLimit::disable()).with_state(state) }

async fn dispatch(AxumState(s): AxumState<State>, req: Request) -> Response {
    let api = req.uri().path() == "/api" || req.uri().path().starts_with("/api/");
    let mut response = match guard(req.headers(), &s.origins) {
        Err(e) => e.into_response(),
        Ok(()) => match serve(&s, req, api).await { Ok(r) => r, Err(e) => e.into_response() },
    };
    if api { response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store")); }
    response
}
fn decode(s: &str) -> Result<String, ApiError> {
    let b = s.as_bytes(); let mut i = 0;
    while i < b.len() { if b[i] == b'%' { if i + 2 >= b.len() || !b[i+1].is_ascii_hexdigit() || !b[i+2].is_ascii_hexdigit() { return Err(bad("invalid path encoding")); } i += 3; } else { i += 1; } }
    let v = percent_encoding::percent_decode_str(s).decode_utf8().map_err(|_| bad("invalid path encoding"))?.into_owned();
    if v.is_empty() || v.contains('/') || v.contains('\\') || v.chars().any(char::is_control) { return Err(bad("invalid path segment")); }
    Ok(v)
}
fn query(q: Option<&str>, allowed: &[&str]) -> Result<std::collections::HashMap<String,String>, ApiError> {
    let mut out = std::collections::HashMap::new();
    if let Some(q) = q {
        let bytes = q.as_bytes();
        let mut i = 0;
        while i < bytes.len() { if bytes[i] == b'%' { if i + 2 >= bytes.len() || !bytes[i+1].is_ascii_hexdigit() || !bytes[i+2].is_ascii_hexdigit() { return Err(bad("invalid query encoding")); } i += 3; } else { i += 1; } }
        percent_encoding::percent_decode_str(q).decode_utf8().map_err(|_| bad("invalid query encoding"))?;
        for (k,v) in url::form_urlencoded::parse(q.as_bytes()) {
        if !allowed.contains(&k.as_ref()) || out.insert(k.into_owned(), v.into_owned()).is_some() { return Err(bad("unknown or duplicate query parameter")); }
    }}
    Ok(out)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
async fn body<T: DeserializeOwned>(req: Request) -> Result<Value, ApiError> {
    let ct = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json") { return Err(ApiError::code("UnsupportedMediaType", "expected application/json")); }
    let bytes = to_bytes(req.into_body(), ctl::MAX_BODY_BYTES).await.map_err(|_| ApiError::code("PayloadTooLarge", "JSON body exceeds limit"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| bad(format!("invalid JSON body: {e}")))?;
    if !value.is_object() { return Err(bad("JSON body must be an object")); }
    let _: T = serde_json::from_value(value.clone()).map_err(|e| bad(format!("invalid JSON body: {e}")))?;
    Ok(value)
}
fn method(allowed: &str) -> Response {
    let mut r = ApiError::code("MethodNotAllowed", "method not allowed").into_response();
    r.headers_mut().insert(header::ALLOW, HeaderValue::from_str(allowed).unwrap()); r
}

async fn serve(s: &State, req: Request, api: bool) -> Result<Response, ApiError> {
    let transport = req.extensions().get::<axum::extract::ConnectInfo<crate::transport::Transport>>().map(|info| info.0.clone());
    if !api { return static_file(s, req).await; }
    let path = req.uri().path().to_string();
    let parts: Vec<String> = path.split('/').skip(1).map(decode).collect::<Result<_,_>>()?;
    if parts.len() < 3 || parts[0] != "api" || parts[1] != "v1" { return Err(ApiError::code("NotFound", "API route not found")); }
    let p: Vec<&str> = parts[2..].iter().map(String::as_str).collect();
    let m = req.method().clone();
    let allow = match p.as_slice() {
        ["preset", "save" | "use"] | ["system", "up" | "down"] | ["speech", "stop" | "clear"] | ["config", "save"] | ["services", _, "restart"] | ["files", "pause" | "resume"] | ["files", _, "cancel"] => "POST",
        ["health" | "status" | "models" | "devices" | "preset"] | ["events", "status" | "subtitles"] | ["files", _, "result"] | ["files", _] => "GET",
        ["logs"] => "GET",
        ["route" | "mute" | "translate" | "speech" | "model" | "mode"] | ["devices", "capture-mixer"] => "PUT",
        ["config"] => "GET, PUT",
        ["files"] => "GET, POST",
        _ => return Err(ApiError::code("NotFound", "API route not found")),
    };
    if !allow.split(", ").any(|x| x == m.as_str()) { return Ok(method(allow)); }
    let keys: &[&str] = match p.as_slice() { ["devices"] => &["all"], ["config"] if m == Method::GET => &["key"], ["events", "subtitles"] => &["cursor"], ["logs"] => &["name"], _ => &[] };
    let q = query(req.uri().query(), keys)?;
    match (m, p.as_slice()) {
        (Method::GET, ["health"]) => Ok(ok(json!({"adapter":"running", "uploads":s.uploads.health(), "transport":crate::transport::health()}))),
        (Method::GET, ["events", "status"]) => s.streams.status_for(transport).await,
        (Method::GET, ["events", "subtitles"]) => {
            let id = req.headers().get("last-event-id").map(|h| h.to_str().map(str::to_owned).map_err(|_| bad("invalid Last-Event-ID"))).transpose()?.or_else(|| q.get("cursor").cloned());
            s.streams.subtitles_for(id, transport).await
        }
        (Method::POST, ["files"]) => s.uploads.clone().handle(req).await,
        (Method::GET, ["files", id, "result"]) => download(s, id).await,
        (Method::GET, ["logs"]) => read_log(s, q.get("name").map(String::as_str)).await,
        (method, path) => {
            let (cmd, data) = match (method, path) {
                (Method::POST, ["system", "up"]) => { body::<Empty>(req).await?; (ctl::DAEMON_SERVICES_START, json!({})) },
                (Method::POST, ["system", "down"]) => { body::<Empty>(req).await?; (ctl::DAEMON_SERVICES_STOP, json!({})) },
                (Method::GET, ["status"]) => (ctl::DAEMON_GET_STATUS, json!({})),
                (Method::GET, ["models"]) => (ctl::DAEMON_MODEL_LIST, json!({})),
                (Method::GET, ["preset"]) => (ctl::DAEMON_PRESET_LIST, json!({})),
                (Method::PUT, ["model"]) => (ctl::DAEMON_MODEL_USE, body::<schema::ModelUseRequest>(req).await?),
                (Method::PUT, ["mode"]) => (ctl::DAEMON_OUTPUT_MODE_SET, body::<schema::OutputModeSetRequest>(req).await?),
                (Method::POST, ["preset", "save"]) => { body::<Empty>(req).await?; (ctl::DAEMON_PRESET_SAVE, json!({})) },
                (Method::POST, ["preset", "use"]) => { body::<Empty>(req).await?; (ctl::DAEMON_PRESET_USE, json!({})) },
                (Method::GET, ["devices"]) => { let all = match q.get("all").map(String::as_str) { None | Some("false") => false, Some("true") => true, _ => return Err(bad("invalid all parameter")) }; (ctl::DAEMON_DEVICE_LIST, json!({"all":all})) },
                (Method::GET, ["config"]) => (ctl::DAEMON_CONFIG_GET, match q.get("key") { Some(k) => json!({"key":k}), None => json!({}) }),
                (Method::GET, ["files"]) => (ctl::DAEMON_FILE_LIST, json!({})),
                (Method::GET, ["files", id]) => (ctl::DAEMON_FILE_STATUS, json!({"job_id":id})),
                (Method::PUT, ["route"]) => (ctl::DAEMON_ROUTE_SET, body::<schema::RouteSetRequest>(req).await?),
                (Method::PUT, ["mute"]) => (ctl::DAEMON_MUTE_SET, body::<schema::MuteSetRequest>(req).await?),
                (Method::PUT, ["devices", "capture-mixer"]) => (ctl::DAEMON_MIC_MIXER_SET, body::<schema::MicMixerSetRequest>(req).await?),
                (Method::PUT, ["translate"]) => (ctl::DAEMON_TRANSLATE_SET, body::<schema::SwitchSetRequest>(req).await?),
                (Method::PUT, ["speech"]) => (ctl::DAEMON_SPEECH_SET, body::<schema::SwitchSetRequest>(req).await?),
                (Method::PUT, ["config"]) => (ctl::DAEMON_CONFIG_SET, body::<schema::ConfigSetRequest>(req).await?),
                (Method::POST, ["services", service, "restart"]) => { body::<Empty>(req).await?; (ctl::DAEMON_RESTART, json!({"service":service})) },
                (Method::POST, ["files", id, "cancel"]) => { body::<Empty>(req).await?; (ctl::DAEMON_FILE_CANCEL, json!({"job_id":id})) },
                (Method::POST, ["speech", "stop"]) => { body::<Empty>(req).await?; (ctl::DAEMON_SPEECH_STOP, json!({})) },
                (Method::POST, ["speech", "clear"]) => { body::<Empty>(req).await?; (ctl::DAEMON_SPEECH_CLEAR, json!({})) },
                (Method::POST, ["config", "save"]) => { body::<Empty>(req).await?; (ctl::DAEMON_CONFIG_SAVE, json!({})) },
                (Method::POST, ["files", "pause"]) => { body::<Empty>(req).await?; (ctl::DAEMON_FILE_PAUSE, json!({})) },
                (Method::POST, ["files", "resume"]) => { body::<Empty>(req).await?; (ctl::DAEMON_FILE_RESUME, json!({})) },
                _ => unreachable!(),
            };
            Ok(control(s.rpc.call(cmd, data).await?))
        }
    }
}

async fn download(s: &State, id: &str) -> Result<Response, ApiError> {
    let data = s.rpc.fetch_result(id.to_owned()).await?;
    let path = data.get("output_path").and_then(Value::as_str).ok_or_else(|| ApiError::code("BadGateway", "missing result path"))?;
    let name = data.get("name").and_then(Value::as_str).ok_or_else(|| ApiError::code("BadGateway", "missing result name"))?;
    use std::os::unix::fs::OpenOptionsExt;
    let path = PathBuf::from(path);
    let (file, len) = tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() { return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)); }
        Ok::<_, std::io::Error>((file, metadata.len()))
    }).await.map_err(|_| ApiError::code("ResultUnavailable", "result cannot be opened"))?
        .map_err(|_| ApiError::code("ResultUnavailable", "result cannot be opened"))?;
    let file = tokio::fs::File::from_std(file);
    let encoded = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC).to_string();
    let stream = ReaderStream::new(file).inspect_err(|e| eprintln!("result read error: {e}"));
    Response::builder().header(header::CONTENT_TYPE, "audio/wav").header(header::CONTENT_LENGTH, len).header(header::CONTENT_DISPOSITION, format!("attachment; filename*=UTF-8''{encoded}")).body(Body::from_stream(stream)).map_err(|e| ApiError::code("Internal", e.to_string()))
}

const LOG_SERVICES: [&str; 8] = ["asr", "bridge", "daemon", "llama", "pre", "rvc", "tts", "web"];
const LOG_TAIL_BYTES: u64 = 16 * 1024;

// 只读日志尾随：白名单服务名，返回日志末尾片段；从中间开始时丢掉首行残缺内容。
async fn read_log(s: &State, name: Option<&str>) -> Result<Response, ApiError> {
    let name = name.ok_or_else(|| bad("missing log name"))?;
    if !LOG_SERVICES.contains(&name) { return Err(bad("unknown log service")); }
    let dir = s.logs_dir.as_ref().ok_or_else(|| ApiError::code("Unavailable", "log directory is not configured"))?;
    let root = tokio::fs::canonicalize(dir).await.map_err(|_| ApiError::code("NotFound", "log directory unavailable"))?;
    let path = tokio::fs::canonicalize(root.join(format!("{name}.log"))).await.map_err(|_| ApiError::code("NotFound", "log file not found"))?;
    if !path.starts_with(&root) { return Err(ApiError::code("NotFound", "log file not found")); }
    let meta = tokio::fs::metadata(&path).await.map_err(|_| ApiError::code("NotFound", "log file not found"))?;
    if !meta.is_file() { return Err(ApiError::code("NotFound", "log file not found")); }
    let start = meta.len().saturating_sub(LOG_TAIL_BYTES);
    let mut file = tokio::fs::File::open(&path).await.map_err(|_| ApiError::code("NotFound", "log file not found"))?;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    file.seek(std::io::SeekFrom::Start(start)).await.map_err(|_| ApiError::code("NotFound", "log file not found"))?;
    let mut buf = Vec::with_capacity(LOG_TAIL_BYTES as usize);
    file.take(LOG_TAIL_BYTES).read_to_end(&mut buf).await.map_err(|_| ApiError::code("NotFound", "log file not found"))?;
    let text = String::from_utf8_lossy(&buf);
    let tail = if start == 0 { text.as_ref() } else { text.split_once('\n').map_or("", |(_, rest)| rest) };
    Ok(ok(json!({ "name": name, "tail": tail })))
}

async fn static_file(s: &State, req: Request) -> Result<Response, ApiError> {
    if req.method() != Method::GET { return Ok(method("GET")); }
    let mut path = PathBuf::new();
    for part in req.uri().path().split('/').filter(|x| !x.is_empty()) { let p = decode(part)?; if p == "." || p == ".." { return Err(forbidden()); } path.push(p); }
    if path.components().any(|x| !matches!(x, Component::Normal(_))) { return Err(forbidden()); }
    let root = tokio::fs::canonicalize(&s.config.web.static_dir).await.map_err(|_| ApiError::code("NotFound", "static directory unavailable"))?;
    let navigation = req.headers().get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|x| x.trim().split(';').next() == Some("text/html")));
    if path.as_os_str().is_empty() { path.push("index.html"); }
    let root_fd = std::fs::File::open(&root).map_err(|_| ApiError::code("NotFound", "static directory unavailable"))?;
    let opened = match open_beneath(&root_fd, &path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && navigation => {
            path = "index.html".into();
            open_beneath(&root_fd, &path).map_err(|_| ApiError::code("NotFound", "static file not found"))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ApiError::code("NotFound", "static file not found")),
        Err(_) => return Err(forbidden()),
    };
    let file = tokio::fs::File::from_std(opened);
    let mime = match path.extension().and_then(|x| x.to_str()).unwrap_or("") { "html" => "text/html; charset=utf-8", "js" => "text/javascript", "css" => "text/css", "svg" => "image/svg+xml", "json" => "application/json", "png" => "image/png", "ico" => "image/x-icon", _ => "application/octet-stream" };
    Ok(([(header::CONTENT_TYPE, mime)], Body::from_stream(ReaderStream::new(file))).into_response())
}

#[cfg(target_os = "linux")]
fn open_beneath(root: &std::fs::File, path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::{fd::{AsRawFd, FromRawFd}, unix::ffi::OsStrExt};
    #[repr(C)]
    struct OpenHow { flags: u64, mode: u64, resolve: u64 }
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS bind resolution to the opened root.
    let how = OpenHow { flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64, mode: 0, resolve: 0x08 | 0x02 };
    let fd = unsafe { libc::syscall(libc::SYS_openat2, root.as_raw_fd(), name.as_ptr(), &how, std::mem::size_of::<OpenHow>()) };
    if fd < 0 { return Err(std::io::Error::last_os_error()); }
    let file = unsafe { std::fs::File::from_raw_fd(fd as i32) };
    if !file.metadata()?.is_file() { return Err(std::io::ErrorKind::PermissionDenied.into()); }
    Ok(file)
}
#[cfg(not(target_os = "linux"))]
fn open_beneath(_: &std::fs::File, _: &std::path::Path) -> std::io::Result<std::fs::File> {
    Err(std::io::ErrorKind::Unsupported.into())
}
