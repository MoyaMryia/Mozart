//! 模型预设（SPEC-11）集成测试：首次自动建、save/use/list、模型切换、
//! 文件与实时跨链、损坏预设与 NotReady。桩经 MOZART_STUB_LOG 记录命令与
//! StartStream 参数体；MOZART_RVC_STUB_MODELS 提供多模型注册表。

mod common;

use common::{
    call, example, status, submit_file, wait_job_state, wait_ready, wait_until, DaemonProcess,
};
use mozart_core::ctl::schema::{
    FileJobState, PresetApplyData, PresetListData, Response, Route, RvcParams, Status,
};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const POLL_TIMEOUT: Duration = Duration::from_secs(5);
const JOB_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
struct LogEntry {
    stub: String,
    type_: u16,
    body: Value,
}

fn parse_entry(line: &str) -> Option<LogEntry> {
    let mut parts = line.splitn(3, ' ');
    let stub = parts.next()?.to_string();
    let type_ = parts.next()?.parse::<u16>().ok()?;
    let body = serde_json::from_str(parts.next()?).ok()?;
    Some(LogEntry { stub, type_, body })
}

struct Harness {
    daemon: DaemonProcess,
    conn: Seqpacket,
    dir: PathBuf,
    models: PathBuf,
    log: PathBuf,
}

impl Harness {
    /// 每个用例独立模型目录（不写仓库资产）；配置模型 id 为空表示不预载。
    fn new(tag: &str, models: &[&str], config_model: &str) -> Self {
        Self::build(tag, models, config_model, None)
    }

    /// rvc 启动即已加载 `preload` 模型（不起流）：daemon 未经历预设载入，
    /// 参数来源记录为空。
    fn new_preloaded(tag: &str, models: &[&str], config_model: &str, preload: &str) -> Self {
        Self::build(tag, models, config_model, Some(preload))
    }

    fn build(tag: &str, models: &[&str], config_model: &str, preload: Option<&str>) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mozart-preset-it-{}-{tag}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let model_root = dir.join("models");
        for model in models {
            std::fs::create_dir_all(model_root.join(model)).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("commands.log");
        let script = dir.join("rvc.sh");
        let mut text = format!("#!/bin/sh\nexport MOZART_STUB_LOG='{}'\n", log.display());
        text.push_str(&format!(
            "export MOZART_RVC_STUB_MODELS='{}'\n",
            models.join(",")
        ));
        if let Some(preload) = preload {
            text.push_str(&format!("export MOZART_RVC_STUB_PRELOAD='{preload}'\n"));
        }
        text.push_str(&format!("exec '{}' \"$@\"\n", example("rvc_stub").display()));
        std::fs::write(&script, text).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let storage = dir.join("files");
        let config = format!(
            "[daemon]\nrpc_timeout_ms = 1000\n\n[rvc]\nmodel_id = \"{config_model}\"\nmodels_dir = \"{}\"\n\n[file]\nstorage_dir = \"{}\"\n",
            model_root.display(),
            storage.display()
        );
        let daemon = DaemonProcess::start_with_config(tag, &example("pre_stub"), &script, &config);
        let conn = daemon.connect();
        wait_ready(&conn);
        Self {
            daemon,
            conn,
            dir,
            models: model_root,
            log,
        }
    }

    fn call(&self, type_: u16, body: &[u8]) -> Response {
        call(&self.conn, type_, body)
    }

    fn status(&self) -> Status {
        status(&self.conn)
    }

    fn rvc_params(&self) -> RvcParams {
        self.status().rvc_params.expect("status 应带 rvc_params")
    }

    fn config_set(&self, key: &str, value: &str) -> Response {
        let body = json!({"key": key, "value": value}).to_string();
        self.call(ctl::DAEMON_CONFIG_SET, body.as_bytes())
    }

    fn preset_save(&self) -> Response {
        self.call(ctl::DAEMON_PRESET_SAVE, b"{}")
    }

    fn preset_use(&self) -> Response {
        self.call(ctl::DAEMON_PRESET_USE, b"{}")
    }

    fn preset_list(&self) -> Response {
        self.call(ctl::DAEMON_PRESET_LIST, b"{}")
    }

    fn preset_path(&self, model: &str) -> PathBuf {
        self.models.join(model).join("presets/default.toml")
    }

