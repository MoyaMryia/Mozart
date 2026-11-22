use axum::{extract::{ConnectInfo, State}, routing::get, Router};
use mozart_core::config::WebAdapterConfig;
use mozart_web_adapter::{rpc::RpcClient, streams::Streams, transport::Transport};
use std::{io::Write, os::fd::AsRawFd, time::Duration};
use tokio::io::AsyncWriteExt;

async fn subtitles(State(streams): State<Streams>, ConnectInfo(transport): ConnectInfo<Transport>) -> axum::response::Response {
    streams.subtitles_for(None, Some(transport)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_tcp_client_is_actively_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subtitles.jsonl");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let payload = "x".repeat(3000);
    for seq in 0..2000 {
        writeln!(file, "{}", serde_json::json!({"seq":seq,"text":payload})).unwrap();
    }
    file.flush().unwrap();
    drop(file);
    let size = std::fs::metadata(&path).unwrap().len();
    assert!((5 * 1024 * 1024..=8 * 1024 * 1024).contains(&size));
    let mut config = WebAdapterConfig::load_from(None).unwrap();
    config.subtitles_path = path;
    config.web.sse_buffer_bytes = 4096;
    let streams = Streams::new(RpcClient::new(None, config.clone()), config);
    let app = Router::new().route("/", get(subtitles)).with_state(streams.clone());
    let listener = mozart_web_adapter::transport::Listener::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<Transport>()).await.unwrap() });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let client = client.into_std().unwrap();
    let closed = tokio::task::spawn_blocking(move || {
        let mut fd = libc::pollfd { fd: client.as_raw_fd(), events: libc::POLLRDHUP | libc::POLLHUP, revents: 0 };
        let result = unsafe { libc::poll(&mut fd, 1, 3000) };
        assert!(result >= 0, "poll: {}", std::io::Error::last_os_error());
        fd.revents & (libc::POLLRDHUP | libc::POLLHUP) != 0
    }).await.unwrap();
    streams.shutdown();
    server.abort();
    assert!(closed, "server did not close the stalled TCP connection within three seconds");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_append_during_replay_preserves_all_sequences() {
    use http_body_util::BodyExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subtitles.jsonl");
    let mut writer = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
    for seq in 0..800 { writeln!(writer, "{{\"seq\":{seq}}}").unwrap(); }
    let mut config = WebAdapterConfig::load_from(None).unwrap();
    config.subtitles_path = path;
    config.web.sse_buffer_bytes = 1024 * 1024;
    config.web.sse_heartbeat_ms = 20;
    let streams = Streams::new(RpcClient::new(None, config.clone()), config);
    let mut body = streams.subtitles(None).await.unwrap().into_body();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let writer_task = tokio::spawn(async move {
        while let Some(batch) = rx.recv().await {
            for seq in (800 + batch * 10)..(810 + batch * 10) { writeln!(writer, "{{\"seq\":{seq}}}").unwrap(); }
        }
        writer
    });
    let mut seen = std::collections::BTreeSet::new();
    let mut batches = 0;
    tokio::time::timeout(Duration::from_secs(15), async {
        while seen.len() < 1200 {
            let frame = body.frame().await.expect("stream ended before replay completed").unwrap().into_data().unwrap();
            let text = std::str::from_utf8(&frame).unwrap();
            assert!(!text.contains("event: error"), "{text}");
            if text.contains("event: reset") { seen.clear(); continue; }
            if let Some(data) = text.lines().find_map(|line| line.strip_prefix("data: ")) {
                let value: serde_json::Value = serde_json::from_str(data).unwrap();
                let seq = value["seq"].as_u64().unwrap() as usize;
                assert!(seq < 1200);
                seen.insert(seq);
                if seq < 800 && seq % 15 == 0 && batches < 40 {
                    tx.send(batches).unwrap();
                    batches += 1;
                }
            }
        }
    }).await.expect("replay or appended subtitles stalled");
    assert_eq!(seen, (0..1200).collect());
    drop(tx);
    let _writer = writer_task.await.unwrap();
    streams.shutdown();
}
