//! 字幕记录表与 JSONL 存档（SPEC-05 字幕桥）：内存表管修订合并，文件是发布点与存档。
//! 一条记录一行 JSON，逐行 flush。revision 覆盖语义沿用旧实现：
//! 创建记 0，每次更新 +1；final 之后不再被 partial/final 覆盖；
//! refined 只更新文本，不重新触发翻译。

use crate::events::{Event, EventKind};
use crate::state::{unix_ms, Queues, Shared, TranslateJob};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

/// 记录表轮询步长：让停止标志在宽限期内生效。
const RECV_POLL: Duration = Duration::from_millis(100);

/// 降级原因：待翻译队列满（Task 5 消费后按需解除）。
const DEGRADED_TRANSLATE_FULL: &str = "translate queue full";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordType {
    Partial,
    Final,
    Refined,
}

/// 一条字幕记录；序列化形态即 JSONL 行。complete 是内存合并标记，不落盘。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleRecord {
    pub utterance_id: String,
    pub text: String,
    #[serde(rename = "type")]
    pub kind: RecordType,
    pub revision: u64,
    pub timestamp_ms: u64,
    pub startup_id: String,
    #[serde(skip)]
    pub complete: bool,
}

/// 合并一条事件进记录表，返回应落盘的记录；None 表示忽略。
/// 事件字段已在 events::parse 入口校验，此处信任。
pub fn apply(
    table: &mut HashMap<String, SubtitleRecord>,
    event: &Event,
    now_ms: u64,
) -> Option<SubtitleRecord> {
    match event.kind {
        EventKind::Partial | EventKind::Final => {
            // 空文本不发布；final 之后不再接受 partial/final 覆盖（沿用旧实现）。
            if event.text.trim().is_empty() {
                return None;
            }
            let previous = table.get(&event.utterance_id);
            if previous.is_some_and(|record| record.complete) {
                return None;
            }
            let kind = if event.kind == EventKind::Final {
                RecordType::Final
            } else {
                RecordType::Partial
            };
            let record = SubtitleRecord {
                utterance_id: event.utterance_id.clone(),
                text: event.text.clone(),
                kind,
                revision: previous.map_or(0, |record| record.revision + 1),
                timestamp_ms: now_ms,
                startup_id: event.startup_id.clone(),
                complete: kind == RecordType::Final,
            };
            table.insert(event.utterance_id.clone(), record.clone());
            Some(record)
        }
        EventKind::Refined => {
            let record = table.get_mut(&event.utterance_id)?;
            // 复核文本非空才替换原文（ASR 仅在复核 completed 时携带 refined_text）。
            if let Some(refined) = event.refined_text.as_deref() {
                if !refined.trim().is_empty() {
                    record.text = refined.to_string();
                }
            }
            record.kind = RecordType::Refined;
            record.revision += 1;
            record.timestamp_ms = now_ms;
            Some(record.clone())
        }
    }
}

/// JSONL 追加写：逐行 flush，进程存活期间保持打开。
pub struct JsonlWriter {
    file: File,
}

impl JsonlWriter {
    /// 打开（缺失则创建）并补齐父目录；失败由调用方致命退出。
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    pub fn write(&mut self, record: &SubtitleRecord) -> io::Result<()> {
        let mut line = serde_json::to_vec(record).expect("subtitle record is JSON-serializable");
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()
    }
}

/// 字幕线程：消费事件队列，合并记录表、逐行落盘、final 入待翻译队列。
/// 停止标志置位后不再处理新事件；存档已逐行 flush，不受收尾影响。
pub fn run(
    rx: Receiver<Event>,
    mut writer: JsonlWriter,
    queues: Arc<Queues>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    let mut table: HashMap<String, SubtitleRecord> = HashMap::new();
    while !stop.load(Relaxed) {
        let event = match rx.recv_timeout(RECV_POLL) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            // 事件线程已退出。
            Err(RecvTimeoutError::Disconnected) => break,
        };
        process(event, &mut table, &mut writer, &queues, &shared)?;
        while let Ok(event) = rx.try_recv() {
            process(event, &mut table, &mut writer, &queues, &shared)?;
        }
    }
    Ok(())
}

