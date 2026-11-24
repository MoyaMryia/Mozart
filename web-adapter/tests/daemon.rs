//! Real daemon supervising real web-adapter, ctl, and protocol stubs; not an inference test.
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::{
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Harness {
    _dir: tempfile::TempDir,
    wrapper: PathBuf,
    port: u16,
}

impl Harness {
    fn new(model: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bin = Path::new(env!("CARGO_BIN_EXE_mozart-web-adapter"))
            .parent()
            .unwrap();
        assert!(!root.to_string_lossy().contains('\'') && !bin.to_string_lossy().contains('\''));
        for name in [
            "mozart-daemon",
            "mozart-ctl",
            "mozart-web-adapter",
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let static_dir = root.join("static");
        std::fs::create_dir_all(&static_dir).unwrap();
        std::fs::write(static_dir.join("index.html"), "page").unwrap();
        let upload_dir = root.join("uploads");
        let config = root.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[daemon]\nrun_dir = {:?}\nrpc_timeout_ms = 300\npoll_interval_ms = 137\nstartup_mode = \"manual\"\n\n[web]\nlisten = \"127.0.0.1:{port}\"\nstatic_dir = {:?}\nupload_dir = {:?}\nallowed_origins = [\"http://127.0.0.1:{port}\"]\n\n[rvc]\nmodel_id = {:?}\n\n[file]\nstorage_dir = {:?}\nmax_upload_bytes = 100000\n",
                root.join("run"),
                static_dir,
                upload_dir,
                if model { "demo" } else { "" },
                root.join("storage"),
            ),
        )
        .unwrap();

        let wrapper = root.join("ctl");
        let script = format!(
            "#!/bin/sh\nexec env -i PATH=/usr/bin:/bin HOME='{root}' MOZART_STATE_DIR='{root}/state' MOZART_DAEMON_BIN='{bin}/mozart-daemon' MOZART_WEB_BIN='{bin}/mozart-web-adapter' MOZART_PRE_BIN='{bin}/examples/pre_stub' MOZART_RVC_BIN='{bin}/examples/rvc_stub' MOZART_ASR_BIN='{bin}/examples/asr_stub' MOZART_TTS_BIN='{bin}/examples/tts_stub' MOZART_BRIDGE_BIN='{bin}/examples/bridge_stub' MOZART_LLAMA_BIN='{bin}/examples/llama_stub' '{bin}/mozart-ctl' --config '{config}' \"$@\"\n",
            config = config.display(),
            root = root.display(),
            bin = bin.display(),
        );
        std::fs::write(&wrapper, script).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _dir: dir,
            wrapper,
            port,
        }
    }

    fn ctl(&self, args: &[&str]) -> Output {
        Command::new(&self.wrapper).args(args).output().unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = Command::new(&self.wrapper)
            .args(["down", "--json"])
            .output();
    }
}

async fn http_call(
    port: u16,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> std::io::Result<(StatusCode, String, Vec<u8>)> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    head.push_str("\r\n");
    let mut raw = head.into_bytes();
    raw.extend_from_slice(body);
    stream.write_all(&raw).await?;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "http timeout"))??;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing CRLFCRLF"))?;
    let headers = String::from_utf8_lossy(&response[..split]);
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad status line"))?;
    let ct = headers
        .lines()
        .skip(1)
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case("content-type").then(|| v.trim().to_owned())
        })
        .unwrap_or_default();
    Ok((status, ct, response[split + 4..].to_vec()))
}

