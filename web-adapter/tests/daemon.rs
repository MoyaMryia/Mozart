//! Real daemon and ctl with protocol stubs, not an inference test.
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use mozart_core::config::WebAdapterConfig;
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tower::ServiceExt;

struct Harness {
    _dir: tempfile::TempDir,
    config: PathBuf,
    wrapper: PathBuf,
    router: Router,
}
impl Harness {
    async fn new(model: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Cargo reports the final binary path. The check script builds the fixtures
        // into the same profile directory, with protocol stubs under examples/.
        let bin = Path::new(env!("CARGO_BIN_EXE_mozart-web-adapter"))
            .parent()
            .unwrap();
        // The wrapper embeds these paths in single-quoted shell arguments.
        assert!(!root.to_string_lossy().contains('\'') && !bin.to_string_lossy().contains('\''));
        for name in [
            "mozart-daemon",
            "mozart-ctl",
            "examples/pre_stub",
            "examples/rvc_stub",
            "examples/asr_stub",
            "examples/tts_stub",
            "examples/bridge_stub",
            "examples/llama_stub",
        ] {
            assert!(
                bin.join(name).is_file(),
                "missing {}; run tools/ci-local.sh adapter to build the integration fixtures",
                bin.join(name).display()
            );
        }
        let config = root.join("config.toml");
        std::fs::write(&config, format!("[daemon]\nrun_dir = {:?}\nrpc_timeout_ms = 300\npoll_interval_ms = 137\n\n[rvc]\nmodel_id = {:?}\n\n[file]\nstorage_dir = {:?}\nmax_upload_bytes = 100000\n", root.join("run"), if model { "demo" } else { "" }, root.join("storage"))).unwrap();
        let wrapper = root.join("ctl");
        // Fixed arguments and a clean environment prevent lifecycle calls escaping the sandbox.
        let script = format!("#!/bin/sh\n[ \"$1\" = --config ] && [ \"$2\" = '{config}' ] && [ \"$4\" = --json ] && [ \"$#\" = 4 ] || exit 2\ncase \"$3\" in up|down) ;; *) exit 2 ;; esac\nexec env -i PATH=/usr/bin:/bin HOME='{root}' MOZART_STATE_DIR='{root}/state' MOZART_DAEMON_BIN='{bin}/mozart-daemon' MOZART_PRE_BIN='{bin}/examples/pre_stub' MOZART_RVC_BIN='{bin}/examples/rvc_stub' MOZART_ASR_BIN='{bin}/examples/asr_stub' MOZART_TTS_BIN='{bin}/examples/tts_stub' MOZART_BRIDGE_BIN='{bin}/examples/bridge_stub' MOZART_LLAMA_BIN='{bin}/examples/llama_stub' '{bin}/mozart-ctl' --config '{config}' \"$3\" --json\n", config=config.display(), root=root.display(), bin=bin.display());
        std::fs::write(&wrapper, script).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut cfg = WebAdapterConfig::load_from(Some(&config)).unwrap();
        // Environment overrides also affect adapter RPC configuration. Never contact
        // an existing instance if the invoking environment overrides the sandbox.
        assert_eq!(
            cfg.daemon.run_dir,
            root.join("run"),
            "MOZART_RUN_DIR overrides test isolation"
        );
        assert_eq!(
            cfg.daemon.poll_interval_ms, 137,
            "MOZART_POLL_INTERVAL_MS overrides the test configuration"
        );
        cfg.web.static_dir = root.join("static");
        cfg.web.upload_dir = root.join("uploads");
        cfg.web.allowed_origins = vec!["http://localhost:8080".into()];
        let app = mozart_web_adapter::build(cfg, Some(config.clone()), wrapper.clone())
            .await
            .unwrap();
        Self {
            _dir: dir,
            config,
            wrapper,
            router: app.router,
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = Command::new(&self.wrapper)
            .arg("--config")
            .arg(&self.config)
            .args(["down", "--json"])
            .output();
    }
}

fn req(method: &str, path: &str, content_type: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost:8080")
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap()
}
async fn json_call(router: &Router, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(req(method, path, "application/json", body.to_owned()))
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn wav_fixture() -> Vec<u8> {
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&32036u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&16000u32.to_le_bytes());
    wav.extend_from_slice(&32000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&32000u32.to_le_bytes());
    for i in 0..16000 {
        let sample: i16 = if i % 2 == 0 { 8192 } else { -8192 };
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}
fn verify_wav(bytes: &[u8]) {
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8,
        bytes.len()
    );
    let mut pos = 12;
    let mut fmt = None;
    let mut data = None;
    while pos + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let chunk = &bytes[pos + 8..pos + 8 + size];
        match &bytes[pos..pos + 4] {
            b"fmt " => fmt = Some(chunk),
            b"data" => data = Some(chunk),
            _ => {}
        }
        pos += 8 + size + size % 2;
    }
    let f = fmt.unwrap();
    let tag = u16::from_le_bytes(f[0..2].try_into().unwrap());
    assert!(tag == 3 || (tag == 0xfffe && u16::from_le_bytes(f[24..26].try_into().unwrap()) == 3));
    assert_eq!(u16::from_le_bytes(f[2..4].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()), 48000);
    assert_eq!(u16::from_le_bytes(f[14..16].try_into().unwrap()), 32);
    let data = data.unwrap();
    assert_eq!(data.len(), 48000 * 4);
    for (i, chunk) in data.chunks_exact(4).enumerate().take(960) {
        let actual = f32::from_le_bytes(chunk.try_into().unwrap());
        let expected = if (i / 3) % 2 == 0 { 0.25 } else { -0.25 };
        assert!(
            (actual - expected).abs() < 1e-6,
            "sample {i}: {actual} != {expected}"
        );
    }
}

