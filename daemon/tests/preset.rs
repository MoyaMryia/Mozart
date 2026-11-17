//! 模型预设（SPEC-11）集成测试：首次自动建、save/use/list、模型切换、
//! 文件与实时跨链、损坏预设与 NotReady。桩经 MOZART_STUB_LOG 记录命令与
//! StartStream 参数体；MOZART_RVC_STUB_MODELS 提供多模型注册表。

mod common;

use common::{
    call, example, file_job, status, submit_file, wait_job_state, wait_ready, wait_until,
    DaemonProcess,
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
        Self::build(tag, models, config_model, None, &[])
    }

    /// rvc 启动即已加载 `preload` 模型（不起流）：daemon 未经历预设载入，
    /// 参数来源记录为空。
    fn new_preloaded(tag: &str, models: &[&str], config_model: &str, preload: &str) -> Self {
        Self::build(tag, models, config_model, Some(preload), &[])
    }

    /// 给 rvc 桩注入失败旋钮（StartStream 失败、文件批处理延迟等）。
    fn new_with_env(tag: &str, models: &[&str], config_model: &str, env: &[(&str, &str)]) -> Self {
        Self::build(tag, models, config_model, None, env)
    }

    fn build(
        tag: &str,
        models: &[&str],
        config_model: &str,
        preload: Option<&str>,
        env: &[(&str, &str)],
    ) -> Self {
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
        for (key, value) in env {
            text.push_str(&format!("export {key}='{value}'\n"));
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

    fn model_use(&self, model_id: &str) -> Response {
        let body = json!({"model_id": model_id}).to_string();
        self.call(ctl::DAEMON_MODEL_USE, body.as_bytes())
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(self.daemon.dir.join("config.toml")).unwrap()
    }

    /// 给目标模型写一份与旧 live 参数可区分的预设（模型目录须已存在）。
    fn write_preset(&self, model: &str, pitch: i32) {
        std::fs::create_dir_all(self.models.join(model)).unwrap();
        let mut values = mozart_core::config::RvcSessionValues::default();
        values.pitch_shift = pitch;
        mozart_core::preset::write_default(&self.models.join(model), &values).unwrap();
    }

    /// rvc 命令按下发顺序排列，供断言 Stop→Load→Start 次序与恢复所用参数。
    fn rvc_commands(&self) -> Vec<(u16, Value)> {
        self.entries()
            .into_iter()
            .filter(|entry| entry.stub == "mozart-rvc-stub")
            .map(|entry| (entry.type_, entry.body))
            .collect()
    }

    /// 实时载入 model-a、把 pitch 调成 3 并存回 A 预设：旧参数 3 与目标预设
    /// 参数 7 可区分，恢复断言不会退化成自我比较。
    fn load_a_with_pitch3(&self) {
        self.set_route_rvc();
        expect_ok(self.config_set("rvc.pitch_shift", "3"));
        apply_data(self.preset_save());
        self.wait_streaming();
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

#[test]
fn model_use_loads_target_preset_and_rejects_corrupt_preset() {
    let h = Harness::new("model-use", &["model-a", "model-b", "model-c"], "model-a");
    let mut b_vals = mozart_core::config::RvcSessionValues::default();
    b_vals.pitch_shift = 7;
    mozart_core::preset::write_default(&h.models.join("model-b"), &b_vals).unwrap();
    let bad_dir = h.models.join("model-c").join("presets");
    std::fs::create_dir_all(&bad_dir).unwrap();
    std::fs::write(bad_dir.join("default.toml"), "not valid toml = [").unwrap();

    h.set_route_rvc();
    h.clear_log();

    let reply = h.call(
        ctl::DAEMON_MODEL_USE,
        &serde_json::to_vec(&mozart_core::ctl::schema::ModelUseRequest {
            model_id: "model-b".to_string(),
        })
        .unwrap(),
    );
    expect_ok(reply);
    let starts = h.stream_starts("realtime");
    assert_eq!(starts.len(), 1, "model use 应只起一次流：{starts:?}");
    assert_eq!(starts[0]["pitch_shift"], 7);
    assert_eq!(h.rvc_params().pitch_shift, 7);
    let listed: mozart_core::ctl::schema::PresetListData =
        serde_json::from_value(expect_ok(h.preset_list())).unwrap();
    assert_eq!(listed.model_id, "model-b");
    assert!(!listed.dirty);
    let get_b = expect_ok(h.call(
        ctl::DAEMON_CONFIG_GET,
        br#"{"key":"rvc.model_id"}"#,
    ));
    assert_eq!(get_b["value"], "model-b");

    // 损坏预设在切流前拦截，不打断当前 model-b。
    let bad_reply = h.call(
        ctl::DAEMON_MODEL_USE,
        &serde_json::to_vec(&mozart_core::ctl::schema::ModelUseRequest {
            model_id: "model-c".to_string(),
        })
        .unwrap(),
    );
    expect_error(bad_reply, ctl::CODE_INTERNAL);
    assert_eq!(h.rvc_params().pitch_shift, 7);
    let get_after_bad = expect_ok(h.call(
        ctl::DAEMON_CONFIG_GET,
        br#"{"key":"rvc.model_id"}"#,
    ));
    assert_eq!(get_after_bad["value"], "model-b");
}

/// 断言失败切换后的恢复序列：目标 LoadModel 之后重新 LoadModel 旧模型，再按
/// 旧参数起实时流。恢复用目标预设参数会让旧模型跑新模型的音高，必须拦住。
fn assert_restored_old_stream(
    commands: &[(u16, Value)],
    old_model: &str,
    target_model: &str,
    old_pitch: i64,
) {
    let target_load = commands
        .iter()
        .position(|(type_, body)| *type_ == ctl::RVC_LOAD_MODEL && body["model_id"] == target_model)
        .expect("应尝试加载目标模型");
    let restore_load = commands[target_load + 1..]
        .iter()
        .position(|(type_, body)| *type_ == ctl::RVC_LOAD_MODEL && body["model_id"] == old_model)
        .map(|index| target_load + 1 + index)
        .unwrap_or_else(|| panic!("恢复应重新加载旧模型 {old_model}：{commands:?}"));
    let restore_start = commands[restore_load + 1..]
        .iter()
        .find(|(type_, body)| *type_ == ctl::RVC_START_STREAM && body["kind"] == "realtime")
        .unwrap_or_else(|| panic!("恢复应重起实时流：{commands:?}"));
    assert_eq!(
        restore_start.1["pitch_shift"], old_pitch,
        "恢复必须用旧模型旧参数：{:?}",
        restore_start.1
    );
}

/// LoadModel 失败：恢复的流用旧模型旧参数，生效配置、五键基线与 model_id
/// 基线都不变，applied 仍属旧模型（preset list/save 可证）。
#[test]
fn model_use_load_failure_restores_old_model_and_old_params() {
    // 桩注册表只有 model-a：model-b 目录与预设存在，但 LoadModel 必失败。
    let h = Harness::new("use-load-fail", &["model-a"], "model-a");
    h.load_a_with_pitch3();
    h.write_preset("model-b", 7);
    let baseline_before = h.config_text();
    let live_before = h.rvc_params();
    let preset_b_before = std::fs::read_to_string(h.preset_path("model-b")).unwrap();
    h.clear_log();

    let reason = expect_error(h.model_use("model-b"), ctl::CODE_INVALID_ARGUMENT);
    assert!(reason.contains("model not found"), "{reason}");

    let commands = h.rvc_commands();
    assert_restored_old_stream(&commands, "model-a", "model-b", 3);
    assert!(
        commands
            .iter()
            .filter(|(type_, _)| *type_ == ctl::RVC_START_STREAM)
            .all(|(_, body)| body["pitch_shift"] != 7),
        "加载失败后任何起流都不得带目标预设参数：{commands:?}"
    );

    h.wait_streaming();
    assert_eq!(h.rvc_params(), live_before, "恢复后生效配置仍是旧参数");
    assert_eq!(h.config_text(), baseline_before, "五键与 model_id 基线都不得变");
    assert_eq!(
        std::fs::read_to_string(h.preset_path("model-b")).unwrap(),
        preset_b_before,
        "目标模型预设不得被改动"
    );
    let list = list_data(h.preset_list());
    assert_eq!(list.model_id, "model-a", "applied 不得改成目标模型");
    assert_eq!(list.live_values.pitch_shift, 3);
    assert!(!list.dirty, "live 应仍等于 A 的预设：{list:?}");
    assert_eq!(apply_data(h.preset_save()).values.pitch_shift, 3);
}

/// StartStream 失败：提交已落盘，必须整体回退（五键基线、model_id 基线、
/// 生效配置、applied），再用旧模型旧参数恢复流。
#[test]
fn model_use_start_failure_rolls_back_and_restores_old_params() {
    let h = Harness::new_with_env(
        "use-start-fail",
        &["model-a", "model-b"],
        "model-a",
        &[("MOZART_RVC_STUB_FAIL_START_PITCH", "7")],
    );
    h.load_a_with_pitch3();
    h.write_preset("model-b", 7);
    let baseline_before = h.config_text();
    let live_before = h.rvc_params();
    h.clear_log();

    let reason = expect_error(h.model_use("model-b"), ctl::CODE_INFERENCE_FAILED);
    assert!(reason.contains("model use start stream"), "{reason}");

    let commands = h.rvc_commands();
    assert!(
        commands
            .iter()
            .any(|(type_, body)| *type_ == ctl::RVC_START_STREAM && body["pitch_shift"] == 7),
        "应确实按目标预设参数尝试起流：{commands:?}"
    );
    assert_restored_old_stream(&commands, "model-a", "model-b", 3);

    h.wait_streaming();
    assert_eq!(h.rvc_params(), live_before, "回退后生效配置仍是旧参数");
    assert_eq!(
        h.config_text(),
        baseline_before,
        "五键与 model_id 基线都应回退到提交前"
    );
    let list = list_data(h.preset_list());
    assert_eq!(list.model_id, "model-a", "applied 应回退到旧模型");
    assert!(!list.dirty, "live 应仍等于 A 的预设：{list:?}");
    assert_eq!(apply_data(h.preset_save()).values.pitch_shift, 3);
}

/// 起流与恢复都失败：返回组合错误，并留 status 可查询的持久降级原因；
/// 基线与 applied 仍整体回退，不静默吞掉恢复失败。
#[test]
fn model_use_restore_failure_records_queryable_degraded() {
    // setup 恰好起流两次（route set rvc、config set pitch）；此后全部失败。
    let h = Harness::new_with_env(
        "use-restore-fail",
        &["model-a", "model-b"],
        "model-a",
        &[("MOZART_RVC_STUB_FAIL_START_AFTER", "2")],
    );
    h.load_a_with_pitch3();
    h.write_preset("model-b", 7);
    assert_eq!(
        h.rvc_count(ctl::RVC_START_STREAM),
        2,
        "失败旋钮按起流次数计数，setup 次数必须确定"
    );
    let baseline_before = h.config_text();
    let live_before = h.rvc_params();
    h.clear_log();

    let reason = expect_error(h.model_use("model-b"), ctl::CODE_INFERENCE_FAILED);
    assert!(
        reason.contains("model use start stream") && reason.contains("restore with old params"),
        "错误应组合起流与恢复两段失败：{reason}"
    );
    assert_restored_old_stream(&h.rvc_commands(), "model-a", "model-b", 3);
    assert!(
        wait_until(POLL_TIMEOUT, || {
            h.status().degraded.iter().any(|entry| {
                entry.contains("rvc model use") && entry.contains("restore with old params")
            })
        }),
        "恢复失败必须留可查询降级原因：{:?}",
        h.status().degraded
    );
    assert_eq!(h.rvc_params(), live_before, "回退后生效配置仍是旧参数");
    assert_eq!(h.config_text(), baseline_before, "基线应回退到提交前");
    let list = list_data(h.preset_list());
    assert_eq!(list.model_id, "model-a", "applied 应回退到旧模型");
    assert!(!list.dirty);
}

/// 成功切换：只起一次新流（提交时流已停，编排不二次起停），五键基线与
/// model_id 基线都已写，applied 为目标模型，preset list 不 dirty。
#[test]
fn model_use_success_commits_baseline_and_starts_one_stream() {
    let h = Harness::new("use-success", &["model-a", "model-b"], "model-a");
    h.load_a_with_pitch3();
    h.write_preset("model-b", 7);
    h.clear_log();

    let data = expect_ok(h.model_use("model-b"));
    assert_eq!(data["model_id"], "model-b");
    assert_eq!(data["previous_model_id"], "model-a");

    let commands = h.rvc_commands();
    let stop = commands
        .iter()
        .position(|(type_, _)| *type_ == ctl::RVC_STOP_STREAM)
        .expect("断流切换应先停流");
    let load = commands
        .iter()
        .position(|(type_, body)| *type_ == ctl::RVC_LOAD_MODEL && body["model_id"] == "model-b")
        .expect("应加载目标模型");
    let starts: Vec<&Value> = commands
        .iter()
        .filter(|(type_, body)| *type_ == ctl::RVC_START_STREAM && body["kind"] == "realtime")
        .map(|(_, body)| body)
        .collect();
    assert_eq!(starts.len(), 1, "成功路径只起一次流：{commands:?}");
    assert_eq!(starts[0]["pitch_shift"], 7, "起流应带目标预设参数");
    assert!(stop < load, "必须先停流再加载：{commands:?}");
    assert_eq!(
        h.rvc_count(ctl::RVC_STOP_STREAM),
        1,
        "不得 Stop/Start 两轮：{commands:?}"
    );
    assert_eq!(
        h.rvc_count(ctl::RVC_LOAD_MODEL),
        1,
        "起流复用 LoadModel 的 handle，不重复加载：{commands:?}"
    );

    h.wait_streaming();
    assert_eq!(h.rvc_params().pitch_shift, 7);
    let config = h.config_text();
    assert!(config.contains("pitch_shift = 7"), "五键基线应已写：{config}");
    assert!(
        config.contains("model_id = \"model-b\""),
        "model_id 基线应已写：{config}"
    );
    let list = list_data(h.preset_list());
    assert_eq!(list.model_id, "model-b");
    assert!(!list.dirty, "载入后 live 应等于目标预设：{list:?}");
    let saved = apply_data(h.preset_save());
    assert_eq!(saved.model_id, "model-b", "applied 应为目标模型");
    assert_eq!(saved.values.pitch_shift, 7);
}

/// 预设提交失败（TOML 基线被外部改坏，重建生效配置读不回来）：LoadModel 已
/// 成功但五键与 applied 都未落盘，恢复仍用旧模型旧参数，live 与 applied 不变。
#[test]
fn model_use_commit_failure_restores_old_stream() {
    let h = Harness::new("use-commit-fail", &["model-a", "model-b"], "model-a");
    h.load_a_with_pitch3();
    h.write_preset("model-b", 7);
    let live_before = h.rvc_params();
    // SPEC-04 允许用户手改 TOML：改坏后提交阶段重建生效配置必失败。
    std::fs::write(h.daemon.dir.join("config.toml"), "not valid toml = [").unwrap();
    h.clear_log();

    let reason = expect_error(h.model_use("model-b"), ctl::CODE_INTERNAL);
    assert!(reason.contains("model use preset commit"), "{reason}");
    assert_restored_old_stream(&h.rvc_commands(), "model-a", "model-b", 3);

    h.wait_streaming();
    assert_eq!(h.rvc_params(), live_before, "提交失败不得改 live 参数");
    let list = list_data(h.preset_list());
    assert_eq!(list.model_id, "model-a", "applied 不得改成目标模型");
    assert!(!list.dirty, "live 应仍等于 A 的预设：{list:?}");
    assert_eq!(apply_data(h.preset_save()).values.pitch_shift, 3);
}

/// 文件任务占 GPU：拒绝切换（CODE_BUSY），不碰流、参数与基线。
#[test]
fn model_use_rejected_while_file_job_owns_gpu() {
    let h = Harness::new_with_env(
        "use-busy",
        &["model-a", "model-b"],
        "model-a",
        &[("MOZART_RVC_STUB_FILE_DELAY_MS", "2000")],
    );
    h.run_file("a.wav", "model-a");
    expect_ok(h.config_set("rvc.pitch_shift", "3"));
    apply_data(h.preset_save());
    h.write_preset("model-b", 7);
    let baseline_before = h.config_text();

    let job_id = h.submit("busy.wav", "model-a");
    assert!(
        wait_until(JOB_TIMEOUT, || {
            file_job(&h.conn, &job_id).progress.stage == "infer"
        }),
        "任务应进入 infer 段占住 GPU"
    );
    h.clear_log();
    let reason = expect_error(h.model_use("model-b"), ctl::CODE_BUSY);
    assert!(reason.contains("file job owns GPU"), "{reason}");
    assert!(
        h.stream_starts("realtime").is_empty(),
        "拒绝切换不得起实时流：{:?}",
        h.entries()
    );
    assert_eq!(h.rvc_params().pitch_shift, 3);
    assert_eq!(h.config_text(), baseline_before);

    let job = wait_job_state(&h.conn, &job_id, FileJobState::Completed, JOB_TIMEOUT);
    assert!(job.result_ready, "被拒的切换不得影响文件任务：{job:?}");
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