async fn json_call(port: u16, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
    let ct = if body.is_empty() {
        None
    } else {
        Some("application/json")
    };
    let (status, ct, bytes) = http_call(port, method, path, ct, body.as_bytes())
        .await
        .unwrap();
    assert_eq!(ct, "application/json", "unexpected content type for {path}");
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn wait_adapter_up(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok((StatusCode::OK, ct, bytes)) = http_call(port, "GET", "/api/v1/health", None, b"").await {
            if ct == "application/json" {
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                if body["data"]["adapter"] == "running" {
                    return;
                }
            }
        }
        assert!(Instant::now() < deadline, "supervised web-adapter did not become ready on port {port}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
async fn real_daemon_supervises_web_lifecycle_and_file() {
    let h = Harness::new(true);
    let up_out = h.ctl(&["up", "--json"]);
    assert!(
        up_out.status.success(),
        "ctl up failed: {}",
        String::from_utf8_lossy(&up_out.stderr)
    );
    let p = h.port;
    wait_adapter_up(p).await;

    let deadline = Instant::now() + Duration::from_secs(8);
    let web_pid = loop {
        let (code, status) = json_call(p, "GET", "/api/v1/status", "").await;
        assert_eq!(code, StatusCode::OK, "{status}");
        assert_eq!(status["data"]["services_active"], true);
        assert_eq!(status["data"]["daemon"]["poll_interval_ms"], 137);
        assert_eq!(status["data"]["file_limits"]["max_upload_bytes"], 100000);
        assert!(status["data"]["api_capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("file_fetch_safe_errors_v1")));
        assert_eq!(status["data"]["processes"]["web"]["state"], "running");
        if !status["data"]["rvc"].is_null() && !status["data"]["pre"].is_null() {
            break status["data"]["processes"]["web"]["pid"].as_u64().unwrap();
        }
        assert!(Instant::now() < deadline, "children not ready: {status}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let (code, set) = json_call(
        p,
        "PUT",
        "/api/v1/config",
        r#"{"key":"rvc.pitch_shift","value":"3"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{set}");
    assert_eq!(set["data"]["key"], "rvc.pitch_shift");
    let (code, bad) = json_call(
        p,
        "PUT",
        "/api/v1/config",
        r#"{"key":"rvc.pitch_shift","value":"999"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
    assert_eq!(bad["status"], "error");
    let (code, bad) = json_call(p, "PUT", "/api/v1/route", r#"{"route":"bogus"}"#).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
    assert_eq!(bad["status"], "error");
    let (code, config) = json_call(p, "GET", "/api/v1/config?key=rvc.pitch_shift", "").await;
    assert_eq!(code, StatusCode::OK, "{config}");
    assert_eq!(config["data"]["value"], 3);
    let (code, status) = json_call(p, "GET", "/api/v1/status", "").await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["data"]["rvc_params"]["pitch_shift"], 3);

    // Restore neutral pitch_shift for deterministic echo fixture.
    let (code, value) = json_call(
        p,
        "PUT",
        "/api/v1/config",
        r#"{"key":"rvc.pitch_shift","value":"0"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{value}");

    let mut multipart = b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"tone.wav\"\r\nContent-Type: audio/wav\r\n\r\n".to_vec();
    multipart.extend_from_slice(&wav_fixture());
    multipart.extend_from_slice(b"\r\n--boundary\r\nContent-Disposition: form-data; name=\"model_id\"\r\n\r\ndemo\r\n--boundary--\r\n");
    let (code, _, body) = http_call(
        p,
        "POST",
        "/api/v1/files",
        Some("multipart/form-data; boundary=boundary"),
        &multipart,
    )
    .await
    .unwrap();
    let submitted: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(code, StatusCode::ACCEPTED, "{submitted}");
    let id = submitted["data"]["job_id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (code, job) = json_call(p, "GET", &format!("/api/v1/files/{id}"), "").await;
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
    let (code, ct, wav) = http_call(p, "GET", &format!("/api/v1/files/{id}/result"), None, b"")
        .await
        .unwrap();
    assert_eq!(code, StatusCode::OK);
    assert_eq!(ct, "audio/wav");
    verify_wav(&wav);

    // Web system/down stops audio+AI business services while keeping daemon and web online.
    let (code, down) = json_call(p, "POST", "/api/v1/system/down", "{}").await;
    assert_eq!(code, StatusCode::OK, "{down}");
    assert_eq!(down["data"]["state"], "stopped");

    let (code, health) = json_call(p, "GET", "/api/v1/health", "").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(health["data"]["adapter"], "running");

    let (code, status) = json_call(p, "GET", "/api/v1/status", "").await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["data"]["services_active"], false);
    assert_eq!(status["data"]["processes"]["web"]["state"], "running");
    assert_eq!(status["data"]["processes"]["web"]["pid"], web_pid);
    for role in ["enhance", "pre", "rvc", "asr", "tts", "bridge", "llama"] {
        assert_eq!(
            status["data"]["processes"][role]["state"], "stopped",
            "role {role} should be stopped"
        );
    }

    let (code, gated) = json_call(p, "PUT", "/api/v1/route", r#"{"route":"rvc"}"#).await;
    assert_eq!(code, StatusCode::CONFLICT, "{gated}");
    assert_eq!(gated["code"], "NotReady");

    // Web system/up restores business services without dropping daemon or web.
    let (code, up) = json_call(p, "POST", "/api/v1/system/up", "{}").await;
    assert_eq!(code, StatusCode::OK, "{up}");
    assert_eq!(up["data"]["state"], "running");

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let (code, status) = json_call(p, "GET", "/api/v1/status", "").await;
        assert_eq!(code, StatusCode::OK, "{status}");
        if status["data"]["services_active"] == true
            && !status["data"]["pre"].is_null()
            && !status["data"]["rvc"].is_null()
        {
            break;
        }
        assert!(Instant::now() < deadline, "services did not recover: {status}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Full host shutdown stops daemon and supervised web-adapter together.
    let down_out = h.ctl(&["down", "--json"]);
    assert!(
        down_out.status.success(),
        "ctl down failed: {}",
        String::from_utf8_lossy(&down_out.stderr)
    );
    let proc_path = format!("/proc/{web_pid}");
    let deadline = Instant::now() + Duration::from_secs(4);
    while Path::new(&proc_path).exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !Path::new(&proc_path).exists(),
        "supervised web-adapter (pid {web_pid}) survived ctl down"
    );
    assert!(http_call(p, "GET", "/api/v1/health", None, b"").await.is_err());
}

#[tokio::test]
async fn real_daemon_rejects_rvc_route_without_model() {
    let h = Harness::new(false);
    let up_out = h.ctl(&["up", "--json"]);
    assert!(
        up_out.status.success(),
        "ctl up failed: {}",
        String::from_utf8_lossy(&up_out.stderr)
    );
    let p = h.port;
    wait_adapter_up(p).await;

    let (code, rejection) = json_call(p, "PUT", "/api/v1/route", r#"{"route":"rvc"}"#).await;
    assert_eq!(code, StatusCode::CONFLICT, "{rejection}");
    assert_eq!(rejection["code"], "NotReady");
    assert!(rejection["reason"].as_str().unwrap().contains("model_id"));
    let (code, status) = json_call(p, "GET", "/api/v1/status", "").await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["data"]["route"]["active"], "direct");
}