    fn clear_log(&self) {
        std::fs::write(&self.log, "").unwrap();
    }

    fn entries(&self) -> Vec<LogEntry> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(parse_entry)
            .collect()
    }

    fn stream_starts(&self, kind: &str) -> Vec<Value> {
        self.entries()
            .into_iter()
            .filter(|entry| {
                entry.stub == "mozart-rvc-stub"
                    && entry.type_ == ctl::RVC_START_STREAM
                    && entry.body["kind"] == kind
            })
            .map(|entry| entry.body)
            .collect()
    }

    fn rvc_count(&self, type_: u16) -> usize {
        self.entries()
            .iter()
            .filter(|entry| entry.stub == "mozart-rvc-stub" && entry.type_ == type_)
            .count()
    }

    fn submit(&self, name: &str, model: &str) -> String {
        let source = self.dir.join(name);
        write_wav(&source, 1, 16000);
        let reply = submit_file(&self.conn, &source, Some(model));
        assert_eq!(reply.status, "ok", "submit: {}", reply.reason);
        reply.data["job_id"].as_str().unwrap().to_string()
    }

    fn run_file(&self, name: &str, model: &str) {
        let job_id = self.submit(name, model);
        let job = wait_job_state(&self.conn, &job_id, FileJobState::Completed, JOB_TIMEOUT);
        assert!(job.result_ready, "文件任务应完成：{job:?}");
    }

    fn run_file_failed(&self, name: &str, model: &str) -> String {
        let job_id = self.submit(name, model);
        let job = wait_job_state(&self.conn, &job_id, FileJobState::Failed, JOB_TIMEOUT);
        job.error.unwrap_or_default()
    }

    fn set_route_rvc(&self) {
        let reply = self.call(ctl::DAEMON_ROUTE_SET, br#"{"route":"rvc"}"#);
        assert_eq!(reply.status, "ok", "route set rvc: {}", reply.reason);
        self.wait_streaming();
    }

    fn wait_streaming(&self) {
        assert!(
            wait_until(POLL_TIMEOUT, || {
                let status = self.status();
                status.rvc.as_ref().is_some_and(|rvc| {
                    rvc.state == "streaming" && rvc.stream_kind.as_deref() == Some("realtime")
                }) && status.route.active == Route::Rvc
                    && status.pre.as_ref().is_some_and(|pre| pre.route == Route::Rvc)
            }),
            "实时流未就绪：{:?}",
            self.status()
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn expect_ok(reply: Response) -> Value {
    assert_eq!(reply.status, "ok", "{}: {}", reply.code, reply.reason);
    reply.data
}

fn expect_error(reply: Response, code: &str) -> String {
    assert_eq!(reply.status, "error", "应失败：{:?}", reply.data);
    assert_eq!(reply.code, code, "reason: {}", reply.reason);
    reply.reason
}

fn apply_data(reply: Response) -> PresetApplyData {
    serde_json::from_value(expect_ok(reply)).unwrap()
}

fn list_data(reply: Response) -> PresetListData {
    serde_json::from_value(expect_ok(reply)).unwrap()
}

#[test]
fn first_file_job_creates_default_preset_and_uses_canonical_values() {
    let h = Harness::new("first", &["model-a"], "model-a");
    h.clear_log();
    h.run_file("a.wav", "model-a");

    let path = h.preset_path("model-a");
    let text = std::fs::read_to_string(&path).expect("首次加载应自动创建 default.toml");
    for key in [
        "version = 1",
        "pitch_shift = 0",
        "index_rate = 0.0",
        "rms_mix_rate = 1.0",
        "protect = 0.33",
        "f0_method = \"rmvpe\"",
    ] {
        assert!(text.contains(key), "预设缺少 {key}：{text}");
    }

    let starts = h.stream_starts("file");
    assert_eq!(starts.len(), 1, "一个文件任务应只起一次文件流：{starts:?}");
    assert_eq!(starts[0]["pitch_shift"], 0);
    assert_eq!(starts[0]["index_rate"], 0.0);
    assert_eq!(starts[0]["rms_mix_rate"], 1.0);
    assert_eq!(starts[0]["protect"], 0.33);
    assert_eq!(starts[0]["f0_method"], "rmvpe");

    let config = std::fs::read_to_string(h.daemon.dir.join("config.toml")).unwrap();
    assert!(config.contains("pitch_shift = 0"), "载入应写 TOML 基线：{config}");
}

#[test]
fn save_use_list_track_live_values_without_writing_config() {
    let h = Harness::new("save-use", &["model-a"], "model-a");
    h.run_file("a.wav", "model-a");
    h.clear_log();
    expect_ok(h.config_set("rvc.pitch_shift", "3"));
    assert_eq!(h.rvc_params().pitch_shift, 3);

    let before = std::fs::read_to_string(h.daemon.dir.join("config.toml")).unwrap();
    let saved = apply_data(h.preset_save());
    assert_eq!(saved.model_id, "model-a");
    assert_eq!(saved.preset, "default");
    assert_eq!(saved.values.pitch_shift, 3);
    let after = std::fs::read_to_string(h.daemon.dir.join("config.toml")).unwrap();
    assert_eq!(before, after, "preset save 不得改 TOML 基线");

    expect_ok(h.config_set("rvc.pitch_shift", "-2"));
    let list = list_data(h.preset_list());
    assert_eq!(list.model_id, "model-a");
    assert!(list.dirty, "live 与预设不同应标 dirty");
    assert_eq!(list.preset_values.pitch_shift, 3);
    assert_eq!(list.live_values.pitch_shift, -2);

    let used = apply_data(h.preset_use());
    assert_eq!(used.values.pitch_shift, 3);
    assert_eq!(h.rvc_params().pitch_shift, 3, "preset use 应重置 live");
    assert!(!list_data(h.preset_list()).dirty, "重置后不应 dirty");
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0, "direct 路由不得停流");
}

#[test]
fn preset_use_restarts_realtime_stream_when_realtime_params_change() {
    let h = Harness::new("realtime", &["model-a"], "model-a");
    h.set_route_rvc();
    expect_ok(h.config_set("rvc.pitch_shift", "3"));
    apply_data(h.preset_save());
    expect_ok(h.config_set("rvc.pitch_shift", "5"));
    h.wait_streaming();
    h.clear_log();

    let used = apply_data(h.preset_use());
    assert_eq!(used.values.pitch_shift, 3);

    let entries = h.entries();
    let stop = entries
        .iter()
        .position(|entry| entry.stub == "mozart-rvc-stub" && entry.type_ == ctl::RVC_STOP_STREAM)
        .expect("重置实时参数应先停流");
    let (start_index, start) = entries
        .iter()
        .enumerate()
        .find(|(_, entry)| {
            entry.stub == "mozart-rvc-stub"
                && entry.type_ == ctl::RVC_START_STREAM
                && entry.body["kind"] == "realtime"
        })
        .map(|(index, entry)| (index, &entry.body))
        .expect("停流后应重建实时流");
    assert!(stop < start_index, "必须先停后起：{entries:?}");
    assert_eq!(start["pitch_shift"], 3, "StartStream 应带预设值：{start}");
    assert_eq!(h.rvc_params().pitch_shift, 3);
    h.wait_streaming();
}

#[test]
fn model_switch_loads_each_default_and_same_model_keeps_tuning() {
    let h = Harness::new("switch", &["model-a", "model-b"], "model-a");
    h.run_file("a1.wav", "model-a");
    expect_ok(h.config_set("rvc.pitch_shift", "3"));
    apply_data(h.preset_save());

    h.clear_log();
    h.run_file("b.wav", "model-b");
    h.run_file("a2.wav", "model-a");
    expect_ok(h.config_set("rvc.pitch_shift", "5"));
    h.run_file("a3.wav", "model-a");

    let starts = h.stream_starts("file");
    let pitches: Vec<i64> = starts
        .iter()
        .map(|body| body["pitch_shift"].as_i64().unwrap())
        .collect();
    assert_eq!(
        pitches,
        vec![0, 3, 5],
        "B 用 B 默认、切回 A 用 A 预设、同模型连续任务保留调参：{starts:?}"
    );
    assert!(h.preset_path("model-b").is_file(), "B 首次加载应自动建预设");
}

#[test]
fn corrupt_preset_fails_file_job_and_preset_use_without_baseline_write() {
    let h = Harness::new("corrupt", &["model-a", "model-b"], "model-a");
    let bad = h.preset_path("model-a");
    std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
    std::fs::write(&bad, "version = 1\npitch_shift = 99\n").unwrap();

    let reason = h.run_file_failed("a.wav", "model-a");
    assert!(reason.contains("preset"), "失败原因应指明预设：{reason}");
    // 坏的是服务端资产文件，不是 submit 参数：错误码为 Internal。
    assert!(
        reason.contains(&format!("preset load: {}:", ctl::CODE_INTERNAL)),
        "文件任务错误码应为 Internal：{reason}"
    );

    // 队列继续：另一模型的任务正常完成。
    h.run_file("b.wav", "model-b");

    // 当前已加载 B；损坏 B 的预设后 preset use 报错，且不写基线。
    let loaded = h.status().rvc.unwrap().model_id.unwrap();
    assert_eq!(loaded, "model-b");
    expect_ok(h.config_set("rvc.pitch_shift", "3"));
    std::fs::write(h.preset_path("model-b"), "not toml").unwrap();
    let reason = expect_error(h.preset_use(), ctl::CODE_INTERNAL);
    assert!(reason.contains("preset"), "{reason}");
    assert_eq!(h.rvc_params().pitch_shift, 3, "损坏预设不得覆盖 live 基线");
}

#[test]
fn failed_model_load_does_not_overwrite_loaded_model_preset() {
    let h = Harness::new("failed-load", &["model-a"], "model-a");
    // model-b 目录存在、预设可建，但桩不加载它。
    std::fs::create_dir_all(h.models.join("model-b")).unwrap();
    h.run_file("a1.wav", "model-a");
    expect_ok(h.config_set("rvc.pitch_shift", "3"));
    let saved = apply_data(h.preset_save());
    let before = std::fs::read_to_string(h.preset_path("model-a")).unwrap();

    let reason = h.run_file_failed("b.wav", "model-b");
    assert!(reason.contains("model not found"), "{reason}");

    // 写基线在 LoadModel 之后：B 加载失败不写基线，live 仍是 A 的预设值，
    // rvc 也仍加载 A，两者不错位。
    let list = list_data(h.preset_list());
    assert_eq!(list.model_id, "model-a");
    assert_eq!(list.live_values, saved.values, "加载失败不得改 live 参数");
    assert!(!list.dirty, "live 应仍等于 A 的预设：{list:?}");
    assert_eq!(
        before,
        std::fs::read_to_string(h.preset_path("model-a")).unwrap(),
        "A 的预设文件不得被改动"
    );

    // 参数来源仍是 A：save 正常成功，不被错位拒绝。
    let again = apply_data(h.preset_save());
    assert_eq!(again.model_id, "model-a");
    assert_eq!(again.values, saved.values);

    // 下一次 A 任务沿用 A 的参数，不会读到 B 的预设。
    h.clear_log();
    h.run_file("a2.wav", "model-a");
    let starts = h.stream_starts("file");
    assert_eq!(starts[0]["pitch_shift"], 3, "{starts:?}");
}

#[test]
fn failed_realtime_model_load_keeps_live_params() {
    // 实时目标固定 model-b（桩不加载它）：切换失败不得写基线或改 live 参数。
    let h = Harness::new("failed-realtime", &["model-a"], "model-b");
    std::fs::create_dir_all(h.models.join("model-b")).unwrap();
    h.run_file("a.wav", "model-a");
    expect_ok(h.config_set("rvc.pitch_shift", "3"));
    let saved = apply_data(h.preset_save());
    let live_before = h.rvc_params();
    let baseline_before = std::fs::read_to_string(h.daemon.dir.join("config.toml")).unwrap();

    let reply = h.call(ctl::DAEMON_ROUTE_SET, br#"{"route":"rvc"}"#);
    let reason = expect_error(reply, ctl::CODE_INVALID_ARGUMENT);
    assert!(reason.contains("model not found"), "{reason}");

    assert_eq!(h.rvc_params(), live_before, "LoadModel 失败不得改 live 参数");
    assert_eq!(
        baseline_before,
        std::fs::read_to_string(h.daemon.dir.join("config.toml")).unwrap(),
        "LoadModel 失败不得写 TOML 基线"
    );
    assert!(
        h.stream_starts("realtime").is_empty(),
        "加载失败不得起实时流：{:?}",
        h.entries()
    );
    // 来源仍是 A：save 正常成功，A 预设不被 B 的默认值覆盖。
    assert_eq!(apply_data(h.preset_save()).values, saved.values);
}

#[test]
fn missing_model_dir_reports_load_model_error_not_preset_error() {
    let h = Harness::new("missing-dir", &["model-a"], "model-a");
    h.run_file("a.wav", "model-a");
    expect_ok(h.config_set("rvc.pitch_shift", "3"));
    let live_before = h.rvc_params();
    let baseline_before = std::fs::read_to_string(h.daemon.dir.join("config.toml")).unwrap();

    // 目录不存在：预设层不预检模型存在性，报错归 LoadModel（SPEC-10）。
    let reason = h.run_file_failed("ghost.wav", "ghost-model");
    assert!(reason.contains("model not found"), "报错须归推理层：{reason}");
    assert!(
        reason.contains(ctl::CODE_INVALID_ARGUMENT),
        "缺失模型应回 InvalidArgument：{reason}"
    );
    assert!(!reason.contains("preset"), "预设层不得抢报错权：{reason}");
    assert!(!h.models.join("ghost-model").exists(), "不得创建模型目录");
    assert_eq!(h.rvc_params(), live_before, "live 参数不得变");
    assert_eq!(
        baseline_before,
        std::fs::read_to_string(h.daemon.dir.join("config.toml")).unwrap(),
        "TOML 基线不得变"
    );
}

#[test]
fn save_rejects_when_param_source_unknown() {
    // rvc 启动即加载 model-a，daemon 未经历载入；路由保持 direct，实时链
    // 不触发载入，来源记录一直为空。此时 live 参数可能属于别的模型。
    let h = Harness::new_preloaded("unknown-source", &["model-a"], "", "model-a");
    assert_eq!(
        h.status().rvc.unwrap().model_id.as_deref(),
        Some("model-a"),
        "桩应预加载模型"
    );

    let reason = expect_error(h.preset_save(), ctl::CODE_NOT_READY);
    assert!(reason.contains("source is unknown"), "{reason}");
    assert!(!h.preset_path("model-a").exists(), "拒绝时不得写预设");

    // preset use 载入一次后来源明确，save 放行。
    apply_data(h.preset_use());
    expect_ok(h.preset_save());
}

#[test]
fn preset_commands_require_loaded_model_and_list_requires_preset() {
    let h = Harness::new("not-ready", &["model-a"], "");
    for reply in [h.preset_save(), h.preset_use(), h.preset_list()] {
        expect_error(reply, ctl::CODE_NOT_READY);
    }

    // 载入模型后 list 缺失：只读报错；use 按首次载入自动建。
    let h = Harness::new("missing-list", &["model-a"], "model-a");
    h.run_file("a.wav", "model-a");
    std::fs::remove_file(h.preset_path("model-a")).unwrap();
    let reason = expect_error(h.preset_list(), ctl::CODE_NOT_READY);
    assert!(reason.contains("preset missing"), "{reason}");

    let used = apply_data(h.preset_use());
    assert_eq!(used.values.pitch_shift, 0);
    assert!(h.preset_path("model-a").is_file(), "use 应重新创建 default");
}

#[test]
fn realtime_switch_after_file_job_uses_target_model_preset() {
    let h = Harness::new("cross-chain", &["model-a", "model-b"], "model-a");
    h.run_file("a.wav", "model-a");
    expect_ok(h.config_set("rvc.pitch_shift", "4"));
    apply_data(h.preset_save());

    // 文件任务切到 B：live 基线变成 B 的默认值。
    h.run_file("b.wav", "model-b");
    h.clear_log();

    // 实时目标仍是 model-a（[rvc].model_id）；切回时必须读 A 的预设。
    h.set_route_rvc();
    let starts = h.stream_starts("realtime");
    let last = starts.last().expect("实时流应已启动");
    assert_eq!(last["pitch_shift"], 4, "应载入 A 的预设：{starts:?}");
    assert_eq!(h.rvc_params().pitch_shift, 4);
}

/// 生成 PCM16 单声道正弦 WAV（同 file_worker 用例）。
fn write_wav(path: &Path, seconds: u32, sample_rate: u32) {
    let count = (seconds * sample_rate) as usize;
    let mut wav = Vec::with_capacity(44 + count * 2);
    let data_bytes = (count * 2) as u32;
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for i in 0..count {
        let t = i as f32 / sample_rate as f32;
        let sample = (0.5 * 32767.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, wav).unwrap();
}
