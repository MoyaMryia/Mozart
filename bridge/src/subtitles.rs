//! 字幕记录表与 JSONL 存档（SPEC-05 字幕桥）：内存表管修订合并，文件是发布点与存档。
//! 记录格式沿用旧实现（subtitle_bridge.py source_caption）：zh/en/final/translation_status/
//! received_at/ts/seq 加可空审计字段；utterance_id 用启动号:句号。
//! revision 覆盖语义：创建记 0，每次更新 +1；final 之后不再被 partial/final 覆盖；
//! refined 只更新 zh 与 refinement_status，不重新触发翻译。
//! 记录表与写盘由 SubtitleStore 共享给字幕线程与翻译线程：
//! 译文合格即追加一行 completed 修订，先于播报投递落盘。

use crate::events::{Event, EventKind};
use crate::preprocess;
use crate::state::{unix_ms, Queues, Shared, TranslateJob};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

/// 内存记录表上限（旧 latest 128）；超限按最久未用淘汰，在途句受保护。
/// 旧 refined 64 是独立的复核等待表，新架构复核直接并入记录表，不另设。
const RECORD_CAP: usize = 128;

/// 降级原因：待翻译队列满（后续入队成功时解除）。
const DEGRADED_TRANSLATE_FULL: &str = "translate queue full";

/// 翻译状态（沿用旧 translation_status 取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranslationStatus {
    Pending,
    Recognizing,
    Completed,
    Failed,
    Skipped,
}

/// 一条字幕记录；序列化形态即 JSONL 行（旧 schema）。
/// 审计字段可空，缺失不落盘；en 初始为空，译文合格后填充。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleRecord {
    pub seq: u64,
    pub utterance_id: String,
    pub zh: String,
    pub en: String,
    pub asr_engine: String,
    pub received_at: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub asr_emitted_at: Option<f64>,
    pub online_text: String,
    pub final_decode_ms: u64,
    pub ts: String,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub revision: u64,
    pub translation_status: TranslationStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_audio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_audio_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_audio_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub audio_start_pts_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub audio_end_pts_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub asr_numeric_audit: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub asr_polarity_audit: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub refinement_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub translation_source_audit: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub homophone_fixes: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub zh_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub translation_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub translated_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub translate_ms: Option<u64>,
}

/// 带 LRU 淘汰的记录表：按最久未用淘汰，保护在途翻译句（pending/recognizing），
/// 确保译文修订必能命中记录。落盘存档不受淘汰影响。全是在途句时短暂超界。
pub struct RecordTable {
    entries: HashMap<String, (SubtitleRecord, u64)>,
    clock: u64,
}

