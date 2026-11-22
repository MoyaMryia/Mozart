use http_body_util::BodyExt;
use mozart_core::{config::WebAdapterConfig, ctl::{Seqpacket, DAEMON_GET_STATUS}};
use mozart_web_adapter::{rpc::RpcClient, streams::Streams};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn each_status_client_gets_a_fresh_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = WebAdapterConfig::load_from(None).unwrap();
    config.daemon.run_dir = dir.path().to_owned();
    config.daemon.socket = "status.sock".into();
    let listener = Seqpacket::bind(&config.daemon_socket()).unwrap();
    listener.listen(8).unwrap();
    let server = std::thread::spawn(move || {
        for marker in [1, 2] {
            let conn = listener.accept().unwrap();
            let request = conn.recv(Some(Duration::from_secs(3))).unwrap();
            let command = request.header.type_;
            let id = request.header.request_id;
            assert_eq!(command, DAEMON_GET_STATUS);
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(), json!({}));
            let reply = json!({"status":"ok", "code":"Ok", "reason":"", "data":{"marker":marker,"daemon":{"poll_interval_ms":60000}}});
            conn.send(command, id, &serde_json::to_vec(&reply).unwrap()).unwrap();
        }
    });
    let streams = Streams::new(RpcClient::new(None, config.clone()), config);
    let mut first = tokio::time::timeout(Duration::from_secs(3), streams.status()).await.unwrap().unwrap().into_body();
    let frame = first.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(String::from_utf8_lossy(&frame).contains("\"marker\":1"));
    let mut second = tokio::time::timeout(Duration::from_secs(3), streams.status()).await.expect("second snapshot waited for poll interval").unwrap().into_body();
    let frame = tokio::time::timeout(Duration::from_secs(3), second.frame()).await.unwrap().unwrap().unwrap().into_data().unwrap();
    assert!(String::from_utf8_lossy(&frame).contains("\"marker\":2"), "{frame:?}");
    streams.shutdown();
    server.join().unwrap();
}
