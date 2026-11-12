//! 识别进程协议桩：只实现 SPEC-05 控制面（Hello/GetStatus），不解码、不复核。

mod stub_common;

use mozart_core::ctl::schema::AsrStatus;
use mozart_core::ctl::{self, Message};
use serde_json::json;
use stub_common::{body, expect_known, ok, parse_args, serve};

fn main() {
    let args = parse_args("mozart-asr-stub");
    serve(args, move |msg| handle(msg));
}

fn handle(msg: &Message) -> Vec<u8> {
    let known = [ctl::ASR_HELLO, ctl::ASR_GET_STATUS];
    if let Err(reply) = expect_known(msg, &known) {
        return reply;
    }
    if let Err(reply) = body(msg) {
        return reply;
    }
    match msg.header.type_ {
        ctl::ASR_HELLO => ok(json!({"version": "0.1.0", "startup_id": "stub", "stub": true})),
        ctl::ASR_GET_STATUS => {
            let status = AsrStatus {
                startup_id: "stub".to_string(),
                version: "0.1.0".to_string(),
                ..Default::default()
            };
            ok(serde_json::to_value(status).expect("AsrStatus is JSON-serializable"))
        }
        _ => unreachable!("expect_known covers all commands"),
    }
}