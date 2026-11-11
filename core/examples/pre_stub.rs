//! 音频进程协议桩：只实现 SPEC-02 控制面，不采集、不播放。

mod stub_common;

use mozart_core::ctl::{self, Message};
use serde_json::json;
use stub_common::{body, expect_known, fail, ok, parse_args, serve};

struct State {
    route: &'static str,
    mute: bool,
}

fn main() {
    let args = parse_args("mozart-pre-stub");
    let mut state = State {
        route: "direct",
        mute: false,
    };
    serve(args, move |msg| handle(msg, &mut state));
}

fn handle(msg: &Message, state: &mut State) -> Vec<u8> {
    let known = [
        ctl::PRE_HELLO,
        ctl::PRE_SET_ROUTE,
        ctl::PRE_SET_MUTE,
        ctl::PRE_GET_STATUS,
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
        ctl::PRE_HELLO => ok(json!({"version": "0.1.0", "stub": true})),
        ctl::PRE_SET_ROUTE => {
            let route = body.get("route").and_then(|value| value.as_str());
            match route {
                Some("direct") => state.route = "direct",
                Some("rvc") => state.route = "rvc",
                _ => {
                    return fail(
                        ctl::CODE_INVALID_ARGUMENT,
                        "route must be \"direct\" or \"rvc\"",
                    )
                }
            }
            ok(json!({"route": state.route}))
        }
        ctl::PRE_SET_MUTE => {
            let Some(mute) = body.get("mute").and_then(|value| value.as_bool()) else {
                return fail(ctl::CODE_INVALID_ARGUMENT, "mute must be a boolean");
            };
            state.mute = mute;
            ok(json!({"mute": state.mute}))
        }
        ctl::PRE_GET_STATUS => ok(json!({
            "state": "ok",
            "route": state.route,
            "mute": state.mute,
            "capture_device": "",
            "playback_device": "",
            "frames_captured": 0,
            "frames_played": 0,
            "frames_dropped": 0,
            "queue_depth": 0,
            "consumer_drops": {},
            "last_error": null,
            "version": "0.1.0"
        })),
        _ => unreachable!("expect_known covers all commands"),
    }
}
