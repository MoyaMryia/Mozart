//! TTS 进程协议桩：只实现 SPEC-05 控制面（Hello/GetStatus），不合成、不重采样。

mod stub_common;

use mozart_core::ctl::schema::TtsStatus;
use mozart_core::ctl::{self, Message};
use serde_json::json;
use stub_common::{body, expect_known, ok, parse_args, serve};

fn main() {
    let args = parse_args("mozart-tts-stub");
    serve(args, move |msg| handle(msg));
}

fn handle(msg: &Message) -> Vec<u8> {
    let known = [ctl::TTS_HELLO, ctl::TTS_GET_STATUS];
    if let Err(reply) = expect_known(msg, &known) {
        return reply;
    }
    if let Err(reply) = body(msg) {
        return reply;
    }
    match msg.header.type_ {
        ctl::TTS_HELLO => ok(json!({"version": "0.1.0", "stub": true})),
        ctl::TTS_GET_STATUS => {
            let status = TtsStatus {
                version: "0.1.0".to_string(),
                ..Default::default()
            };
            ok(serde_json::to_value(status).expect("TtsStatus is JSON-serializable"))
        }
        _ => unreachable!("expect_known covers all commands"),
    }
}