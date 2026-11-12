//! 字幕记录表与 JSONL 存档（SPEC-05 字幕桥）：内存表管修订合并，文件是发布点与存档。
//! 一条记录一行 JSON，逐行 flush。revision 覆盖语义沿用旧实现：
//! 创建记 0，每次更新 +1；final 之后不再被 partial/final 覆盖；
//! refined 只更新文本，不重新触发翻译。
//! 记录表与写盘由 SubtitleStore 共享给字幕线程与翻译线程：
//! 译文合格即追加一行带 translation 的修订记录，先于播报投递落盘。

use crate::events::{Event, EventKind};
use crate::state::{unix_ms, Queues, Shared, TranslateJob};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 记录表轮询步长：让停止标志在宽限期内生效。
const RECV_POLL: Duration = Duration::from_millis(100);

/// 降级原因：待翻译队列满（后续入队成功时解除）。
const DEGRADED_TRANSLATE_FULL: &str = "translate queue full";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordType {
    Partial,
    Final,
    Refined,
}

/// 一条字幕记录；序列化形态即 JSONL 行。complete 是内存合并标记，不落盘。
/// translation 仅在译文合格后写入（沿用旧实现：原文保留，译文另字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleRecord {
    pub utterance_id: String,
    pub text: String,
    #[serde(rename = "type")]
    pub kind: RecordType,
    pub revision: u64,
    pub timestamp_ms: u64,
    pub startup_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub translation: Option<String>,
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
                translation: None,
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

/// 记录表 + 存档文件的共享门面：字幕线程合并事件，翻译线程写入译文修订。
/// 锁序固定为先记录表后文件，两个线程一致；写盘失败由调用方致命退出。
pub struct SubtitleStore {
    records: Mutex<HashMap<String, SubtitleRecord>>,
    writer: Mutex<JsonlWriter>,
    path: PathBuf,
}

impl SubtitleStore {
    pub fn new(writer: JsonlWriter, path: PathBuf) -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            writer: Mutex::new(writer),
            path,
        }
    }

    /// 合并一条事件并落盘，返回应入待翻译队列的记录；None 表示忽略。
    pub fn apply_event(&self, event: &Event, shared: &Shared) -> Result<Option<SubtitleRecord>, String> {
        let now = unix_ms();
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let created = !records.contains_key(&event.utterance_id);
        let Some(record) = apply(&mut records, event, now) else {
            return Ok(None);
        };
        self.write_locked(&record)?;
        if created {
            shared.add_published();
        }
        shared.set_records_in_memory(records.len() as u32);
        shared.set_last_write_unix_ms(now);
        Ok(Some(record))
    }

    /// 译文合格：更新记录并追加一行修订（先于播报投递落盘，SPEC-05）。
    /// 记录必然存在：final 先入表落盘才入待翻译队列，表不淘汰。
    pub fn publish_translation(
        &self,
        utterance_id: &str,
        translation: &str,
        shared: &Shared,
    ) -> Result<(), String> {
        let now = unix_ms();
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = records
            .get_mut(utterance_id)
            .expect("translate job enqueued after record insert");
        record.translation = Some(translation.to_string());
        record.revision += 1;
        record.timestamp_ms = now;
        let snapshot = record.clone();
        self.write_locked(&snapshot)?;
        shared.set_last_write_unix_ms(now);
        Ok(())
    }

    /// 调用方已持记录表锁；文件锁内只做写盘。
    fn write_locked(&self, record: &SubtitleRecord) -> Result<(), String> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        writer
            .write(record)
            .map_err(|err| format!("写字幕 {}：{err}", self.path.display()))
    }
}

/// 字幕线程：消费事件队列，合并记录表、逐行落盘、final 入待翻译队列。
/// 停止标志置位后不再处理新事件；存档已逐行 flush，不受收尾影响。
pub fn run(
    rx: Receiver<Event>,
    store: Arc<SubtitleStore>,
    queues: Arc<Queues>,
    shared: Arc<Shared>,
    stop: &'static AtomicBool,
) -> Result<(), String> {
    while !stop.load(Relaxed) {
        let event = match rx.recv_timeout(RECV_POLL) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            // 事件线程已退出。
            Err(RecvTimeoutError::Disconnected) => break,
        };
        process(event, &store, &queues, &shared)?;
        while let Ok(event) = rx.try_recv() {
            process(event, &store, &queues, &shared)?;
        }
    }
    Ok(())
}

fn process(
    event: Event,
    store: &SubtitleStore,
    queues: &Queues,
    shared: &Shared,
) -> Result<(), String> {
    let Some(record) = store.apply_event(&event, shared)? else {
        return Ok(());
    };
    // final 入待翻译队列；translate off 只发布原文，不入队。
    if record.kind == RecordType::Final && shared.translate_enabled() {
        let job = TranslateJob {
            utterance_id: record.utterance_id.clone(),
            text: record.text.clone(),
        };
        if !queues.push_translate(job) {
            shared.degrade(DEGRADED_TRANSLATE_FULL);
        } else {
            shared.resolve(DEGRADED_TRANSLATE_FULL);
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
            translation: None,
            complete: true,
        };
        let line = serde_json::to_string(&record).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        // 未翻译记录落盘形态只含契约六字段。
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
        assert_eq!(back.translation, None);

        // 译文合格后多一个 translation 字段，原文保留。
        let mut translated = record.clone();
        translated.translation = Some("Hello.".to_string());
        translated.revision += 1;
        let value: Value =
            serde_json::from_str(&serde_json::to_string(&translated).unwrap()).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 7);
        assert_eq!(value["text"], "你好");
        assert_eq!(value["translation"], "Hello.");
        assert_eq!(value["revision"], 3);
        let back: SubtitleRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back.translation.as_deref(), Some("Hello."));
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
            translation: None,
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

    #[test]
    fn store_publishes_translation_as_revision() {
        use crate::state::{Shared, Switches, STATE_FILE_NAME};

        let dir = std::env::temp_dir().join(format!(
            "mozart-bridge-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("subtitles.jsonl");
        let shared = Shared::new(
            Switches { translate_enabled: true, speech_enabled: true },
            path.clone(),
            dir.join(STATE_FILE_NAME),
        );
        let store = SubtitleStore::new(JsonlWriter::open(&path).unwrap(), path.clone());

        let record = store
            .apply_event(&event(EventKind::Final, 1, "你好"), &shared)
            .unwrap()
            .unwrap();
        assert_eq!(record.revision, 0);
        assert_eq!(shared.status(&Queues::new(1, 1)).subtitles_published, 1);

        store.publish_translation("a1b2c3d4:1", "Hello.", &shared).unwrap();
        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        // 原文行不带 translation；译文行保留原文、revision +1。
        assert!(lines[0].get("translation").is_none());
        assert_eq!(lines[1]["text"], "你好");
        assert_eq!(lines[1]["translation"], "Hello.");
        assert_eq!(lines[1]["revision"], 1);
        assert_eq!(lines[1]["type"], "final");
        // 译文修订不新增已发布句数。
        let status = shared.status(&Queues::new(1, 1));
        assert_eq!(status.subtitles_published, 1);
        assert_eq!(status.records_in_memory, 1);
        assert!(status.last_write_unix_ms > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}