use axum::{body::{Body, to_bytes}, http::{Request, StatusCode, header}};
use mozart_core::config::WebAdapterConfig;
use tower::ServiceExt;
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::time::Duration;

async fn setup(dir: &tempfile::TempDir, ctl: std::path::PathBuf) -> mozart_web_adapter::Application {
    std::fs::write(dir.path().join("config.toml"), format!("[daemon]\nrun_dir = {:?}\nsocket = \"test.sock\"\n", dir.path().to_str().unwrap())).unwrap();
    let mut cfg = WebAdapterConfig::load_from(Some(&dir.path().join("config.toml"))).unwrap();
    cfg.web.static_dir = dir.path().join("static");
    cfg.web.upload_dir = dir.path().join("uploads");
    cfg.web.allowed_origins = vec!["http://localhost:8080".into()];
    std::fs::create_dir_all(&cfg.web.static_dir).unwrap();
    std::fs::write(cfg.web.static_dir.join("index.html"), "page").unwrap();
    mozart_web_adapter::build(cfg, Some(dir.path().join("config.toml")), ctl).await.unwrap()
}
fn request(method: &str, uri: &str, host: &str, body: &'static str) -> Request<Body> {
    Request::builder().method(method).uri(uri).header(header::HOST, host).body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn boundary_routes_and_static() {
    let dir = tempfile::tempdir().unwrap();
    let app = setup(&dir, "/nonexistent".into()).await;
    let router = app.router;
    let denied = router.clone().oneshot(request("GET", "/api/v1/health", "attacker", "")).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let cross = Request::builder().uri("/api/v1/health").header(header::HOST, "localhost:8080").header(header::ORIGIN, "http://evil:8080").body(Body::empty()).unwrap();
    assert_eq!(router.clone().oneshot(cross).await.unwrap().status(), StatusCode::FORBIDDEN);
    let unknown = router.clone().oneshot(request("GET", "/api/v1/no-such-route", "localhost:8080", "")).await.unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(unknown.headers()[header::CACHE_CONTROL], "no-store");
    let method = router.clone().oneshot(request("POST", "/api/v1/status", "localhost:8080", "")).await.unwrap();
    assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(method.headers()[header::ALLOW], "GET");
    let bad_type = router.clone().oneshot(request("PUT", "/api/v1/mute", "localhost:8080", "{}")).await.unwrap();
    assert_eq!(bad_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let invalid = Request::builder().method("PUT").uri("/api/v1/mute").header(header::HOST, "localhost:8080").header(header::CONTENT_TYPE, "application/json").body(Body::from(r#"{"mute":true,"extra":1}"#)).unwrap();
    assert_eq!(router.clone().oneshot(invalid).await.unwrap().status(), StatusCode::BAD_REQUEST);
    let oversized = Request::builder().method("PUT").uri("/api/v1/mute").header(header::HOST, "localhost:8080").header(header::CONTENT_TYPE, "application/json").body(Body::from(vec![b' '; mozart_core::ctl::MAX_BODY_BYTES + 1])).unwrap();
    assert_eq!(router.clone().oneshot(oversized).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);
    let dup = router.clone().oneshot(request("GET", "/api/v1/devices?all=true&all=false", "localhost:8080", "")).await.unwrap();
    assert_eq!(dup.status(), StatusCode::BAD_REQUEST);
    let nav = Request::builder().uri("/some-page").header(header::HOST, "localhost:8080").header(header::ACCEPT, "text/html").body(Body::empty()).unwrap();
    let page = router.clone().oneshot(nav).await.unwrap();
    assert_eq!(&to_bytes(page.into_body(), 1024).await.unwrap()[..], b"page");
    let traversal = router.clone().oneshot(request("GET", "/%2e%2e/secret", "localhost:8080", "")).await.unwrap();
    assert_eq!(traversal.status(), StatusCode::FORBIDDEN);
    #[cfg(unix)]
    {
        std::fs::write(dir.path().join("secret"), "private").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), dir.path().join("static/leak")).unwrap();
        let escaped = router.oneshot(request("GET", "/leak", "localhost:8080", "")).await.unwrap();
        assert_eq!(escaped.status(), StatusCode::FORBIDDEN);
    }
}

// Each inner list is one connection, preserving the session boundary.
fn mock(dir: &tempfile::TempDir, sessions: Vec<Vec<(u16, Value)>>) -> std::thread::JoinHandle<()> {
    let listener = Seqpacket::bind(&dir.path().join("test.sock")).unwrap();
    listener.listen(8).unwrap();
    std::thread::spawn(move || {
        for session in sessions {
            let conn = listener.accept().unwrap();
            for (command, data) in session {
                let msg = conn.recv(Some(Duration::from_secs(5))).unwrap();
                let type_ = msg.header.type_;
                assert_eq!(type_, command);
                let request: Value = serde_json::from_slice(&msg.body).unwrap();
                if command == ctl::DAEMON_FILE_FETCH { assert_eq!(request, json!({"job_id":"job"})); }
                if command == ctl::DAEMON_FILE_SUBMIT {
                    assert_eq!(request["model_id"], "voice");
                    assert!(std::path::Path::new(request["path"].as_str().unwrap()).is_file());
                }
                let reply = serde_json::to_vec(&json!({"status":"ok", "code":"Ok", "reason":"", "data":data})).unwrap();
                conn.send(command, msg.header.request_id, &reply).unwrap();
            }
            assert!(conn.recv(Some(Duration::from_secs(5))).is_err(), "unexpected extra request on session");
        }
    })
}

fn web_request(method: &str, uri: &str, content_type: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder().method(method).uri(uri).header(header::HOST, "localhost:8080")
        .header(header::CONTENT_TYPE, content_type).body(Body::from(body)).unwrap()
}

fn multipart(size: usize, duplicate: bool) -> Vec<u8> {
    let mut body = b"--test-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\n".to_vec();
    body.resize(body.len() + size, b'a');
    body.extend_from_slice(b"\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"model_id\"\r\n\r\nvoice\r\n");
    if duplicate { body.extend_from_slice(b"--test-boundary\r\nContent-Disposition: form-data; name=\"model_id\"\r\n\r\nother\r\n"); }
    body.extend_from_slice(b"--test-boundary--\r\n");
    body
}

#[tokio::test]
async fn multipart_uses_daemon_limit_not_axum_limit() {
    let dir = tempfile::tempdir().unwrap();
    let app = setup(&dir, "/nonexistent".into()).await;
    let size = 2 * 1024 * 1024 + 1024;
    let status = |limit| (ctl::DAEMON_GET_STATUS, json!({"file_limits":{"max_upload_bytes":limit}}));
    let server = mock(&dir, vec![vec![status(size - 1)], vec![status(size)],
        vec![(ctl::DAEMON_FILE_SUBMIT, json!({"job_id":"job","state":"queued","queue_position":1}))], vec![status(size)]]);
    let send = |data| web_request("POST", "/api/v1/files", "multipart/form-data; boundary=test-boundary", data);
    let rejected = app.router.clone().oneshot(send(multipart(size, false))).await.unwrap();
    assert_eq!(rejected.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let accepted = app.router.clone().oneshot(send(multipart(size, false))).await.unwrap();
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    assert_eq!(accepted.headers()[header::LOCATION], "/api/v1/files/job");
    let duplicate = app.router.clone().oneshot(send(multipart(1, true))).await.unwrap();
    assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
    server.join().unwrap();
}

#[tokio::test]
async fn download_requires_capability_and_streams_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let app = setup(&dir, "/nonexistent".into()).await;
    let path = dir.path().join("result.wav");
    std::fs::write(&path, b"RIFFtestWAVE").unwrap();
    let cap = || (ctl::DAEMON_GET_STATUS, json!({"api_capabilities":["file_fetch_safe_errors_v1"]}));
    let fetch = |p: &std::path::Path| (ctl::DAEMON_FILE_FETCH, json!({"output_path":p,"name":"声音.wav"}));
    let missing = dir.path().join("missing.wav");
    let fifo = dir.path().join("fifo");
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let server = mock(&dir, vec![vec![(ctl::DAEMON_GET_STATUS, json!({}))], vec![cap(), fetch(&path)], vec![cap(), fetch(&missing)], vec![cap(), fetch(&fifo)]]);
    let get = || request("GET", "/api/v1/files/job/result", "localhost:8080", "");
    assert_eq!(app.router.clone().oneshot(get()).await.unwrap().status(), StatusCode::NOT_IMPLEMENTED);
    let response = app.router.clone().oneshot(get()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/wav");
    assert_eq!(response.headers()[header::CONTENT_DISPOSITION], "attachment; filename*=UTF-8''%E5%A3%B0%E9%9F%B3%2Ewav");
    assert_eq!(&to_bytes(response.into_body(), 1024).await.unwrap()[..], b"RIFFtestWAVE");
    for _ in 0..2 {
        let response = tokio::time::timeout(Duration::from_secs(2), app.router.clone().oneshot(get())).await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains(dir.path().to_str().unwrap()));
    }
    for query in ["output=x", "path=x"] {
        let response = app.router.clone().oneshot(request("GET", &format!("/api/v1/files/job/result?{query}"), "localhost:8080", "")).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    server.join().unwrap();
}

#[tokio::test]
async fn model_preset_and_output_mode_routes_relay_and_validate() {
    let dir = tempfile::tempdir().unwrap();
    let app = setup(&dir, "/nonexistent".into()).await;
    let listener = Seqpacket::bind(&dir.path().join("test.sock")).unwrap();
    listener.listen(8).unwrap();
    let server = std::thread::spawn(move || {
        let cases = [
            (ctl::DAEMON_MODEL_USE, json!({"model_id":"voice-a"}), json!({"status":"ok","code":"Ok","reason":"","data":{"model_id":"voice-a","previous_model_id":"old"}})),
            (ctl::DAEMON_OUTPUT_MODE_SET, json!({"mode":"broadcast"}), json!({"status":"ok","code":"Ok","reason":"","data":{"requested":"broadcast","applied":null,"phase":"closing_speech","last_error":null}})),
            (ctl::DAEMON_PRESET_LIST, json!({}), json!({"status":"ok","code":"Ok","reason":"","data":{"model_id":"voice-a","preset":"default","dirty":true}})),
            (ctl::DAEMON_PRESET_SAVE, json!({}), json!({"status":"ok","code":"Ok","reason":"","data":{"saved":42}})),
            (ctl::DAEMON_PRESET_USE, json!({}), json!({"status":"error","code":"Busy","reason":"provider busy","data":null})),
        ];
        for (command, expected, reply) in cases {
            let conn = listener.accept().unwrap();
            let msg = conn.recv(Some(Duration::from_secs(2))).unwrap();
            let type_ = msg.header.type_;
            assert_eq!(type_, command);
            assert_eq!(serde_json::from_slice::<Value>(&msg.body).unwrap(), expected);
            conn.send(command, msg.header.request_id, &serde_json::to_vec(&reply).unwrap()).unwrap();
        }
    });
    let cases = [
        ("PUT", "/api/v1/model", r#"{"model_id":"voice-a"}"#, StatusCode::OK, json!({"status":"ok","code":"Ok","reason":"","data":{"model_id":"voice-a","previous_model_id":"old"}})),
        ("PUT", "/api/v1/mode", r#"{"mode":"broadcast"}"#, StatusCode::OK, json!({"status":"ok","code":"Ok","reason":"","data":{"requested":"broadcast","applied":null,"phase":"closing_speech","last_error":null}})),
        ("GET", "/api/v1/preset", "", StatusCode::OK, json!({"status":"ok","code":"Ok","reason":"","data":{"model_id":"voice-a","preset":"default","dirty":true}})),
        ("POST", "/api/v1/preset/save", "{}", StatusCode::OK, json!({"status":"ok","code":"Ok","reason":"","data":{"saved":42}})),
        ("POST", "/api/v1/preset/use", "{}", StatusCode::CONFLICT, json!({"status":"error","code":"Busy","reason":"provider busy","data":null})),
    ];
    for (method, path, body, status, expected) in cases {
        let response = app.router.clone().oneshot(web_request(method, path, "application/json", body.as_bytes().to_vec())).await.unwrap();
        assert_eq!(response.status(), status, "{path}");
        let actual: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(actual, expected);
    }
    server.join().unwrap();
    for (method, path, body, status) in [
        ("POST", "/api/v1/model", "{}", StatusCode::METHOD_NOT_ALLOWED),
        ("GET", "/api/v1/mode", "", StatusCode::METHOD_NOT_ALLOWED),
        ("PUT", "/api/v1/mode", r#"{"mode":"file"}"#, StatusCode::BAD_REQUEST),
        ("PUT", "/api/v1/preset", "{}", StatusCode::METHOD_NOT_ALLOWED),
        ("GET", "/api/v1/preset/save", "", StatusCode::METHOD_NOT_ALLOWED),
        ("PUT", "/api/v1/preset/use", "{}", StatusCode::METHOD_NOT_ALLOWED),
        ("PUT", "/api/v1/model", "[]", StatusCode::BAD_REQUEST),
        ("PUT", "/api/v1/model", r#"{"model_id":1}"#, StatusCode::BAD_REQUEST),
        ("PUT", "/api/v1/model", r#"{"model_id":"a","extra":1}"#, StatusCode::BAD_REQUEST),
        ("POST", "/api/v1/preset/save", "[]", StatusCode::BAD_REQUEST),
        ("POST", "/api/v1/preset/use", r#"{"extra":1}"#, StatusCode::BAD_REQUEST),
        ("GET", "/api/v1/preset?extra=1", "", StatusCode::BAD_REQUEST),
    ] {
        let response = app.router.clone().oneshot(web_request(method, path, "application/json", body.as_bytes().to_vec())).await.unwrap();
        assert_eq!(response.status(), status, "{method} {path} {body}");
    }
}

#[tokio::test]
async fn model_and_preset_writes_are_not_retried_after_disconnect() {
    use std::os::fd::AsRawFd;
    for (method, path, body, command, expected) in [
        ("PUT", "/api/v1/model", r#"{"model_id":"voice"}"#, ctl::DAEMON_MODEL_USE, json!({"model_id":"voice"})),
        ("POST", "/api/v1/preset/save", "{}", ctl::DAEMON_PRESET_SAVE, json!({})),
        ("POST", "/api/v1/preset/use", "{}", ctl::DAEMON_PRESET_USE, json!({})),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let app = setup(&dir, "/nonexistent".into()).await;
        let listener = Seqpacket::bind(&dir.path().join("test.sock")).unwrap();
        listener.listen(8).unwrap();
        let server = std::thread::spawn(move || {
            let conn = listener.accept().unwrap();
            let msg = conn.recv(Some(Duration::from_secs(2))).unwrap();
            let type_ = msg.header.type_;
            assert_eq!(type_, command);
            assert_eq!(serde_json::from_slice::<Value>(&msg.body).unwrap(), expected);
            drop(conn);
            let mut fd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 500) }, 0, "unexpected retry connection");
        });
        let response = app.router.clone().oneshot(web_request(method, path, "application/json", body.as_bytes().to_vec())).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("执行结果未知"));
        server.join().unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_uses_fixed_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("ctl");
    let args = dir.path().join("arguments");
    std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '{{\"state\":\"running\"}}'\n", args.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let app = setup(&dir, script).await;
    let req = Request::builder().method("POST").uri("/api/v1/system/up").header(header::HOST, "localhost:8080").header(header::CONTENT_TYPE, "application/json").body(Body::from("{}")).unwrap();
    assert_eq!(app.router.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    assert_eq!(std::fs::read_to_string(args).unwrap(), format!("--config\n{}\nup\n--json\n", dir.path().join("config.toml").display()));
    for output in [r#"{"state":"stopped"}"#, r#"{"state":"running","extra":1}"#, r#"{"state":1}"#] {
        std::fs::write(dir.path().join("ctl"), format!("#!/bin/sh\nprintf '%s' '{output}'\n")).unwrap();
        let req = web_request("POST", "/api/v1/system/up", "application/json", b"{}".to_vec());
        let response = app.router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("execution result unknown"));
    }
}
