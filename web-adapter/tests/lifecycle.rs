#![cfg(unix)]

use axum::{body::Body, http::{header, Request, StatusCode}};
use mozart_core::config::WebAdapterConfig;
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use tower::ServiceExt;

fn request(action: &str) -> Request<Body> {
    Request::builder().method("POST").uri(format!("/api/v1/system/{action}"))
        .header(header::HOST, "localhost:8080").header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}")).unwrap()
}

async fn wait_for(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await.expect("CLI did not reach the expected point");
}

#[tokio::test]
async fn cancellation_does_not_release_lifecycle_lock() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, format!("[daemon]\nrun_dir = {:?}\nsocket = \"test.sock\"\n", dir.path().to_str().unwrap())).unwrap();
    let mut config = WebAdapterConfig::load_from(Some(&config_path)).unwrap();
    config.web.static_dir = dir.path().join("static");
    config.web.upload_dir = dir.path().join("uploads");
    config.web.allowed_origins = vec!["http://localhost:8080".into()];
    std::fs::create_dir_all(&config.web.static_dir).unwrap();
    let script = dir.path().join("ctl");
    let log = dir.path().join("order");
    let started = dir.path().join("started");
    let release = dir.path().join("release");
    std::fs::write(&script, format!(r##"#!/bin/sh
case "$3" in
  up)
    echo up-start >> '{log}'
    touch '{started}'
    while [ ! -e '{release}' ]; do sleep 0.01; done
    echo up-end >> '{log}'
    printf '{{"state":"running"}}'
    ;;
  down)
    echo down-start >> '{log}'
    printf '{{"state":"stopped"}}'
    ;;
esac
"##, log = log.display(), started = started.display(), release = release.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let app = mozart_web_adapter::build(config, Some(config_path), script).await.unwrap();
    let first = tokio::spawn(app.router.clone().oneshot(request("up")));
    wait_for(&started).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let mut second = tokio::spawn(app.router.clone().oneshot(request("down")));
    assert!(tokio::time::timeout(Duration::from_millis(200), &mut second).await.is_err(), "down completed before up was released");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "up-start\n");
    std::fs::write(&release, "").unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), second).await.unwrap().unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(std::fs::read_to_string(log).unwrap(), "up-start\nup-end\ndown-start\n");
}
