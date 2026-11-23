//! 字幕记录表与 JSONL 存档（SPEC-05 字幕桥）：内存表管修订合并，文件是发布点与存档。
//! 记录格式沿用旧实现（subtitle_bridge.py source_caption）：zh/en/final/translation_status/
//! received_at/ts/seq 加可空审计字段；utterance_id 用启动号:句号。
//! revision 覆盖语义：创建记 0，每次更新 +1；final 之后不再被 partial/final 覆盖；
//! 复核开启（refinement_status = pending）时等 refined 到达后再触发单次翻译与播报；
//! 复核未开启时则在 final 直接触发一次。
//! 记录表与写盘由 SubtitleStore 共享给字幕线程与翻译线程：
//! 译文合格即追加一行 completed 修订，先于播报投递落盘。

use crate::events::{Event, EventKind};
use crate::preprocess;
use crate::state::{unix_ms, Queues, Shared, SpeechGeneration, TranslateJob};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 记录表轮询步长：让停止标志在宽限期内生效。
const RECV_POLL: Duration = Duration::from_millis(100);
const REFINEMENT_DEADLINE: Duration = Duration::from_secs(30);
const RECOVERY_TAIL_BYTES: u64 = 1024 * 1024;
const REVIEW_TIMEOUT: &str = "refinement timeout";
const REVIEW_INTERRUPTED: &str = "refinement interrupted by bridge restart";
const DEGRADED_RECOVERY: &str = "subtitle recovery skipped invalid journal records";

/// 内存记录表上限（旧 latest 128）；超限按最久未用淘汰，在途句受保护。
/// 旧 refined 64 是独立的复核等待表，新架构复核直接并入记录表，不另设。
const RECORD_CAP: usize = 128;

/// 降级原因：待翻译队列满（后续入队成功时解除）。
const DEGRADED_TRANSLATE_FULL: &str = "translate queue full";

/// ASR 复核待完成状态；复核终态为 completed / failed / skipped。
const REFINEMENT_PENDING: &str = "pending";

fn is_terminal_refinement(status: Option<&str>) -> bool {
    matches!(status, Some("completed" | "failed" | "skipped"))
}

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
    #[serde(skip)]
    speech_generation: Option<SpeechGeneration>,
    #[serde(skip)]
    review_deadline: Option<Instant>,
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
    pub refinement_error: Option<String>,
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

    /// 只读查看记录，不刷新 LRU 访问序。
    fn peek(&self, key: &str) -> Option<&SubtitleRecord> {
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

/// 对原文执行同音字与术语纠错，返回纠错后文本及仅在命中时附带的审计字段。
fn apply_source_corrections(
    raw: &str,
    corrections: &[(String, String)],
) -> (String, Option<String>, Option<Value>) {
    let (zh, fixes) =
        preprocess::correct_with_terms(raw, &preprocess::builtin_correction_terms(), corrections);
    if fixes.is_empty() {
        (zh, None, None)
    } else {
        (
            zh,
            Some(raw.to_string()),
            Some(serde_json::to_value(&fixes).expect("correction fixes are serializable")),
        )
    }
}

/// 合并一条事件进记录表，返回应落盘的记录；None 表示忽略。
/// 事件字段已在 events::parse 入口校验，此处信任。zh 存纠错后原文。
pub fn apply(
    table: &mut RecordTable,
    event: &Event,
    now_ms: u64,
    corrections: &[(String, String)],
    speech_generation: Option<SpeechGeneration>,
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
            let (zh, zh_raw, homophone_fixes) = apply_source_corrections(&raw_text, corrections);
            let is_final = event.kind == EventKind::Final;
            let received_at = now_ms as f64 / 1000.0;
            let record = SubtitleRecord {
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
                speech_generation: if is_final { speech_generation } else { None },
                review_deadline: (is_final && event.refinement_status.as_deref() == Some(REFINEMENT_PENDING))
                    .then(|| Instant::now() + REFINEMENT_DEADLINE),
                source_audio: event.source_audio.clone(),
                source_audio_sha256: event.source_audio_sha256.clone(),
                source_audio_seconds: event.source_audio_seconds,
                audio_start_pts_ns: event.audio_start_pts_ns,
                audio_end_pts_ns: event.audio_end_pts_ns,
                asr_numeric_audit: event.asr_numeric_audit.clone(),
                asr_polarity_audit: event.asr_polarity_audit.clone(),
                refinement_status: event.refinement_status.clone(),
                refinement_error: None,
                translation_source_audit: event.translation_source_audit.clone(),
                homophone_fixes,
                zh_raw,
                translation_error: None,
                translated_at: None,
                translate_ms: None,
            };
            table.insert(event.utterance_id.clone(), record.clone());
            Some(record)
        }
        EventKind::Refined => {
            let record = table.get_mut(&event.utterance_id)?;
            if sealed_refinement(record) {
                return None;
            }
            // 复核文本非空才替换原文（ASR 仅在复核 completed 时携带 refined_text）；
            // 替换时同步更新/清理旧纠错审计字段，避免残留上一版的 zh_raw。
            if let Some(refined) = event.refined_text.as_deref() {
                let trimmed = refined.trim();
                if !trimmed.is_empty() {
                    let (zh, zh_raw, homophone_fixes) =
                        apply_source_corrections(trimmed, corrections);
                    record.zh = zh;
                    record.zh_raw = zh_raw;
                    record.homophone_fixes = homophone_fixes;
                }
            }
            if let Some(status) = &event.refinement_status {
                record.refinement_status = Some(status.clone());
                if is_terminal_refinement(Some(status.as_str())) {
                    record.review_deadline = None;
                }
            }
            record.revision += 1;
            Some(record.clone())
        }
    }
}

