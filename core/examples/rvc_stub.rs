//! 推理进程协议桩：只实现 SPEC-03 控制面，不加载模型、不推理。

mod stub_common;

use mozart_core::ctl::{self, Message};
use serde_json::json;
use stub_common::{body, expect_known, fail, ok, parse_args, serve};

#[derive(Default)]
struct State {
    model: Option<String>,
    streaming: bool,
    kind: Option<String>,
}

fn main() {
    let args = parse_args("mozart-rvc-stub");
    let mut state = State::default();
    serve(args, move |msg| handle(msg, &mut state));
}

fn handle(msg: &Message, state: &mut State) -> Vec<u8> {
    let known = [
        ctl::RVC_HELLO,
        ctl::RVC_LOAD_MODEL,
        ctl::RVC_START_STREAM,
        ctl::RVC_STOP_STREAM,
        ctl::RVC_RESET_STREAM,
        ctl::RVC_GET_STATUS,
        ctl::RVC_LIST_MODELS,
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
        ctl::RVC_HELLO => ok(json!({"version": "0.1.0", "stub": true})),
        ctl::RVC_LOAD_MODEL => {
            let model_id = body.get("model_id").and_then(|value| value.as_str());
            let Some(model_id) = model_id.filter(|id| !id.is_empty()) else {
                return fail(ctl::CODE_INVALID_ARGUMENT, "model_id is required");
            };
            state.model = Some(model_id.to_string());
            ok(json!({
                "model_id": model_id,
                "model_version": "stub",
                "model_handle": 1,
                "capabilities": ["realtime"]
            }))
        }
        ctl::RVC_START_STREAM => {
            if state.model.is_none() {
                return fail(ctl::CODE_NOT_LOADED, "load a model first");
            }
            if state.streaming {
                return fail(ctl::CODE_BUSY, "stream already active");
            }
            let kind = body
                .get("kind")
                .and_then(|value| value.as_str())
                .unwrap_or("realtime");
            if kind != "realtime" && kind != "file" {
                return fail(ctl::CODE_INVALID_ARGUMENT, "kind must be realtime or file");
            }
            state.streaming = true;
            state.kind = Some(kind.to_string());
            ok(json!({"stream_kind": kind}))
        }
        ctl::RVC_STOP_STREAM | ctl::RVC_RESET_STREAM => {
            state.streaming = false;
            ok(json!({}))
        }
        ctl::RVC_GET_STATUS => {
            let state_name = if state.streaming {
                "streaming"
            } else if state.model.is_some() {
                "loaded"
            } else {
                "idle"
            };
            ok(json!({
                "state": state_name,
                "model_id": state.model.as_ref(),
                "model_version": state.model.as_ref().map(|_| "stub"),
                "capabilities": ["realtime"],
                "stream_kind": state.kind.as_ref(),
                "frames_in": 0,
                "frames_out": 0,
                "frames_dropped": 0,
                "blocks": 0,
                "inference_ms_last": null,
                "inference_ms_avg": null,
                "inference_ms_p95": null,
                "queue_depth": 0,
                "last_error": null
            }))
        }
        ctl::RVC_LIST_MODELS => {
            // 桩模型目录固定一个 demo；已加载的其他模型一并列出。
            let mut ids = vec!["demo".to_string()];
            if let Some(model) = state.model.as_ref() {
                if model != "demo" {
                    ids.push(model.clone());
                }
            }
            let models: Vec<_> = ids
                .into_iter()
                .map(|id| {
                    let loaded = state.model.as_deref() == Some(id.as_str());
                    json!({"id": id, "exists": true, "loaded": loaded})
                })
                .collect();
            ok(json!({"models": models}))
        }
        _ => unreachable!("expect_known covers all commands"),
    }
}
