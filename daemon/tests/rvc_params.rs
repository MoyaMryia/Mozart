//! RVC 会话参数运行时编排（SPEC-04）：`config set rvc.<会话参数>` 的 Stop/Start
//! 决策、失败回退与 status.rvc_params 展示。桩经 MOZART_STUB_LOG 记录命令与
//! StartStream 参数体；失败恢复用桩的 StartStream 失败旋钮，不做调用 mock。

mod common;

use common::{
    call, example, file_job, status, submit_file, wait_job_state, wait_ready, wait_until,
    DaemonProcess,
};
use mozart_core::ctl::schema::{FileJobState, Response, Route, RvcParams, Status};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONFIG: &str = "[daemon]\nrpc_timeout_ms = 1000\n\n[rvc]\nmodel_id = \"demo\"\n";

/// 状态收敛与任务等待上限（轮询间隔 100 ms，桩起停无延迟）。
const POLL_TIMEOUT: Duration = Duration::from_secs(5);
const JOB_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
struct LogEntry {
    stub: String,
    type_: u16,
    body: Value,
}

/// 解析 `{桩名} {命令号} {请求体}` 日志行。
fn parse_entry(line: &str) -> Option<LogEntry> {
    let mut parts = line.splitn(3, ' ');
    let stub = parts.next()?.to_string();
    let type_ = parts.next()?.parse::<u16>().ok()?;
    let body = serde_json::from_str(parts.next()?).ok()?;
    Some(LogEntry { stub, type_, body })
}

struct Harness {
    _daemon: DaemonProcess,
    conn: Seqpacket,
    log: PathBuf,
    dir: PathBuf,
}

