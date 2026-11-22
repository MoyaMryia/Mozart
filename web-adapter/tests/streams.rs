use axum::{body::Body, http::header};
use http_body_util::BodyExt;
use mozart_core::config::WebAdapterConfig;
use mozart_web_adapter::{rpc::RpcClient, streams::Streams};
use std::{io::Write, time::Duration};

fn setup() -> (tempfile::TempDir, std::path::PathBuf, Streams) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subtitles.jsonl");
    let mut config = WebAdapterConfig::load_from(None).unwrap();
    config.subtitles_path = path.clone();
    config.web.sse_heartbeat_ms = 20;
    let rpc = RpcClient::new(None, config.clone());
    (dir, path, Streams::new(rpc, config))
}
async fn business_frame(body: &mut Body) -> String {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let data = body.frame().await.expect("stream ended").unwrap().into_data().unwrap();
            let text = String::from_utf8(data.to_vec()).unwrap();
            if !text.starts_with(':') { return text; }
        }
    }).await.expect("event did not arrive")
}
fn id(frame: &str) -> String { frame.lines().find_map(|line| line.strip_prefix("id: ")).unwrap().to_owned() }

#[tokio::test]
async fn replay_partial_line_resume_and_shutdown() {
    let (_dir, path, streams) = setup();
    std::fs::write(&path, b"{\"n\":1}\n{\"n\":2").unwrap();
    let mut writer = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    let response = streams.subtitles(None).await.unwrap();
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut body = response.into_body();
    assert!(business_frame(&mut body).await.contains("event: reset"));
    let first = business_frame(&mut body).await;
    assert!(first.contains("data: {\"n\":1}"));
    let cursor = id(&first); assert!(cursor.ends_with(":8"));
    assert!(tokio::time::timeout(Duration::from_millis(80), business_frame(&mut body)).await.is_err());
    writer.write_all(b"}\n").unwrap();
    writer.flush().unwrap();
    let second = business_frame(&mut body).await;
    assert!(second.contains("event: subtitle")); assert!(second.contains("data: {\"n\":2}"));
    drop(body);
    let mut resumed = streams.subtitles(Some(cursor)).await.unwrap().into_body();
    let replay = business_frame(&mut resumed).await;
    assert!(!replay.contains("reset")); assert!(replay.contains("data: {\"n\":2}"));
    streams.shutdown();
    assert!(tokio::time::timeout(Duration::from_secs(2), resumed.frame()).await.unwrap().is_none());
}

#[tokio::test]
async fn same_inode_rewrite_resets_and_bad_line_errors() {
    let (_dir, path, streams) = setup();
    std::fs::write(&path, b"{\"old\":1}\n").unwrap();
    let mut body = streams.subtitles(None).await.unwrap().into_body();
    business_frame(&mut body).await;
    let original = id(&business_frame(&mut body).await);
    std::fs::write(&path, b"{\"new\":123456789}\n").unwrap();
    let reset = business_frame(&mut body).await;
    assert!(reset.contains("event: reset"));
    let replacement = business_frame(&mut body).await;
    assert!(replacement.contains("data: {\"new\":123456789}"));
    assert_ne!(id(&replacement).split(':').next(), original.split(':').next());
    std::fs::write(&path, b"not json\n").unwrap();
    assert!(business_frame(&mut body).await.contains("event: reset"));
    let error = business_frame(&mut body).await;
    assert!(error.contains("event: error")); assert!(error.contains("SourceUnavailable"));
    assert!(body.frame().await.is_none()); streams.shutdown();
}

#[tokio::test]
async fn cursor_rejected_before_stream_headers_and_missing_file_waits() {
    let (_dir, path, streams) = setup();
    let mut body = streams.subtitles(None).await.unwrap().into_body();
    assert!(business_frame(&mut body).await.contains("event: reset"));
    std::fs::write(&path, b"{}\n").unwrap();
    let reset = business_frame(&mut body).await; assert!(reset.contains("event: reset"));
    let frame = loop {
        let frame = business_frame(&mut body).await;
        if frame.contains("event: subtitle") { break frame; }
        assert!(frame.contains("event: reset"));
    };
    let cursor = id(&frame);
    let generation = cursor.split(':').next().unwrap();
    let error = streams.subtitles(Some(format!("{generation}:1"))).await.unwrap_err();
    assert_eq!(error.code, "BadRequest");
    assert_eq!(streams.subtitles(Some("invalid".into())).await.unwrap_err().code, "BadRequest");
    streams.shutdown();
}