impl RecordTable {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
        }
    }

    fn touch(&mut self, key: &str) {
        self.clock += 1;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.1 = self.clock;
        }
    }

    pub fn get(&mut self, key: &str) -> Option<&SubtitleRecord> {
        self.touch(key);
        self.entries.get(key).map(|(record, _)| record)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut SubtitleRecord> {
        self.touch(key);
        self.entries.get_mut(key).map(|(record, _)| record)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub fn insert(&mut self, key: String, record: SubtitleRecord) {
        self.clock += 1;
        let stamp = self.clock;
        self.entries.insert(key, (record, stamp));
        self.evict();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn evict(&mut self) {
        while self.entries.len() > RECORD_CAP {
            let victim = self
                .entries
                .iter()
                .filter(|(_, (record, _))| {
                    !matches!(
                        record.translation_status,
                        TranslationStatus::Pending | TranslationStatus::Recognizing
                    )
                })
                .min_by_key(|(_, (_, stamp))| *stamp)
                .map(|(key, _)| key.clone());
            match victim {
                Some(key) => {
                    self.entries.remove(&key);
                }
                // 全是在途句：不淘汰，保护译文修订命中。
                None => break,
            }
        }
    }
}

impl std::ops::Index<&str> for RecordTable {
    type Output = SubtitleRecord;
    fn index(&self, key: &str) -> &SubtitleRecord {
        &self.entries[key].0
    }
}

/// 本地时间 HH:MM:SS（旧 time.strftime('%H:%M:%S') 用本地时区）。
fn local_hms(unix_secs: f64) -> String {
    let secs = unix_secs as libc::time_t;
    // SAFETY: tm 由调用方提供且零初始化；localtime_r 只写入该 tm。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::localtime_r(&secs, &mut tm) };
    if result.is_null() {
        return "00:00:00".to_string();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// 句号缺省时从 utterance_id（启动号:句号）尾段解析。
fn seq_from_utterance(utterance_id: &str) -> u64 {
    utterance_id
        .rsplit_once(':')
        .and_then(|(_, seq)| seq.parse().ok())
        .unwrap_or(0)
}

/// 合并一条事件进记录表，返回应落盘的记录；None 表示忽略。
/// 事件字段已在 events::parse 入口校验，此处信任。zh 存纠错后原文。
pub fn apply(
    table: &mut RecordTable,
    event: &Event,
    now_ms: u64,
    corrections: &[(String, String)],
) -> Option<SubtitleRecord> {
    match event.kind {
        EventKind::Partial | EventKind::Final => {
            // 空文本不发布；final 之后不再接受 partial/final 覆盖（沿用旧实现）。
            if event.text.trim().is_empty() {
                return None;
            }
            let previous = table.get(&event.utterance_id);
            if previous.is_some_and(|record| record.is_final) {
                return None;
            }
            let raw_text = event
                .text_raw
                .as_deref()
                .unwrap_or(&event.text)
                .trim()
                .to_string();
            let (zh, fixes) = preprocess::correct(&raw_text, corrections);
            let is_final = event.kind == EventKind::Final;
            let received_at = now_ms as f64 / 1000.0;
            let mut record = SubtitleRecord {
                seq: event.seq.unwrap_or_else(|| seq_from_utterance(&event.utterance_id)),
                utterance_id: event.utterance_id.clone(),
                zh,
                en: String::new(),
                asr_engine: event
                    .final_engine
                    .clone()
                    .unwrap_or_else(|| "zipformer".to_string()),
                received_at,
                asr_emitted_at: event.emitted_at,
                online_text: event.online_text.clone().unwrap_or_else(|| raw_text.clone()),
                final_decode_ms: event.final_decode_ms.unwrap_or(0),
                ts: local_hms(received_at),
                is_final,
                revision: previous.map_or(0, |record| record.revision + 1),
                translation_status: if is_final {
                    TranslationStatus::Pending
                } else {
                    TranslationStatus::Recognizing
                },
                source_audio: event.source_audio.clone(),
                source_audio_sha256: event.source_audio_sha256.clone(),
                source_audio_seconds: event.source_audio_seconds,
                audio_start_pts_ns: event.audio_start_pts_ns,
                audio_end_pts_ns: event.audio_end_pts_ns,
                asr_numeric_audit: event.asr_numeric_audit.clone(),
                asr_polarity_audit: event.asr_polarity_audit.clone(),
                refinement_status: event.refinement_status.clone(),
                translation_source_audit: event.translation_source_audit.clone(),
                homophone_fixes: None,
                zh_raw: None,
                translation_error: None,
                translated_at: None,
                translate_ms: None,
            };
            if !fixes.is_empty() {
                record.zh_raw = Some(raw_text);
                record.homophone_fixes =
                    Some(serde_json::to_value(&fixes).expect("correction fixes are serializable"));
            }
            table.insert(event.utterance_id.clone(), record.clone());
            Some(record)
        }
        EventKind::Refined => {
            let record = table.get_mut(&event.utterance_id)?;
            // 复核文本非空才替换原文（ASR 仅在复核 completed 时携带 refined_text）。
            if let Some(refined) = event.refined_text.as_deref() {
                let trimmed = refined.trim();
                if !trimmed.is_empty() {
                    let (zh, fixes) = preprocess::correct(trimmed, corrections);
                    record.zh = zh;
                    if !fixes.is_empty() {
                        record.zh_raw = Some(trimmed.to_string());
                        record.homophone_fixes = Some(
                            serde_json::to_value(&fixes).expect("correction fixes are serializable"),
                        );
                    }
                }
            }
            if let Some(status) = &event.refinement_status {
                record.refinement_status = Some(status.clone());
            }
            record.revision += 1;
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
    records: Mutex<RecordTable>,
    writer: Mutex<JsonlWriter>,
    path: PathBuf,
    corrections: Vec<(String, String)>,
}

impl SubtitleStore {
    pub fn new(writer: JsonlWriter, path: PathBuf, corrections: Vec<(String, String)>) -> Self {
        Self {
            records: Mutex::new(RecordTable::new()),
            writer: Mutex::new(writer),
            path,
            corrections,
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
        let Some(record) = apply(&mut records, event, now, &self.corrections) else {
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
    /// 记录必然存在：final 先入表落盘才入待翻译队列，在途句受淘汰保护。
    pub fn publish_translation(
        &self,
        utterance_id: &str,
        translation: &str,
        translate_ms: u64,
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
        record.en = translation.to_string();
        record.translation_status = TranslationStatus::Completed;
        record.translation_error = None;
        record.translated_at = Some(now as f64 / 1000.0);
        record.translate_ms = Some(translate_ms);
        record.revision += 1;
        let snapshot = record.clone();
        self.write_locked(&snapshot)?;
        shared.set_last_write_unix_ms(now);
        Ok(())
    }

    /// 翻译失败：标 failed 并记原因，追加一行修订（原文保留）。
    pub fn publish_translation_failed(
        &self,
        utterance_id: &str,
        error: &str,
        shared: &Shared,
    ) -> Result<(), String> {
        self.publish_outcome(utterance_id, TranslationStatus::Failed, Some(error), shared)
    }

    /// 翻译跳过（关闭/队满/收尾）：标 skipped 并记原因。
    pub fn publish_translation_skipped(
        &self,
        utterance_id: &str,
        reason: &str,
        shared: &Shared,
    ) -> Result<(), String> {
        self.publish_outcome(utterance_id, TranslationStatus::Skipped, Some(reason), shared)
    }

    fn publish_outcome(
        &self,
        utterance_id: &str,
        status: TranslationStatus,
        error: Option<&str>,
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
        record.translation_status = status;
        record.translation_error = error.map(|e| e.to_string());
        record.revision += 1;
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
    if record.is_final && shared.translate_enabled() {
        let job = TranslateJob {
            utterance_id: record.utterance_id.clone(),
            text: record.zh.clone(),
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
            "seq": seq,
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
            "seq": seq,
            "text": "你好",
            "refinement_status": "completed",
        });
        if let Some(text) = refined_text {
            body["refined_text"] = json!(text);
        }
        parse(&serde_json::to_vec(&body).unwrap()).unwrap().unwrap()
    }

    fn no_corrections() -> Vec<(String, String)> {
        Vec::new()
    }

    #[test]
    fn partial_final_refined_merge_with_revisions() {
        let mut table = RecordTable::new();
        let first = apply(&mut table, &event(EventKind::Partial, 1, "你"), 100, &no_corrections()).unwrap();
        assert_eq!(first.revision, 0);
        assert!(!first.is_final);
        assert_eq!(first.translation_status, TranslationStatus::Recognizing);
        assert_eq!(first.seq, 1);
        assert_eq!(first.zh, "你");
        assert_eq!(first.en, "");

        let second = apply(&mut table, &event(EventKind::Partial, 1, "你好"), 200, &no_corrections()).unwrap();
        assert_eq!(second.revision, 1);
        assert_eq!(second.zh, "你好");

        let third = apply(&mut table, &event(EventKind::Final, 1, "你好"), 300, &no_corrections()).unwrap();
        assert_eq!(third.revision, 2);
        assert!(third.is_final);
        assert_eq!(third.translation_status, TranslationStatus::Pending);

        let fourth = apply(&mut table, &refined_event(1, Some("你好。")), 400, &no_corrections()).unwrap();
        assert_eq!(fourth.revision, 3);
        assert_eq!(fourth.zh, "你好。");
        assert!(fourth.is_final);
        assert_eq!(fourth.refinement_status.as_deref(), Some("completed"));
        assert_eq!(table.len(), 1);
        assert_eq!(table["a1b2c3d4:1"], fourth);
    }

    #[test]
    fn final_is_sticky_against_late_partial_and_final() {
        let mut table = RecordTable::new();
        apply(&mut table, &event(EventKind::Final, 1, "你好"), 100, &no_corrections());
        assert_eq!(apply(&mut table, &event(EventKind::Partial, 1, "晚到"), 200, &no_corrections()), None);
        assert_eq!(apply(&mut table, &event(EventKind::Final, 1, "重写"), 300, &no_corrections()), None);
        assert_eq!(table["a1b2c3d4:1"].zh, "你好");
        assert_eq!(table["a1b2c3d4:1"].revision, 0);
    }

    #[test]
    fn empty_text_is_not_published() {
        let mut table = RecordTable::new();
        assert_eq!(apply(&mut table, &event(EventKind::Partial, 1, ""), 100, &no_corrections()), None);
        assert_eq!(apply(&mut table, &event(EventKind::Final, 1, "  "), 200, &no_corrections()), None);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn final_without_partial_creates_record() {
        let mut table = RecordTable::new();
        let record = apply(&mut table, &event(EventKind::Final, 7, "整句"), 100, &no_corrections()).unwrap();
        assert_eq!(record.revision, 0);
        assert!(record.is_final);
        assert_eq!(record.seq, 7);
    }

    #[test]
    fn refined_without_record_is_ignored() {
        let mut table = RecordTable::new();
        assert_eq!(apply(&mut table, &refined_event(9, Some("无主")), 100, &no_corrections()), None);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn refined_without_text_keeps_original_and_bumps_revision() {
        let mut table = RecordTable::new();
        apply(&mut table, &event(EventKind::Final, 1, "你好"), 100, &no_corrections());
        // 复核 failed/skipped：ASR 不携带 refined_text。
        let record = apply(&mut table, &refined_event(1, None), 200, &no_corrections()).unwrap();
        assert_eq!(record.zh, "你好");
        assert_eq!(record.revision, 1);
        // 空白复核文本同样不替换。
        let record = apply(&mut table, &refined_event(1, Some("  ")), 300, &no_corrections()).unwrap();
        assert_eq!(record.zh, "你好");
        assert_eq!(record.revision, 2);
    }

    #[test]
    fn distinct_utterances_keep_separate_revisions() {
        let mut table = RecordTable::new();
        apply(&mut table, &event(EventKind::Partial, 1, "甲"), 100, &no_corrections());
        let second = apply(&mut table, &event(EventKind::Partial, 2, "乙"), 200, &no_corrections()).unwrap();
        assert_eq!(second.revision, 0);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn correction_applies_to_zh_and_records_fixes() {
        let mut table = RecordTable::new();
        let corrections = preprocess::builtin_corrections();
        let record = apply(&mut table, &event(EventKind::Final, 1, "我以经说了"), 100, &corrections).unwrap();
        assert_eq!(record.zh, "我已经说了");
        assert_eq!(record.zh_raw.as_deref(), Some("我以经说了"));
        let fixes = record.homophone_fixes.unwrap();
        assert_eq!(fixes[0]["from"], "以经");
        assert_eq!(fixes[0]["to"], "已经");
        assert_eq!(fixes[0]["count"], 1);
    }

    #[test]
    fn schema_fields_match_old_source_caption() {
        let mut table = RecordTable::new();
        let bytes = serde_json::to_vec(&json!({
            "type": "final",
            "startup_id": "a1b2c3d4",
            "utterance_id": "a1b2c3d4:3",
            "seq": 3,
            "text": "你好",
            "emitted_at": 1731234567.89,
            "online_text": "你好",
            "final_engine": "zipformer",
            "final_decode_ms": 12,
            "audio_start_pts_ns": 100,
            "audio_end_pts_ns": 200,
        }))
        .unwrap();
        let event = parse(&bytes).unwrap().unwrap();
        let record = apply(&mut table, &event, 1731234567890, &no_corrections()).unwrap();
        let value: Value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["seq"], 3);
        assert_eq!(value["utterance_id"], "a1b2c3d4:3");
        assert_eq!(value["zh"], "你好");
        assert_eq!(value["en"], "");
        assert_eq!(value["asr_engine"], "zipformer");
        assert_eq!(value["received_at"], 1731234567.89);
        assert_eq!(value["asr_emitted_at"], 1731234567.89);
        assert_eq!(value["online_text"], "你好");
        assert_eq!(value["final_decode_ms"], 12);
        assert_eq!(value["final"], true);
        assert_eq!(value["revision"], 0);
        assert_eq!(value["translation_status"], "pending");
        assert_eq!(value["audio_start_pts_ns"], 100);
        assert_eq!(value["audio_end_pts_ns"], 200);
        // ts 为 HH:MM:SS。
        let ts = value["ts"].as_str().unwrap();
        assert_eq!(ts.len(), 8);
        assert_eq!(ts.as_bytes()[2], b':');
        assert_eq!(ts.as_bytes()[5], b':');
        // 未携带的审计字段不落盘。
        assert!(value.get("asr_numeric_audit").is_none());
        assert!(value.get("zh_raw").is_none());
        assert!(value.get("homophone_fixes").is_none());
        assert!(value.get("translation_error").is_none());
        // 落盘行可反序列化回记录。
        let back: SubtitleRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, record);
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
        let mut table = RecordTable::new();
        let record = apply(&mut table, &event(EventKind::Partial, 1, "你好"), 1, &no_corrections()).unwrap();
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
            assert_eq!(back.utterance_id, "a1b2c3d4:1");
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
        let store = SubtitleStore::new(JsonlWriter::open(&path).unwrap(), path.clone(), no_corrections());

        let record = store
            .apply_event(&event(EventKind::Final, 1, "你好"), &shared)
            .unwrap()
            .unwrap();
        assert_eq!(record.revision, 0);
        assert_eq!(shared.status(&Queues::new(1, 1)).subtitles_published, 1);

        store.publish_translation("a1b2c3d4:1", "Hello.", 5, &shared).unwrap();
        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        // 原文行 en 为空、pending；译文行保留原文、completed、revision +1。
        assert_eq!(lines[0]["en"], "");
        assert_eq!(lines[0]["translation_status"], "pending");
        assert_eq!(lines[1]["zh"], "你好");
        assert_eq!(lines[1]["en"], "Hello.");
        assert_eq!(lines[1]["translation_status"], "completed");
        assert_eq!(lines[1]["translate_ms"], 5);
        assert!(lines[1]["translated_at"].as_f64().unwrap() > 0.0);
        assert_eq!(lines[1]["revision"], 1);
        assert_eq!(lines[1]["final"], true);
        // 译文修订不新增已发布句数。
        let status = shared.status(&Queues::new(1, 1));
        assert_eq!(status.subtitles_published, 1);
        assert_eq!(status.records_in_memory, 1);
        assert!(status.last_write_unix_ms > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_publishes_failed_and_skipped_outcomes() {
        use crate::state::{Shared, Switches, STATE_FILE_NAME};

        let dir = std::env::temp_dir().join(format!(
            "mozart-bridge-outcome-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("subtitles.jsonl");
        let shared = Shared::new(
            Switches { translate_enabled: true, speech_enabled: true },
            path.clone(),
            dir.join(STATE_FILE_NAME),
        );
        let store = SubtitleStore::new(JsonlWriter::open(&path).unwrap(), path.clone(), no_corrections());
        store.apply_event(&event(EventKind::Final, 1, "你好"), &shared).unwrap();
        store.publish_translation_failed("a1b2c3d4:1", "Translation rejected: x", &shared).unwrap();
        store.apply_event(&event(EventKind::Final, 2, "世界"), &shared).unwrap();
        store.publish_translation_skipped("a1b2c3d4:2", "translate off", &shared).unwrap();

        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[1]["translation_status"], "failed");
        assert_eq!(lines[1]["translation_error"], "Translation rejected: x");
        assert_eq!(lines[1]["en"], "");
        assert_eq!(lines[1]["revision"], 1);
        assert_eq!(lines[3]["translation_status"], "skipped");
        assert_eq!(lines[3]["translation_error"], "translate off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lru_caps_table_and_protects_in_flight() {
        let mut table = RecordTable::new();
        // 一条在途句（pending）先入表。
        apply(&mut table, &event(EventKind::Final, 0, "在途"), 1, &no_corrections());
        // 灌入超过上限的已完成句（可淘汰）。
        for seq in 1..=200u64 {
            apply(&mut table, &event(EventKind::Final, seq, "句"), seq, &no_corrections());
            table
                .get_mut(&format!("a1b2c3d4:{seq}"))
                .unwrap()
                .translation_status = TranslationStatus::Completed;
        }
        // 表受上限约束，最旧已完成句被淘汰，在途句受保护。
        assert_eq!(table.len(), RECORD_CAP);
        assert!(!table.contains_key("a1b2c3d4:1"));
        assert!(table.contains_key("a1b2c3d4:0"));
        assert_eq!(table["a1b2c3d4:0"].translation_status, TranslationStatus::Pending);
    }

    #[test]
    fn lru_keeps_all_when_every_record_is_in_flight() {
        let mut table = RecordTable::new();
        // 全是在途句（pending）：不淘汰，可短暂超界。
        for seq in 1..=150u64 {
            apply(&mut table, &event(EventKind::Final, seq, "句"), seq, &no_corrections());
        }
        assert_eq!(table.len(), 150);
        assert!(table.contains_key("a1b2c3d4:1"));
    }
}