impl Harness {
    /// wrapper 给 rvc 桩注入命令日志与失败旋钮；其余桩不记录。
    fn new(tag: &str, env: &[(&str, &str)]) -> Self {
        let dir =
            std::env::temp_dir().join(format!("mozart-rvc-params-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("commands.log");
        let script = dir.join("rvc.sh");
        let mut text = format!("#!/bin/sh\nexport MOZART_STUB_LOG='{}'\n", log.display());
        for (key, value) in env {
            text.push_str(&format!("export {key}='{value}'\n"));
        }
        text.push_str(&format!("exec '{}' \"$@\"\n", example("rvc_stub").display()));
        std::fs::write(&script, text).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let daemon = DaemonProcess::start_with_config(tag, &example("pre_stub"), &script, CONFIG);
        let conn = daemon.connect();
        wait_ready(&conn);
        Self {
            _daemon: daemon,
            conn,
            log,
            dir,
        }
    }

    fn call(&self, type_: u16, body: &[u8]) -> Response {
        call(&self.conn, type_, body)
    }

    fn config_set(&self, key: &str, value: &str) -> Response {
        let body = json!({"key": key, "value": value}).to_string();
        self.call(ctl::DAEMON_CONFIG_SET, body.as_bytes())
    }

    fn status(&self) -> Status {
        status(&self.conn)
    }

    fn rvc_params(&self) -> RvcParams {
        self.status().rvc_params.expect("status 应带 rvc_params")
    }

    fn clear_log(&self) {
        std::fs::write(&self.log, "").unwrap();
    }

    fn entries(&self) -> Vec<LogEntry> {
        read_entries(&self.log)
    }

    fn rvc_count(&self, type_: u16) -> usize {
        self.entries()
            .iter()
            .filter(|entry| entry.stub == "mozart-rvc-stub" && entry.type_ == type_)
            .count()
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

    fn set_route_rvc(&self) {
        let reply = self.call(ctl::DAEMON_ROUTE_SET, br#"{"route":"rvc"}"#);
        assert_eq!(reply.status, "ok", "route set rvc: {}", reply.reason);
        self.wait_streaming();
    }

    /// 实时链路就绪：rvc 流在跑、pre 已切到 rvc。
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

/// 实时流在跑、改实时链路参数：StopStream → StartStream 带新值；生效配置与
/// status.rvc_params 更新，响应维持 restart_required=false。
#[test]
fn pitch_shift_change_restarts_realtime_stream_with_new_value() {
    let h = Harness::new("pitch", &[]);
    h.set_route_rvc();
    assert_eq!(h.rvc_params().pitch_shift, 0);
    h.clear_log();

    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(reply.data["key"], "rvc.pitch_shift");
    assert_eq!(reply.data["value"], "3");
    assert_eq!(reply.data["restart_required"], false);
    assert!(
        reply.data["affected"].as_array().is_some_and(Vec::is_empty),
        "会话参数不应要求重启：{:?}",
        reply.data["affected"]
    );

    let entries = h.entries();
    let stop = entries
        .iter()
        .position(|entry| entry.stub == "mozart-rvc-stub" && entry.type_ == ctl::RVC_STOP_STREAM)
        .expect("改实时参数应先下发 StopStream");
    let (start_index, start) = entries
        .iter()
        .enumerate()
        .find(|(_, entry)| {
            entry.stub == "mozart-rvc-stub"
                && entry.type_ == ctl::RVC_START_STREAM
                && entry.body["kind"] == "realtime"
        })
        .map(|(index, entry)| (index, &entry.body))
        .expect("StopStream 后应重建实时流");
    assert!(stop < start_index, "必须先停流再起流：{entries:?}");
    assert_eq!(start["pitch_shift"], 3, "StartStream 应带新值：{start}");
    assert_eq!(start["index_rate"], 0.0);
    assert_eq!(start["rms_mix_rate"], 1.0);
    assert_eq!(start["protect"], 0.33);
    assert_eq!(start["f0_method"], "rmvpe");

    assert_eq!(h.rvc_params().pitch_shift, 3);
    h.wait_streaming();
}

/// 仅改 protect / rms_mix_rate：实时链路不应用，不触发 Stop/Start、不打断流。
#[test]
fn file_only_params_do_not_restart_realtime_stream() {
    let h = Harness::new("soft", &[]);
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.protect", "0.4");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(h.rvc_params().protect, 0.4);

    let reply = h.config_set("rvc.rms_mix_rate", "0.5");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(h.rvc_params().rms_mix_rate, 0.5);

    // 跨过至少一轮轮询：流仍在跑且没有任何停/起命令。
    std::thread::sleep(Duration::from_millis(250));
    let status = h.status();
    assert_eq!(
        status.rvc.as_ref().map(|rvc| rvc.state.as_str()),
        Some("streaming"),
        "改文件链路参数不得打断实时流"
    );
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0);
    assert!(h.stream_starts("realtime").is_empty());
}

/// 参数值未变化：不触发 Stop/Start。
#[test]
fn unchanged_value_does_not_restart_realtime_stream() {
    let h = Harness::new("same", &[]);
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.pitch_shift", "0");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    std::thread::sleep(Duration::from_millis(250));

    assert_eq!(h.rvc_params().pitch_shift, 0);
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0);
    assert!(h.stream_starts("realtime").is_empty());
    h.wait_streaming();
}

/// 新参起流失败：用旧参恢复实时流并返回错误；生效配置保留旧值，TOML 保留
/// 新基线。不自动重试：等下一次对应键写入或 daemon 重启再应用。
#[test]
fn start_failure_restores_old_params_and_keeps_toml_baseline() {
    let h = Harness::new("restore", &[("MOZART_RVC_STUB_FAIL_START_PITCH", "3")]);
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "error", "新参起流失败必须报错");
    assert_eq!(reply.code, "InferenceFailed");
    assert!(
        reply.reason.contains("new params"),
        "错误应指明新参起流失败：{}",
        reply.reason
    );

    // 旧参恢复实时流：流继续在跑，status 显示旧值。
    h.wait_streaming();
    assert_eq!(h.rvc_params().pitch_shift, 0, "失败后生效配置保留旧值");
    let starts = h.stream_starts("realtime");
    assert!(
        starts.iter().any(|body| body["pitch_shift"] == 3),
        "应先按新参起流：{starts:?}"
    );
    assert!(
        starts.iter().any(|body| body["pitch_shift"] == 0),
        "失败后应用旧参恢复：{starts:?}"
    );

    // TOML 保留新基线，等待下一次对应键写入或 daemon 重启应用。
    let reply = h.call(ctl::DAEMON_CONFIG_GET, br#"{"key":"rvc.pitch_shift"}"#);
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(reply.data["value"], 3);
}

/// 存在待生效的 pitch 基线时只改 rvc.protect：触发判据只看当前设置的键，
/// 不 Stop/Start、不打断实时流，生效 pitch 仍是旧值；之后写 pitch 键才按
/// TOML 新基线重建实时流。
#[test]
fn protect_write_with_pending_pitch_baseline_keeps_stream() {
    let h = Harness::new("pending", &[("MOZART_RVC_STUB_FAIL_START_PITCH", "3")]);
    h.set_route_rvc();
    // 制造待生效基线：pitch=3 起流失败，旧参恢复，TOML 保留 3。
    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "error", "新参起流失败必须报错");
    h.wait_streaming();
    h.clear_log();

    let reply = h.config_set("rvc.protect", "0.4");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);

    // 跨过至少一轮轮询：protect 只更新生效配置，未生效的 pitch 基线不混入。
    std::thread::sleep(Duration::from_millis(250));
    let params = h.rvc_params();
    assert_eq!(params.protect, 0.4);
    assert_eq!(params.pitch_shift, 0, "未生效的 pitch 基线不得混入生效配置");
    assert_eq!(
        h.status().rvc.as_ref().map(|rvc| rvc.state.as_str()),
        Some("streaming"),
        "protect 写入不得打断实时流"
    );
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0, "protect 不得触发 StopStream");
    assert!(
        h.stream_starts("realtime").is_empty(),
        "protect 不得触发 StartStream：{:?}",
        h.stream_starts("realtime")
    );

