//! 仅用于 daemon 生命周期测试的控制协议桩，不加载模型、不处理音频。
use mozart_core::ctl::{self, schema, Seqpacket};
use serde_json::json;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut socket = None;
    let mut audio_socket = None;
    let mut model = None;
    while let Some(flag) = args.next() {
        let value = args.next().expect("flag requires value");
        match flag.as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--audio-socket" => audio_socket = Some(PathBuf::from(value)),
            "--model" => model = Some(value),
            _ => panic!("unknown flag {flag}"),
        }
    }
    let socket = socket.expect("--socket");
    let _audio_socket = audio_socket.expect("--audio-socket");
    assert!(model.unwrap().ends_with("test.onnx"));
    if socket.exists() {
        std::fs::remove_file(&socket).unwrap();
    }
    let listener = Seqpacket::bind(&socket).unwrap();
    listener.listen(4).unwrap();
    loop {
        let conn = listener.accept().unwrap();
        loop {
            let msg = match conn.recv(None) {
                Ok(msg) => msg,
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => panic!("stub recv: {err}"),
            };
            let body = match msg.header.type_ {
                ctl::ENHANCE_HELLO => schema::ok_body(json!({"version":"stub"})),
                ctl::ENHANCE_GET_STATUS => schema::ok_body(json!({
                    "version":"stub", "backend":"stub-only", "frames_in":0,
                    "frames_out":0, "sessions":0, "algorithm_delay_samples":640,
                    "last_error":null
                })),
                _ => schema::error_body(ctl::CODE_UNKNOWN_COMMAND, "unknown command"),
            };
            conn.send(msg.header.type_, msg.header.request_id, &body)
                .unwrap();
        }
    }
}