fn process(
    event: Event,
    table: &mut HashMap<String, SubtitleRecord>,
    writer: &mut JsonlWriter,
    queues: &Queues,
    shared: &Shared,
) -> Result<(), String> {
    let created = !table.contains_key(&event.utterance_id);
    let now = unix_ms();
    let Some(record) = apply(table, &event, now) else {
        return Ok(());
    };
    writer
        .write(&record)
        .map_err(|err| format!("写字幕 {}：{err}", shared.subtitles_path.display()))?;
    if created {
        shared.add_published();
    }
    shared.set_records_in_memory(table.len() as u32);
    shared.set_last_write_unix_ms(now);
    // final 入待翻译队列（Task 5 消费）；translate off 只发布原文，不入队。
    if record.kind == RecordType::Final && shared.translate_enabled() {
        let job = TranslateJob {
            utterance_id: record.utterance_id.clone(),
            text: record.text.clone(),
        };
        if !queues.push_translate(job) {
            shared.degrade(DEGRADED_TRANSLATE_FULL);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{parse, EventKind};
    use serde_json::{json, Value};

    fn event(kind: EventKind, seq: u64, text: &str) -> Event {
        let kind_str = match kind {
            EventKind::Partial => "partial",
            EventKind::Final => "final",
            EventKind::Refined => "refined",
        };
        let bytes = serde_json::to_vec(&json!({
            "type": kind_str,
            "startup_id": "a1b2c3d4",
            "utterance_id": format!("a1b2c3d4:{seq}"),
            "text": text,
        }))
        .unwrap();
        parse(&bytes).unwrap().unwrap()
    }

    fn refined_event(seq: u64, refined_text: Option<&str>) -> Event {
        let mut body = json!({
            "type": "refined",
            "startup_id": "a1b2c3d4",
            "utterance_id": format!("a1b2c3d4:{seq}"),
            "text": "你好",
        });
        if let Some(text) = refined_text {
            body["refined_text"] = json!(text);
        }
        parse(&serde_json::to_vec(&body).unwrap()).unwrap().unwrap()
    }

    #[test]
    fn partial_final_refined_merge_with_revisions() {
        let mut table = HashMap::new();
        let first = apply(&mut table, &event(EventKind::Partial, 1, "你"), 100).unwrap();
        assert_eq!(first.revision, 0);
        assert_eq!(first.kind, RecordType::Partial);
        assert!(!first.complete);

        let second = apply(&mut table, &event(EventKind::Partial, 1, "你好"), 200).unwrap();
        assert_eq!(second.revision, 1);
        assert_eq!(second.text, "你好");

        let third = apply(&mut table, &event(EventKind::Final, 1, "你好"), 300).unwrap();
        assert_eq!(third.revision, 2);
        assert_eq!(third.kind, RecordType::Final);
        assert!(third.complete);

        let fourth = apply(&mut table, &refined_event(1, Some("你好。")), 400).unwrap();
        assert_eq!(fourth.revision, 3);
        assert_eq!(fourth.kind, RecordType::Refined);
        assert_eq!(fourth.text, "你好。");
        assert!(fourth.complete);
        assert_eq!(table.len(), 1);
        assert_eq!(table["a1b2c3d4:1"], fourth);
    }

    #[test]
    fn final_is_sticky_against_late_partial_and_final() {
        let mut table = HashMap::new();
        apply(&mut table, &event(EventKind::Final, 1, "你好"), 100);
        assert_eq!(apply(&mut table, &event(EventKind::Partial, 1, "晚到"), 200), None);
        assert_eq!(apply(&mut table, &event(EventKind::Final, 1, "重写"), 300), None);
        assert_eq!(table["a1b2c3d4:1"].text, "你好");
        assert_eq!(table["a1b2c3d4:1"].revision, 0);
    }

    #[test]
    fn empty_text_is_not_published() {
        let mut table = HashMap::new();
        assert_eq!(apply(&mut table, &event(EventKind::Partial, 1, ""), 100), None);
        assert_eq!(apply(&mut table, &event(EventKind::Final, 1, "  "), 200), None);
        assert!(table.is_empty());
    }

    #[test]
    fn final_without_partial_creates_record() {
        let mut table = HashMap::new();
        let record = apply(&mut table, &event(EventKind::Final, 7, "整句"), 100).unwrap();
        assert_eq!(record.revision, 0);
        assert!(record.complete);
    }

    #[test]
    fn refined_without_record_is_ignored() {
        let mut table = HashMap::new();
        assert_eq!(apply(&mut table, &refined_event(9, Some("无主")), 100), None);
        assert!(table.is_empty());
    }

    #[test]
    fn refined_without_text_keeps_original_and_bumps_revision() {
        let mut table = HashMap::new();
        apply(&mut table, &event(EventKind::Final, 1, "你好"), 100);
        // 复核 failed/skipped：ASR 不携带 refined_text。
        let record = apply(&mut table, &refined_event(1, None), 200).unwrap();
        assert_eq!(record.text, "你好");
        assert_eq!(record.revision, 1);
        assert_eq!(record.kind, RecordType::Refined);
        // 空白复核文本同样不替换。
        let record = apply(&mut table, &refined_event(1, Some("  ")), 300).unwrap();
        assert_eq!(record.text, "你好");
        assert_eq!(record.revision, 2);
    }

    #[test]
    fn distinct_utterances_keep_separate_revisions() {
        let mut table = HashMap::new();
        apply(&mut table, &event(EventKind::Partial, 1, "甲"), 100);
        let second = apply(&mut table, &event(EventKind::Partial, 2, "乙"), 200).unwrap();
        assert_eq!(second.revision, 0);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn jsonl_line_roundtrip() {
        let record = SubtitleRecord {
            utterance_id: "a1b2c3d4:1".to_string(),
            text: "你好".to_string(),
            kind: RecordType::Final,
            revision: 2,
            timestamp_ms: 1731234567890,
            startup_id: "a1b2c3d4".to_string(),
            complete: true,
        };
        let line = serde_json::to_string(&record).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        // 落盘形态只含契约六字段。
        assert_eq!(value.as_object().unwrap().len(), 6);
        assert_eq!(value["type"], "final");
        assert_eq!(value["revision"], 2);
        assert_eq!(value["timestamp_ms"], 1731234567890u64);
        let back: SubtitleRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.utterance_id, record.utterance_id);
        assert_eq!(back.text, record.text);
        assert_eq!(back.kind, record.kind);
        assert_eq!(back.revision, record.revision);
        assert_eq!(back.timestamp_ms, record.timestamp_ms);
        assert_eq!(back.startup_id, record.startup_id);
    }

    #[test]
    fn writer_appends_and_flushes_each_line() {
        let dir = std::env::temp_dir().join(format!(
            "mozart-bridge-jsonl-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("nested/subtitles.jsonl");
        let mut writer = JsonlWriter::open(&path).unwrap();
        let record = SubtitleRecord {
            utterance_id: "s:1".to_string(),
            text: "你好".to_string(),
            kind: RecordType::Partial,
            revision: 0,
            timestamp_ms: 1,
            startup_id: "s".to_string(),
            complete: false,
        };
        writer.write(&record).unwrap();
        // 不关闭文件即可读到已 flush 的行。
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 1);
        writer.write(&record).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.ends_with('\n'));
        for line in content.lines() {
            let back: SubtitleRecord = serde_json::from_str(line).unwrap();
            assert_eq!(back.utterance_id, "s:1");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}