    // 下一次对应键写入（pitch）重读基线：pitch=3 再次尝试并成功（桩只失败一次）。
    h.clear_log();
    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    h.wait_streaming();
    let params = h.rvc_params();
    assert_eq!(params.pitch_shift, 3);
    assert_eq!(params.protect, 0.4);
    let starts = h.stream_starts("realtime");
    assert!(!starts.is_empty(), "写 pitch 键应重建实时流：{starts:?}");
    assert!(
        starts
            .iter()
            .all(|body| body["pitch_shift"] == 3 && body["protect"] == 0.4),
        "所有 StartStream 都应带新值：{starts:?}"
    );
}

/// 新参与旧参起流都失败：返回组合错误并留下可查询的持久降级原因。
#[test]
fn failed_restore_returns_combined_error_and_records_degraded() {
    // 第一次 StartStream（route set rvc）成功，之后全部失败。
    let h = Harness::new("degraded", &[("MOZART_RVC_STUB_FAIL_START_AFTER", "1")]);
    h.set_route_rvc();

    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "error", "起流与恢复都失败必须报错");
    assert!(
        reply.reason.contains("new params") && reply.reason.contains("old params"),
        "错误应组合新参与旧参两段失败：{}",
        reply.reason
    );
    assert!(
        reply.reason.contains("TOML baseline kept"),
        "错误应提示 TOML 已保留新基线及恢复方式：{}",
        reply.reason
    );
    assert_eq!(h.rvc_params().pitch_shift, 0, "失败后生效配置保留旧值");
    assert!(
        wait_until(POLL_TIMEOUT, || {
            h.status()
                .degraded
                .iter()
                .any(|reason| reason.contains("rvc session params"))
        }),
        "恢复失败必须留可查询降级原因：{:?}",
        h.status().degraded
    );
}