#[tokio::test]
async fn real_daemon_http_lifecycle_and_file() {
    let h = Harness::new(true).await;
    let r = &h.router;
    let (code, up) = json_call(r, "POST", "/api/v1/system/up", "{}").await;
    assert_eq!(code, StatusCode::OK, "{up}");
    assert_eq!(up["data"]["state"], "running");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let (code, status) = json_call(r, "GET", "/api/v1/status", "").await;
        assert_eq!(code, StatusCode::OK, "{status}");
        assert_eq!(status["data"]["daemon"]["poll_interval_ms"], 137);
        assert_eq!(status["data"]["file_limits"]["max_upload_bytes"], 100000);
        assert!(status["data"]["api_capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("file_fetch_safe_errors_v1")));
        if !status["data"]["rvc"].is_null() && !status["data"]["pre"].is_null() {
            break;
        }
        assert!(Instant::now() < deadline, "children not ready: {status}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (code, set) = json_call(
        r,
        "PUT",
        "/api/v1/config",
        r#"{"key":"rvc.pitch_shift","value":"3"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{set}");
    assert_eq!(set["data"]["key"], "rvc.pitch_shift");
    let (code, bad) = json_call(
        r,
        "PUT",
        "/api/v1/config",
        r#"{"key":"rvc.pitch_shift","value":"999"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
    assert_eq!(bad["status"], "error");
    let (code, bad) = json_call(r, "PUT", "/api/v1/route", r#"{"route":"bogus"}"#).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
    assert_eq!(bad["status"], "error");
    let (code, config) = json_call(r, "GET", "/api/v1/config?key=rvc.pitch_shift", "").await;
    assert_eq!(code, StatusCode::OK, "{config}");
    assert_eq!(config["data"]["value"], 3);
    let (code, status) = json_call(r, "GET", "/api/v1/status", "").await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["data"]["rvc_params"]["pitch_shift"], 3);
    // Restore the neutral setting for the deterministic echo fixture.
    let (code, value) = json_call(
        r,
        "PUT",
        "/api/v1/config",
        r#"{"key":"rvc.pitch_shift","value":"0"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{value}");
    let mut multipart = b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"tone.wav\"\r\nContent-Type: audio/wav\r\n\r\n".to_vec();
    multipart.extend_from_slice(&wav_fixture());
    multipart.extend_from_slice(b"\r\n--boundary\r\nContent-Disposition: form-data; name=\"model_id\"\r\n\r\ndemo\r\n--boundary--\r\n");
    let response = r
        .clone()
        .oneshot(req(
            "POST",
            "/api/v1/files",
            "multipart/form-data; boundary=boundary",
            multipart,
        ))
        .await
        .unwrap();
    let code = response.status();
    let body = to_bytes(response.into_body(), 8192).await.unwrap();
    let submitted: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(code, StatusCode::ACCEPTED, "{submitted}");
    let id = submitted["data"]["job_id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (code, job) = json_call(r, "GET", &format!("/api/v1/files/{id}"), "").await;
        assert_eq!(code, StatusCode::OK, "{job}");
        let state = job["data"]["state"].as_str().unwrap();
        if state == "completed" {
            assert_eq!(job["data"]["result_ready"], true);
            break;
        }
        assert!(!["failed", "cancelled"].contains(&state), "{job}");
        assert!(Instant::now() < deadline, "job timeout: {job}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let result = r
        .clone()
        .oneshot(req(
            "GET",
            &format!("/api/v1/files/{id}/result"),
            "",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::OK);
    assert_eq!(result.headers()[header::CONTENT_TYPE], "audio/wav");
    verify_wav(&to_bytes(result.into_body(), 1024 * 1024).await.unwrap());
    let (code, down) = json_call(r, "POST", "/api/v1/system/down", "{}").await;
    assert_eq!(code, StatusCode::OK, "{down}");
    assert_eq!(down["data"]["state"], "stopped");
    let (code, health) = json_call(r, "GET", "/api/v1/health", "").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(health["data"]["adapter"], "running");
    let (code, status) = json_call(r, "GET", "/api/v1/status", "").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{status}");
    let (code, up) = json_call(r, "POST", "/api/v1/system/up", "{}").await;
    assert_eq!(code, StatusCode::OK, "{up}");
    assert_eq!(up["data"]["state"], "running");
    let (code, status) = json_call(r, "GET", "/api/v1/status", "").await;
    assert_eq!(code, StatusCode::OK, "{status}");
}

#[tokio::test]
async fn real_daemon_rejects_rvc_route_without_model() {
    let h = Harness::new(false).await;
    let r = &h.router;
    let (code, up) = json_call(r, "POST", "/api/v1/system/up", "{}").await;
    assert_eq!(code, StatusCode::OK, "{up}");
    let (code, rejection) = json_call(r, "PUT", "/api/v1/route", r#"{"route":"rvc"}"#).await;
    assert_eq!(code, StatusCode::CONFLICT, "{rejection}");
    assert_eq!(rejection["code"], "NotReady");
    assert!(rejection["reason"].as_str().unwrap().contains("model_id"));
    let (code, status) = json_call(r, "GET", "/api/v1/status", "").await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["data"]["route"]["active"], "direct");
}
