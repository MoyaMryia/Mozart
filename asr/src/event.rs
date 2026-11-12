//! 识别事件 JSON 契约（SPEC-05）：partial/final/refined 三种类型，字段沿用旧实现，
//! 新增 startup_id 与 utterance_id（启动号:句号）。一条 SEQPACKET 消息一个 JSON 事件。
//! 同音字纠错归字幕桥（SPEC-05 翻译节），识别服务 text 即原始增量，homophone_fixes 恒空。

use crate::state::unix_ms;
use serde::Serialize;
use serde_json::Value;

pub const PARTIAL: &str = "partial";
pub const FINAL: &str = "final";
pub const REFINED: &str = "refined";

/// 复核状态取值（沿用旧 refinement_status 语义）。
pub const REVIEW_PENDING: &str = "pending";
pub const REVIEW_DISABLED: &str = "disabled";
pub const REVIEW_COMPLETED: &str = "completed";
pub const REVIEW_FAILED: &str = "failed";
pub const REVIEW_SKIPPED: &str = "skipped";

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub startup_id: String,
    pub utterance_id: String,
    pub seq: u64,
    pub text: String,
    /// 事件发出时刻（Unix 秒，沿用旧 emitted_at）。
    pub emitted_at: f64,
    pub audio_start_pts_ns: Option<u64>,
    pub audio_end_pts_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub online_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homophone_fixes: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_decode_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refinement_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refined_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refinement_error: Option<String>,
}

/// utterance_id = 启动号:句号（SPEC-05 字幕桥节）。
pub fn utterance_id(startup_id: &str, seq: u64) -> String {
    format!("{startup_id}:{seq}")
}

fn now_secs() -> f64 {
    unix_ms() as f64 / 1000.0
}

impl Event {
    /// 中间增量事件。seq 为即将到来的 final 句号。
    pub fn partial(
        startup_id: &str,
        seq: u64,
        text: String,
        start_pts: Option<u64>,
        end_pts: Option<u64>,
    ) -> Self {
        Self {
            kind: PARTIAL,
            startup_id: startup_id.to_string(),
            utterance_id: utterance_id(startup_id, seq),
            seq,
            text,
            emitted_at: now_secs(),
            audio_start_pts_ns: start_pts,
            audio_end_pts_ns: end_pts,
            text_raw: None,
            online_text: None,
            homophone_fixes: None,
            final_engine: None,
            final_decode_ms: None,
            refinement_status: None,
            refined_text: None,
            refinement_error: None,
        }
    }

    /// 整句 final 事件。review_enabled 决定 refinement_status 为 pending 或 disabled。
    pub fn final_utterance(
        startup_id: &str,
        seq: u64,
        text: String,
        start_pts: Option<u64>,
        end_pts: Option<u64>,
        review_enabled: bool,
    ) -> Self {
        Self {
            kind: FINAL,
            startup_id: startup_id.to_string(),
            utterance_id: utterance_id(startup_id, seq),
            seq,
            text: text.clone(),
            emitted_at: now_secs(),
            audio_start_pts_ns: start_pts,
            audio_end_pts_ns: end_pts,
            text_raw: Some(text.clone()),
            online_text: Some(text),
            homophone_fixes: Some(Vec::new()),
            final_engine: Some("zipformer".to_string()),
            final_decode_ms: Some(0),
            refinement_status: Some(
                if review_enabled {
                    REVIEW_PENDING
                } else {
                    REVIEW_DISABLED
                }
                .to_string(),
            ),
            refined_text: None,
            refinement_error: None,
        }
    }

    /// 由 final 事件派生 refined 事件：保留原字段，覆盖类型与复核结果。
    pub fn into_refined(
        mut self,
        status: &str,
        refined_text: Option<String>,
        error: Option<String>,
        decode_ms: u64,
    ) -> Self {
        self.kind = REFINED;
        self.emitted_at = now_secs();
        self.refinement_status = Some(status.to_string());
        self.refined_text = refined_text;
        self.refinement_error = error;
        self.final_decode_ms = Some(decode_ms);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse(event: &Event) -> Value {
        serde_json::to_value(event).unwrap()
    }

    #[test]
    fn utterance_id_joins_startup_and_seq() {
        assert_eq!(utterance_id("a1b2c3d4", 1), "a1b2c3d4:1");
        assert_eq!(utterance_id("a1b2c3d4", 42), "a1b2c3d4:42");
    }

    #[test]
    fn partial_event_fields() {
        let event = Event::partial("abcd1234", 3, "你好".to_string(), Some(100), Some(200));
        let json = parse(&event);
        assert_eq!(json["type"], "partial");
        assert_eq!(json["startup_id"], "abcd1234");
        assert_eq!(json["utterance_id"], "abcd1234:3");
        assert_eq!(json["seq"], 3);
        assert_eq!(json["text"], "你好");
        assert_eq!(json["audio_start_pts_ns"], 100);
        assert_eq!(json["audio_end_pts_ns"], 200);
        assert!(json["emitted_at"].as_f64().unwrap() > 0.0);
        // partial 不携带 final 专属字段。
        assert!(json.get("text_raw").is_none());
        assert!(json.get("refinement_status").is_none());
    }

    #[test]
    fn final_event_fields_pending_and_disabled() {
        let pending = Event::final_utterance("abcd1234", 1, "你好".to_string(), Some(1), Some(2), true);
        let json = parse(&pending);
        assert_eq!(json["type"], "final");
        assert_eq!(json["utterance_id"], "abcd1234:1");
        assert_eq!(json["text"], "你好");
        assert_eq!(json["text_raw"], "你好");
        assert_eq!(json["online_text"], "你好");
        assert_eq!(json["final_engine"], "zipformer");
        assert_eq!(json["final_decode_ms"], 0);
        assert_eq!(json["refinement_status"], "pending");
        assert_eq!(json["homophone_fixes"], serde_json::json!([]));

        let disabled = Event::final_utterance("abcd1234", 1, "你好".to_string(), None, None, false);
        assert_eq!(parse(&disabled)["refinement_status"], "disabled");
    }

    #[test]
    fn refined_event_keeps_identity_and_overrides_status() {
        let final_event = Event::final_utterance("abcd1234", 7, "你好".to_string(), Some(1), Some(2), true);
        let refined = final_event.into_refined(
            REVIEW_COMPLETED,
            Some("你好。".to_string()),
            None,
            12,
        );
        let json = parse(&refined);
        assert_eq!(json["type"], "refined");
        assert_eq!(json["startup_id"], "abcd1234");
        assert_eq!(json["utterance_id"], "abcd1234:7");
        assert_eq!(json["seq"], 7);
        // 原文保留，复核文本另列。
        assert_eq!(json["text"], "你好");
        assert_eq!(json["refined_text"], "你好。");
        assert_eq!(json["refinement_status"], "completed");
        assert_eq!(json["final_decode_ms"], 12);
        assert!(json.get("refinement_error").is_none());

        let failed = Event::final_utterance("abcd1234", 8, "x".to_string(), None, None, true)
            .into_refined(REVIEW_FAILED, None, Some("模型未加载".to_string()), 0);
        let json = parse(&failed);
        assert_eq!(json["refinement_status"], "failed");
        assert_eq!(json["refinement_error"], "模型未加载");
        assert!(json.get("refined_text").is_none());
    }
}