/// 停流未生效（StopStream 应答成功但流仍在）：新参起流失败，旧参恢复遇
/// Busy 被视为已恢复，但在跑参数不确定，留可查询的持久降级原因。
#[test]
fn recovery_busy_records_unknown_running_params() {
    let h = Harness::new("busyrestore", &[("MOZART_RVC_STUB_STOP_KEEP_STREAM", "1")]);
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "error", "流未停时新参无法起流，必须报错");
    assert!(
        reply.reason.contains("restore busy") && reply.reason.contains("running params unknown"),
        "错误应说明恢复遇 Busy、在跑参数未知：{}",
        reply.reason
    );
    assert_eq!(h.rvc_params().pitch_shift, 0, "失败后生效配置保留旧值");
    assert!(
        wait_until(POLL_TIMEOUT, || {
            h.status()
                .degraded
                .iter()
                .any(|reason| reason.contains("recovery busy; running params unknown"))
        }),
        "恢复遇 Busy 必须留可查询降级原因：{:?}",
        h.status().degraded
    );
}

/// rvc 不可达：无法确认实时流，不打断，生效配置更新并留可查询降级原因。
#[test]
fn rvc_unreachable_updates_config_and_records_degraded() {
    let dir =
        std::env::temp_dir().join(format!("mozart-rvc-params-{}-statusdown", std::process::id()));
    let marker = dir.join("status.fail");
    let h = Harness::new(
        "statusdown",
        &[(
            "MOZART_RVC_STUB_STATUS_FAIL_FILE",
            marker.to_str().unwrap(),
        )],
    );
    h.set_route_rvc();
    h.clear_log();

    // 标记文件出现后 GetStatus 回错，模拟 rvc 不可达。
    std::fs::write(&marker, "").unwrap();
    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(
        reply.status, "ok",
        "rvc 不可达时只更新生效配置：{}",
        reply.reason
    );
    assert_eq!(h.rvc_params().pitch_shift, 3, "生效配置应更新");
    assert!(
        h.status().degraded.iter().any(|reason| reason
            .contains("rvc status unavailable; config updated, stream not verified")),
        "rvc 不可达必须留可查询降级原因：{:?}",
        h.status().degraded
    );
    assert_eq!(
        h.rvc_count(ctl::RVC_STOP_STREAM),
        0,
        "rvc 不可达时不得下发 StopStream"
    );
}

/// direct/空闲：不建流，只更新生效配置；下一次建流（route set rvc）带新值。
#[test]
fn direct_route_only_updates_effective_config() {
    let h = Harness::new("direct", &[]);

    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(h.status().route.active, Route::Direct);
    assert_eq!(h.rvc_params().pitch_shift, 3);
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0);
    assert!(
        h.stream_starts("realtime").is_empty(),
        "direct 不应建流：{:?}",
        h.stream_starts("realtime")
    );

    h.set_route_rvc();
    // 不做精确计数：轮询对账也可能补起流；要求所有 StartStream 都带新值。
    let starts = h.stream_starts("realtime");
    assert!(!starts.is_empty(), "下一次建流应下发 StartStream：{starts:?}");
    assert!(
        starts.iter().all(|body| body["pitch_shift"] == 3),
        "所有 StartStream 都应带新值：{starts:?}"
    );
}