fn sealed_refinement(record: &SubtitleRecord) -> bool {
    matches!(record.refinement_error.as_deref(), Some(REVIEW_TIMEOUT | REVIEW_INTERRUPTED))
}

fn seal_refinement(record: &mut SubtitleRecord, reason: &str) {
    record.refinement_status = Some("skipped".into());
    record.refinement_error = Some(reason.to_owned());
    record.speech_generation = None;
    record.review_deadline = None;
    record.revision += 1;
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
        let file = OpenOptions::new().read(true).create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    /// Read at most the documented tail. A torn final line belongs to this writer;
    /// remove the incomplete suffix so the next valid append starts on a new line.
    fn recent_records(&mut self) -> io::Result<(Vec<SubtitleRecord>, bool)> {
        let len = self.file.metadata()?.len();
        let start = len.saturating_sub(RECOVERY_TAIL_BYTES);
        self.file.seek(SeekFrom::Start(start))?;
        let mut tail = Vec::new();
        (&mut self.file).take(len - start).read_to_end(&mut tail)?;
        let mut damaged = false;
        if !tail.is_empty() && !tail.ends_with(b"\n") {
            let safe_len = match tail.iter().rposition(|&byte| byte == b'\n') {
                Some(last) => last + 1,
                None if start == 0 => 0,
                None => return Err(io::Error::other("subtitle recovery cannot locate a complete line within the tail")),
            };
            self.file.set_len(start + safe_len as u64)?;
            tail.truncate(safe_len);
            damaged = true;
        }
        if start > 0 {
            // Locate the first complete line; only skip the initial line if the
            // preceding byte proves that the window begins inside a line.
            self.file.seek(SeekFrom::Start(start - 1))?;
            let mut previous = [0];
            self.file.read_exact(&mut previous)?;
            if previous[0] != b'\n' {
                let through = tail.iter().position(|&byte| byte == b'\n').map_or(tail.len(), |i| i + 1);
                tail.drain(..through);
            }
            eprintln!("mozart-bridge: subtitle recovery inspected only the last {RECOVERY_TAIL_BYTES} bytes");
        }
        let mut latest = HashMap::<String, (usize, SubtitleRecord)>::new();
        let lines = tail.split(|&byte| byte == b'\n').filter(|line| !line.is_empty());
        for (position, line) in lines.enumerate() {
            let record: SubtitleRecord = match serde_json::from_slice(line) {
                Ok(record) => record,
                Err(_) => {
                    // The unreadable line may be the newest revision of any
                    // previously seen sentence. Only trust records after it.
                    damaged = true;
                    latest.clear();
                    continue;
                }
            };
            if record.is_final && latest.get(&record.utterance_id).is_none_or(|(_, old)| record.revision >= old.revision) {
                latest.insert(record.utterance_id.clone(), (position, record));
            }
        }
        let mut recent: Vec<_> = latest.into_values().collect();
        recent.sort_unstable_by_key(|(position, _)| *position);
        if recent.len() > RECORD_CAP {
            eprintln!("mozart-bridge: subtitle recovery retained only the latest {RECORD_CAP} final utterances");
            recent.drain(..recent.len() - RECORD_CAP);
        }
        Ok((recent.into_iter().map(|(_, record)| record).collect(), damaged))
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

    /// 合并一条事件并落盘，返回新记录；None 表示忽略（测试便利入口）。
    #[cfg(test)]
    pub fn apply_event(
        &self,
        event: &Event,
        shared: &Shared,
    ) -> Result<Option<SubtitleRecord>, String> {
        Ok(self
            .apply_for_dispatch(event, shared)?
            .map(|(record, _)| record))
    }

    /// 合并一条事件并落盘，同时判定是否应入待翻译队列（复核开启时等 refined 落地再入队，
    /// 复核未开启则在 final 入队；每句至多触发一次）。
    pub fn apply_for_dispatch(
        &self,
        event: &Event,
        shared: &Shared,
    ) -> Result<Option<(SubtitleRecord, bool)>, String> {
        let now = unix_ms();
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let created = !records.contains_key(&event.utterance_id);
        let was_pending_review = records.peek(&event.utterance_id).is_some_and(|record| {
            record.is_final && record.refinement_status.as_deref() == Some(REFINEMENT_PENDING)
        });
        let Some(record) = apply(
            &mut records,
            event,
            now,
            &self.corrections,
            shared.speech_generation(),
        ) else {
            return Ok(None);
        };
        self.write_locked(&record)?;
        if created {
            shared.add_published();
        }
        shared.set_records_in_memory(records.len() as u32);
        shared.set_last_write_unix_ms(now);
        let ready_to_translate = match event.kind {
            EventKind::Partial => false,
            EventKind::Final => event.refinement_status.as_deref() != Some(REFINEMENT_PENDING),
            EventKind::Refined => {
                was_pending_review && is_terminal_refinement(event.refinement_status.as_deref())
            }
        };
        Ok(Some((record, ready_to_translate)))
    }

    fn recover(&self, shared: &Shared) -> Result<Vec<SubtitleRecord>, String> {
        let (recent, damaged) = self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .recent_records().map_err(|error| format!("恢复字幕 {}：{error}", self.path.display()))?;
        if damaged {
            shared.degrade(DEGRADED_RECOVERY);
            eprintln!("mozart-bridge: {DEGRADED_RECOVERY}");
        }
        let mut records = self.records.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut fallback = Vec::new();
        for mut record in recent {
            if record.refinement_status.as_deref() == Some(REFINEMENT_PENDING) {
                seal_refinement(&mut record, REVIEW_INTERRUPTED);
                self.write_locked(&record)?;
                shared.set_last_write_unix_ms(unix_ms());
            }
            if record.translation_status == TranslationStatus::Pending && sealed_refinement(&record) {
                fallback.push(record.clone());
            }
            records.insert(record.utterance_id.clone(), record);
        }
        shared.set_records_in_memory(records.len() as u32);
        Ok(fallback)
    }

    fn expire_refinements(&self, now: Instant, shared: &Shared) -> Result<Vec<SubtitleRecord>, String> {
        let mut records = self.records.lock().unwrap_or_else(|p| p.into_inner());
        let mut expired = Vec::new();
        for (record, _) in records.entries.values_mut() {
            if record.review_deadline.is_some_and(|deadline| now >= deadline) {
                seal_refinement(record, REVIEW_TIMEOUT);
                self.write_locked(record)?;
                expired.push(record.clone());
            }
        }
        if !expired.is_empty() { shared.set_last_write_unix_ms(unix_ms()); }
        Ok(expired)
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
        self.publish_outcome(
            utterance_id,
            TranslationStatus::Skipped,
            Some(reason),
            shared,
        )
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
    for record in store.recover(&shared)? {
        dispatch_fallback(record, &store, &queues, &shared)?;
    }
    let mut next_expiry = Instant::now();
    while !stop.load(Relaxed) {
        let now = Instant::now();
        if now >= next_expiry {
            for record in store.expire_refinements(now, &shared)? {
                dispatch_fallback(record, &store, &queues, &shared)?;
            }
            next_expiry = now + RECV_POLL;
        }
        let event = match rx.recv_timeout(RECV_POLL) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            // 事件线程已退出。
            Err(RecvTimeoutError::Disconnected) => break,
        };
        process(event, &store, &queues, &shared)?;
    }
    Ok(())
}

fn dispatch_fallback(record: SubtitleRecord, store: &SubtitleStore, queues: &Queues, shared: &Shared) -> Result<(), String> {
    if !shared.translate_enabled() {
        return store.publish_translation_skipped(&record.utterance_id, "translation disabled", shared);
    }
    let job = TranslateJob { utterance_id: record.utterance_id, text: record.zh, speech_generation: None };
    if queues.push_translate(job.clone()) {
        shared.resolve(DEGRADED_TRANSLATE_FULL);
        Ok(())
    } else {
        shared.degrade(DEGRADED_TRANSLATE_FULL);
        store.publish_translation_skipped(&job.utterance_id, DEGRADED_TRANSLATE_FULL, shared)
    }
}

fn process(
    event: Event,
    store: &SubtitleStore,
    queues: &Queues,
    shared: &Shared,
) -> Result<(), String> {
    let Some((record, ready_to_translate)) = store.apply_for_dispatch(&event, shared)? else {
        return Ok(());
    };
    // 复核开启时等 refined 再入待翻译队列；复核关闭时在 final 入队；translate off 只发布原文。
    if ready_to_translate && shared.translate_enabled() {
        let job = TranslateJob {
            utterance_id: record.utterance_id.clone(),
            text: record.zh.clone(),
            speech_generation: record.speech_generation,
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

    fn apply(
        table: &mut RecordTable,
        event: &Event,
        now_ms: u64,
        corrections: &[(String, String)],
    ) -> Option<SubtitleRecord> {
        super::apply(table, event, now_ms, corrections, None)
    }
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

    fn pending_final(seq: u64) -> Event {
        let mut event = event(EventKind::Final, seq, "在线原文");
        event.refinement_status = Some(REFINEMENT_PENDING.to_owned());
        event
    }

    #[test]
    fn lost_refinement_expires_once_and_falls_back_to_text_only() {
        let dir = crate::testutil::temp_dir("review-timeout");
        let path = dir.join("subtitles.jsonl");
        let shared = Shared::new(crate::state::Switches { translate_enabled: true, speech_enabled: true }, path.clone());
        let store = SubtitleStore::new(JsonlWriter::open(&path).unwrap(), path.clone(), no_corrections());
        let (record, ready) = store.apply_for_dispatch(&pending_final(1), &shared).unwrap().unwrap();
        assert!(!ready);
        assert!(record.speech_generation.is_some());
        let expiry = record.review_deadline.unwrap();
        assert!(store.expire_refinements(expiry - Duration::from_millis(1), &shared).unwrap().is_empty());
        let mut expired = store.expire_refinements(expiry, &shared).unwrap();
        assert_eq!(expired.len(), 1);
        let fallback = expired.pop().unwrap();
        assert_eq!(fallback.refinement_status.as_deref(), Some("skipped"));
        assert_eq!(fallback.refinement_error.as_deref(), Some(REVIEW_TIMEOUT));
        assert_eq!(fallback.revision, record.revision + 1);
        let queues = Queues::new(2, 2);
        dispatch_fallback(fallback, &store, &queues, &shared).unwrap();
        let job = queues.recv_translate(Duration::ZERO).unwrap();
        assert_eq!(job.text, "在线原文");
        assert!(job.speech_generation.is_none());
        assert!(store.apply_for_dispatch(&refined_event(1, Some("迟到复核")), &shared).unwrap().is_none());
        assert!(store.expire_refinements(expiry, &shared).unwrap().is_empty());
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restart_recovers_latest_pending_revision_and_never_replays_completed() {
        let dir = crate::testutil::temp_dir("review-restart");
        let path = dir.join("subtitles.jsonl");
        let shared = Shared::new(crate::state::Switches { translate_enabled: true, speech_enabled: true }, path.clone());
        {
            let store = SubtitleStore::new(JsonlWriter::open(&path).unwrap(), path.clone(), no_corrections());
            store.apply_for_dispatch(&pending_final(1), &shared).unwrap();
            store.apply_for_dispatch(&pending_final(2), &shared).unwrap();
            store.apply_for_dispatch(&refined_event(2, Some("已有复核")), &shared).unwrap();
            store.publish_translation("a1b2c3d4:2", "Finished.", 1, &shared).unwrap();
        }
        OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"torn").unwrap();
        let store = SubtitleStore::new(JsonlWriter::open(&path).unwrap(), path.clone(), no_corrections());
        let recovered = store.recover(&shared).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].utterance_id, "a1b2c3d4:1");
        assert_eq!(recovered[0].refinement_error.as_deref(), Some(REVIEW_INTERRUPTED));
        assert!(recovered[0].speech_generation.is_none());
        assert!(store.apply_for_dispatch(&pending_final(2), &shared).unwrap().is_none());
        assert!(store.apply_for_dispatch(&refined_event(1, Some("迟到复核")), &shared).unwrap().is_none());
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 5);
        assert!(text.lines().all(|line| serde_json::from_str::<Value>(line).is_ok()));
        drop(store);
        // If a restart happens after the skipped revision but before translation, retry text only.
        let store = SubtitleStore::new(JsonlWriter::open(&path).unwrap(), path, no_corrections());
        let replay = store.recover(&shared).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].revision, recovered[0].revision);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_complete_line_cannot_replay_older_pending_records() {
        let dir = crate::testutil::temp_dir("review-corrupt-line");
        let path = dir.join("subtitles.jsonl");
        let mut writer = JsonlWriter::open(&path).unwrap();
        let mut table = RecordTable::new();
        let old = apply(&mut table, &pending_final(1), 1, &no_corrections()).unwrap();
        writer.write(&old).unwrap();
        writer.file.write_all(b"{corrupt completed revision}\n").unwrap();
        let fresh = apply(&mut table, &pending_final(2), 2, &no_corrections()).unwrap();
        writer.write(&fresh).unwrap();
        let (records, damaged) = writer.recent_records().unwrap();
        assert!(damaged);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].utterance_id, fresh.utterance_id);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovery_starts_at_a_complete_line_in_the_bounded_tail() {
        let dir = crate::testutil::temp_dir("review-tail-bound");
        let path = dir.join("subtitles.jsonl");
        let mut writer = JsonlWriter::open(&path).unwrap();
        let mut table = RecordTable::new();
        let old = apply(&mut table, &pending_final(1), 1, &no_corrections()).unwrap();
        let recent = apply(&mut table, &pending_final(2), 2, &no_corrections()).unwrap();
        writer.write(&old).unwrap();
        writer.file.write_all(&vec![b' '; RECOVERY_TAIL_BYTES as usize]).unwrap();
        writer.file.write_all(b"\n").unwrap();
        writer.write(&recent).unwrap();
        let (recovered, damaged) = writer.recent_records().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].utterance_id, recent.utterance_id);
        assert!(!damaged, "the truncated prefix is not a parsed record");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_final_refined_merge_with_revisions() {
        let mut table = RecordTable::new();
        let first = apply(
            &mut table,
            &event(EventKind::Partial, 1, "你"),
            100,
            &no_corrections(),
        )
        .unwrap();
        assert_eq!(first.revision, 0);
        assert!(!first.is_final);
        assert_eq!(first.translation_status, TranslationStatus::Recognizing);
        assert_eq!(first.seq, 1);
        assert_eq!(first.zh, "你");
        assert_eq!(first.en, "");

        let second = apply(
            &mut table,
            &event(EventKind::Partial, 1, "你好"),
            200,
            &no_corrections(),
        )
        .unwrap();
        assert_eq!(second.revision, 1);
        assert_eq!(second.zh, "你好");

        let third = apply(
            &mut table,
            &event(EventKind::Final, 1, "你好"),
            300,
            &no_corrections(),
        )
        .unwrap();
        assert_eq!(third.revision, 2);
        assert!(third.is_final);
        assert_eq!(third.translation_status, TranslationStatus::Pending);

        let fourth = apply(
            &mut table,
            &refined_event(1, Some("你好。")),
            400,
            &no_corrections(),
        )
        .unwrap();
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
        apply(
            &mut table,
            &event(EventKind::Final, 1, "你好"),
            100,
            &no_corrections(),
        );
        assert_eq!(
            apply(
                &mut table,
                &event(EventKind::Partial, 1, "晚到"),
                200,
                &no_corrections()
            ),
            None
        );
        assert_eq!(
            apply(
                &mut table,
                &event(EventKind::Final, 1, "重写"),
                300,
                &no_corrections()
            ),
            None
        );
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
        let record = apply(
            &mut table,
            &event(EventKind::Final, 7, "整句"),
            100,
            &no_corrections(),
        )
        .unwrap();
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
        apply(
            &mut table,
            &event(EventKind::Final, 1, "你好"),
            100,
            &no_corrections(),
        );
        // 复核 failed/skipped：ASR 不携带 refined_text。
        let record = apply(&mut table, &refined_event(1, None), 200, &no_corrections()).unwrap();
        assert_eq!(record.zh, "你好");
        assert_eq!(record.revision, 1);
        // 空白复核文本同样不替换。
        let record = apply(
            &mut table,
            &refined_event(1, Some("  ")),
            300,
            &no_corrections(),
        )
        .unwrap();
        assert_eq!(record.zh, "你好");
        assert_eq!(record.revision, 2);
    }

    #[test]
    fn distinct_utterances_keep_separate_revisions() {
        let mut table = RecordTable::new();
        apply(
            &mut table,
            &event(EventKind::Partial, 1, "甲"),
            100,
            &no_corrections(),
        );
        let second = apply(
            &mut table,
            &event(EventKind::Partial, 2, "乙"),
            200,
            &no_corrections(),
        )
        .unwrap();
        assert_eq!(second.revision, 0);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn correction_applies_to_zh_and_records_fixes() {
        let mut table = RecordTable::new();
        let corrections = preprocess::builtin_corrections();
        let record = apply(
            &mut table,
            &event(EventKind::Final, 1, "我以经说了"),
            100,
            &corrections,
        )
        .unwrap();
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
        let record = apply(
            &mut table,
            &event(EventKind::Partial, 1, "你好"),
            1,
            &no_corrections(),
        )
        .unwrap();
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
        use crate::state::{Shared, Switches};

        let dir = std::env::temp_dir().join(format!(
            "mozart-bridge-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("subtitles.jsonl");
        let shared = Shared::new(
            Switches {
                translate_enabled: true,
                speech_enabled: true,
            },
            path.clone(),
        );
        let store = SubtitleStore::new(
            JsonlWriter::open(&path).unwrap(),
            path.clone(),
            no_corrections(),
        );

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
        use crate::state::{Shared, Switches};

        let dir = std::env::temp_dir().join(format!(
            "mozart-bridge-outcome-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("subtitles.jsonl");
        let shared = Shared::new(
            Switches {
                translate_enabled: true,
                speech_enabled: true,
            },
            path.clone(),
        );
        let store = SubtitleStore::new(
            JsonlWriter::open(&path).unwrap(),
            path.clone(),
            no_corrections(),
        );
        store
            .apply_event(&event(EventKind::Final, 1, "你好"), &shared)
            .unwrap();
        store
            .publish_translation_failed("a1b2c3d4:1", "Translation rejected: x", &shared)
            .unwrap();
        store
            .apply_event(&event(EventKind::Final, 2, "世界"), &shared)
            .unwrap();
        store
            .publish_translation_skipped("a1b2c3d4:2", "translate off", &shared)
            .unwrap();

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
        apply(
            &mut table,
            &event(EventKind::Final, 0, "在途"),
            1,
            &no_corrections(),
        );
        // 灌入超过上限的已完成句（可淘汰）。
        for seq in 1..=200u64 {
            apply(
                &mut table,
                &event(EventKind::Final, seq, "句"),
                seq,
                &no_corrections(),
            );
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
            apply(
                &mut table,
                &event(EventKind::Final, seq, "句"),
                seq,
                &no_corrections(),
            );
        }
        assert_eq!(table.len(), 150);
        assert!(table.contains_key("a1b2c3d4:1"));
    }

    #[test]
    fn dispatch_waits_for_refined_when_pending_and_fires_once() {
        use crate::state::{Shared, Switches};

        let dir = std::env::temp_dir().join(format!(
            "mozart-bridge-dispatch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("subtitles.jsonl");
        let shared = Shared::new(
            Switches {
                translate_enabled: true,
                speech_enabled: true,
            },
            path.clone(),
        );
        let store = SubtitleStore::new(
            JsonlWriter::open(&path).unwrap(),
            path.clone(),
            no_corrections(),
        );

        let final_pending = parse(
            &serde_json::to_vec(&json!({
                "type": "final",
                "startup_id": "a1b2c3d4",
                "utterance_id": "a1b2c3d4:1",
                "seq": 1,
                "text": "初版错字",
                "refinement_status": "pending",
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let (_, ready) = store.apply_for_dispatch(&final_pending, &shared).unwrap().unwrap();
        assert!(!ready, "开启复核时 final 不得直接入翻译队列");

        let (rec, ready) = store
            .apply_for_dispatch(&refined_event(1, Some("复核正文")), &shared)
            .unwrap()
            .unwrap();
        assert!(ready, "refined 到达后应触发一次翻译");
        assert_eq!(rec.zh, "复核正文");

        // 重复 refined 不得再触发第二次。
        let (_, ready_again) = store
            .apply_for_dispatch(&refined_event(1, Some("重复复核")), &shared)
            .unwrap()
            .unwrap();
        assert!(!ready_again, "同一句不得触发第二次翻译");

        // 复核跳过/失败（无 refined_text）也应兜底用原文触发一次。
        let final_pending_2 = parse(
            &serde_json::to_vec(&json!({
                "type": "final",
                "startup_id": "a1b2c3d4",
                "utterance_id": "a1b2c3d4:2",
                "seq": 2,
                "text": "兜底原文",
                "refinement_status": "pending",
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(!store.apply_for_dispatch(&final_pending_2, &shared).unwrap().unwrap().1);
        let refined_skipped = parse(
            &serde_json::to_vec(&json!({
                "type": "refined",
                "startup_id": "a1b2c3d4",
                "utterance_id": "a1b2c3d4:2",
                "seq": 2,
                "text": "兜底原文",
                "refinement_status": "skipped",
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let (rec2, ready2) = store.apply_for_dispatch(&refined_skipped, &shared).unwrap().unwrap();
        assert!(ready2);
        assert_eq!(rec2.zh, "兜底原文");

        // 复核关闭（disabled）则在 final 立即触发，且后续 stray refined 不重复触发。
        let final_disabled = parse(
            &serde_json::to_vec(&json!({
                "type": "final",
                "startup_id": "a1b2c3d4",
                "utterance_id": "a1b2c3d4:3",
                "seq": 3,
                "text": "直接翻译",
                "refinement_status": "disabled",
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(store.apply_for_dispatch(&final_disabled, &shared).unwrap().unwrap().1);
        assert!(!store.apply_for_dispatch(&refined_event(3, Some("晚到复核")), &shared).unwrap().unwrap().1);

        // 非终态 refined（如缺状态）不提前触发，终态 refined 到来后才触发。
        let final_pending_4 = parse(
            &serde_json::to_vec(&json!({
                "type": "final",
                "startup_id": "a1b2c3d4",
                "utterance_id": "a1b2c3d4:4",
                "seq": 4,
                "text": "等待终态",
                "refinement_status": "pending",
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(!store.apply_for_dispatch(&final_pending_4, &shared).unwrap().unwrap().1);
        let non_terminal_refined = event(EventKind::Refined, 4, "等待终态");
        assert!(!store.apply_for_dispatch(&non_terminal_refined, &shared).unwrap().unwrap().1);
        assert!(store.apply_for_dispatch(&refined_event(4, Some("终态完成")), &shared).unwrap().unwrap().1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refined_clears_stale_homophone_fixes_from_final() {
        let mut table = RecordTable::new();
        let corrections = preprocess::builtin_corrections();
        let final_rec = apply(
            &mut table,
            &event(EventKind::Final, 1, "我以经说了"),
            100,
            &corrections,
        )
        .unwrap();
        assert_eq!(final_rec.zh, "我已经说了");
        assert_eq!(final_rec.zh_raw.as_deref(), Some("我以经说了"));
        assert!(final_rec.homophone_fixes.is_some());

        let refined_rec = apply(
            &mut table,
            &refined_event(1, Some("我已经说完了。")),
            200,
            &corrections,
        )
        .unwrap();
        assert_eq!(refined_rec.zh, "我已经说完了。");
        assert!(refined_rec.zh_raw.is_none(), "复核未命中纠错时不得残留旧 zh_raw");
        assert!(refined_rec.homophone_fixes.is_none(), "复核未命中纠错时不得残留旧 homophone_fixes");
    }
}
