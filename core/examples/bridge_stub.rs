//! 字幕桥协议桩：实现 SPEC-05 控制面（Hello/GetStatus + 字幕开关命令），
//! 只维护开关活状态，不收事件、不落盘、不翻译、不投递。

mod stub_common;

use mozart_core::ctl::schema::BridgeStatus;
use mozart_core::ctl::{self, Message};
use serde_json::json;
use stub_common::{body, expect_known, fail, ok, parse_args, serve};

struct State {
    translate_enabled: bool,
    speech_enabled: bool,
}

fn main() {
    let args = parse_args("mozart-bridge-stub");
    let mut state = State {
        translate_enabled: true,
        speech_enabled: true,
    };
    serve(args, move |msg| handle(msg, &mut state));
}

fn handle(msg: &Message, state: &mut State) -> Vec<u8> {
    let known = [
        ctl::BRIDGE_HELLO,
        ctl::BRIDGE_GET_STATUS,
        ctl::BRIDGE_TRANSLATE_SET,
        ctl::BRIDGE_SPEECH_SET,
        ctl::BRIDGE_SPEECH_STOP,
    ];
    if let Err(reply) = expect_known(msg, &known) {
        return reply;
    }
    let body = match body(msg) {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let type_ = msg.header.type_;
    match type_ {
        ctl::BRIDGE_HELLO => ok(json!({"version": "0.1.0", "stub": true})),
        ctl::BRIDGE_GET_STATUS => {
            let status = BridgeStatus {
                translate_enabled: state.translate_enabled,
                speech_enabled: state.speech_enabled,
                version: "0.1.0".to_string(),
                ..Default::default()
            };
            ok(serde_json::to_value(status).expect("BridgeStatus is JSON-serializable"))
        }
        ctl::BRIDGE_TRANSLATE_SET | ctl::BRIDGE_SPEECH_SET => {
            let Some(enabled) = body.get("enabled").and_then(|value| value.as_bool()) else {
                return fail(ctl::CODE_INVALID_ARGUMENT, "enabled must be a boolean");
            };
            if type_ == ctl::BRIDGE_TRANSLATE_SET {
                state.translate_enabled = enabled;
            } else {
                state.speech_enabled = enabled;
            }
            ok(json!({
                "translate_enabled": state.translate_enabled,
                "speech_enabled": state.speech_enabled,
            }))
        }
        ctl::BRIDGE_SPEECH_STOP => {
            state.speech_enabled = false;
            ok(json!({"speech_enabled": false, "queue_cleared": 0}))
        }
        _ => unreachable!("expect_known covers all commands"),
    }
}