/// 文件任务运行中：不中断、不向 rvc 发 Stop/Start；当前任务用旧快照，
/// 下一任务读到新配置。
#[test]
fn file_job_keeps_running_and_next_job_uses_new_params() {
    let dir =
        std::env::temp_dir().join(format!("mozart-rvc-file-params-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("commands.log");
    let storage = dir.join("storage");

    // 桩攒满帧后等 3 s 再回帧：config set 稳定落在 infer 段（任务占用 GPU）。
    let daemon = DaemonProcess::start_file(
        "rvc-file-params",
        &storage,
        "",
        true,
        &[
            ("MOZART_STUB_LOG", log.display().to_string()),
            ("MOZART_RVC_STUB_FILE_DELAY_MS", "3000".to_string()),
        ],
    );
    let conn = daemon.connect();
    wait_ready(&conn);

    let source = daemon.dir.join("tone.wav");
    write_wav(&source, 1, 16000);
    let reply = submit_file(&conn, &source, Some("demo"));
    assert_eq!(reply.status, "ok", "submit: {}", reply.reason);
    let job_id = reply.data["job_id"].as_str().unwrap().to_string();
    assert!(
        wait_until(JOB_TIMEOUT, || {
            file_job(&conn, &job_id).progress.stage == "infer"
        }),
        "任务应进入 infer 段"
    );

    // 当前任务在启动时取配置快照：它的 file StartStream 已带旧值 pitch=0。
    let first_start = read_entries(&log)
        .into_iter()
        .find(|entry| {
            entry.stub == "mozart-rvc-stub"
                && entry.type_ == ctl::RVC_START_STREAM
                && entry.body["kind"] == "file"
        })
        .expect("当前任务应有 file StartStream");
    assert_eq!(
        first_start.body["pitch_shift"], 0,
        "当前任务使用启动时快照（旧值）：{:?}",
        first_start.body
    );

    // 文件起流已发生：只观察此后的命令。
    std::fs::write(&log, "").unwrap();
    let body = json!({"key": "rvc.pitch_shift", "value": "3"}).to_string();
    let reply = call(&conn, ctl::DAEMON_CONFIG_SET, body.as_bytes());
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);

    // 运行中立即检查：不得停流/重建流。
    let entries = read_entries(&log);
    assert!(
        !entries.iter().any(|entry| {
            entry.stub == "mozart-rvc-stub" && entry.type_ == ctl::RVC_STOP_STREAM
        }),
        "文件任务运行中不得下发 StopStream：{entries:?}"
    );
    assert!(
        !entries.iter().any(|entry| {
            entry.stub == "mozart-rvc-stub"
                && entry.type_ == ctl::RVC_START_STREAM
                && entry.body["kind"] == "realtime"
        }),
        "文件任务运行中不得重建实时流：{entries:?}"
    );
    // status 显示新生效配置；当前任务仍用启动时快照（pitch=0），
    // 下一任务启动时取新快照（SPEC-04）。
    let params = status(&conn).rvc_params.expect("status 应带 rvc_params");
    assert_eq!(params.pitch_shift, 3);

    let job = wait_job_state(&conn, &job_id, FileJobState::Completed, JOB_TIMEOUT);
    assert!(job.result_ready, "文件任务应正常完成：{job:?}");

    // 下一任务在启动时取新配置快照：file StartStream 带新值。
    let source = daemon.dir.join("next.wav");
    write_wav(&source, 1, 16000);
    let reply = submit_file(&conn, &source, Some("demo"));
    assert_eq!(reply.status, "ok", "submit: {}", reply.reason);
    let next_id = reply.data["job_id"].as_str().unwrap().to_string();
    let next = wait_job_state(&conn, &next_id, FileJobState::Completed, JOB_TIMEOUT);
    assert!(next.result_ready, "第二任务应正常完成：{next:?}");

    let file_starts: Vec<LogEntry> = read_entries(&log)
        .into_iter()
        .filter(|entry| {
            entry.stub == "mozart-rvc-stub"
                && entry.type_ == ctl::RVC_START_STREAM
                && entry.body["kind"] == "file"
        })
        .collect();
    let second = file_starts.last().expect("第二任务应有 file StartStream");
    assert_eq!(
        second.body["pitch_shift"], 3,
        "下一任务应读取新配置快照：{:?}",
        second.body
    );

    drop(conn);
    drop(daemon);
    let _ = std::fs::remove_dir_all(&dir);
}

fn read_entries(path: &Path) -> Vec<LogEntry> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(parse_entry)
        .collect()
}

/// 生成 PCM16 单声道正弦 WAV（测试源，留在临时目录不入库；同 file_worker 用例）。
fn write_wav(path: &Path, seconds: u32, sample_rate: u32) {
    let count = (seconds * sample_rate) as usize;
    let mut wav = Vec::with_capacity(44 + count * 2);
    let data_bytes = (count * 2) as u32;
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // 单声道
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
