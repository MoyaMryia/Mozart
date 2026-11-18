//! RVC 会话参数运行时编排（SPEC-04）：`config set rvc.<会话参数>` 的热更新（`SetParams`）
//! 与结构旋钮 Stop/Start 决策、失败回退与 status.rvc_params 展示。桩经 MOZART_STUB_LOG
//! 记录命令与请求体；失败恢复用桩的失败旋钮，不做调用 mock。

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

    fn set_params_calls(&self) -> Vec<Value> {
        self.entries()
            .into_iter()
            .filter(|entry| entry.stub == "mozart-rvc-stub" && entry.type_ == ctl::RVC_SET_PARAMS)
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

/// 实时流在跑、改可热更参数与 `skip_silence`：下发 `SetParams` 在块边界生效，
/// 不触发 `StopStream`→`StartStream`；生效配置与 status.rvc_params 更新，
/// 响应维持 restart_required=false。
#[test]
fn pitch_shift_and_skip_silence_update_via_set_params_without_restarting() {
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

    let reply = h.config_set("rvc.realtime.skip_silence", "false");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);

    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0, "热更新不得停流");
    assert!(
        h.stream_starts("realtime").is_empty(),
        "热更新不得重起实时流"
    );
    let calls = h.set_params_calls();
    assert_eq!(calls.len(), 2, "两次热更新应各自下发一次 SetParams：{calls:?}");
    assert_eq!(calls[0]["pitch_shift"], 3);
    assert_eq!(calls[0]["index_rate"], 0.0);
    assert_eq!(calls[0]["rms_mix_rate"], 1.0);
    assert_eq!(calls[0]["protect"], 0.33);
    assert_eq!(calls[0]["f0_method"], "rmvpe");
    assert_eq!(calls[0]["skip_silence"], true);
    assert_eq!(calls[1]["pitch_shift"], 3);
    assert_eq!(calls[1]["skip_silence"], false);

    assert_eq!(h.rvc_params().pitch_shift, 3);
    h.wait_streaming();
}

/// 仅改 protect / rms_mix_rate：实时链路不应用，不触发 SetParams 或 Stop/Start，不打断流。
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

    // 跨过至少一轮轮询：流仍在跑且没有任何热更/停/起命令。
    std::thread::sleep(Duration::from_millis(250));
    let status = h.status();
    assert_eq!(
        status.rvc.as_ref().map(|rvc| rvc.state.as_str()),
        Some("streaming"),
        "改文件链路参数不得打断实时流"
    );
    assert_eq!(h.rvc_count(ctl::RVC_SET_PARAMS), 0);
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0);
    assert!(h.stream_starts("realtime").is_empty());
}

/// 参数值未变化：不触发 SetParams 或 Stop/Start。
#[test]
fn unchanged_value_does_not_restart_realtime_stream() {
    let h = Harness::new("same", &[]);
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.pitch_shift", "0");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    std::thread::sleep(Duration::from_millis(250));

    assert_eq!(h.rvc_params().pitch_shift, 0);
    assert_eq!(h.rvc_count(ctl::RVC_SET_PARAMS), 0);
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0);
    assert!(h.stream_starts("realtime").is_empty());
    h.wait_streaming();
}

/// SetParams 失败：流不断、生效配置保留旧值、TOML 保留新基线；
/// 存在待生效 pitch 基线时改 protect 不误发 SetParams，再写 pitch 才重试成功。
#[test]
fn set_params_failure_keeps_stream_and_pending_baseline_isolation() {
    let h = Harness::new(
        "set-params-fail",
        &[("MOZART_RVC_STUB_FAIL_SET_PARAMS_PITCH", "3")],
    );
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "error", "SetParams 失败必须报错");
    assert_eq!(reply.code, "InferenceFailed");
    assert!(
        reply.reason.contains("rvc set params"),
        "错误应指明 SetParams 失败：{}",
        reply.reason
    );
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0, "SetParams 失败不得停流");
    h.wait_streaming();
    assert_eq!(h.rvc_params().pitch_shift, 0, "失败后生效配置保留旧值");
    let get = h.call(ctl::DAEMON_CONFIG_GET, br#"{"key":"rvc.pitch_shift"}"#);
    assert_eq!(get.status, "ok");
    assert_eq!(get.data["value"], 3, "TOML 保留新基线");

    // 存在待生效 pitch 基线时只改 protect：不触发 SetParams，未生效 pitch 不混入。
    h.clear_log();
    let reply = h.config_set("rvc.protect", "0.4");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    std::thread::sleep(Duration::from_millis(250));
    let params = h.rvc_params();
    assert_eq!(params.protect, 0.4);
    assert_eq!(params.pitch_shift, 0, "未生效 pitch 基线不得混入生效配置");
    assert_eq!(h.rvc_count(ctl::RVC_SET_PARAMS), 0);
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0);

    // 下一次写 pitch 键：重读基线并经 SetParams 成功生效（带上已更新的 protect）。
    h.clear_log();
    let reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    let calls = h.set_params_calls();
    assert_eq!(calls.len(), 1, "重试应下发一次 SetParams：{calls:?}");
    assert_eq!(calls[0]["pitch_shift"], 3);
    assert_eq!(calls[0]["protect"], 0.4);
    assert_eq!(h.rvc_count(ctl::RVC_STOP_STREAM), 0);
    let params = h.rvc_params();
    assert_eq!(params.pitch_shift, 3);
    assert_eq!(params.protect, 0.4);
}

/// 须停流重起的旋钮（startup_delay_ms）新参起流失败：用旧参恢复实时流并
/// 返回错误；TOML 保留新基线。不自动重试：等下一次对应键写入或 daemon 重启应用。
#[test]
fn start_failure_restores_old_params_and_keeps_toml_baseline() {
    let h = Harness::new("restore", &[("MOZART_RVC_STUB_FAIL_START_DELAY_MS", "200")]);
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.realtime.startup_delay_ms", "200");
    assert_eq!(reply.status, "error", "新参起流失败必须报错");
    assert_eq!(reply.code, "InferenceFailed");
    assert!(
        reply.reason.contains("new params"),
        "错误应指明新参起流失败：{}",
        reply.reason
    );

    // 旧参恢复实时流：流继续在跑。
    h.wait_streaming();
    let starts = h.stream_starts("realtime");
    assert!(
        starts.iter().any(|body| body["startup_delay_ms"] == 200),
        "应先按新参起流：{starts:?}"
    );
    assert!(
        starts.iter().any(|body| body["startup_delay_ms"] == 400),
        "失败后应用旧参恢复：{starts:?}"
    );

    // TOML 保留新基线，等待下一次对应键写入或 daemon 重启应用。
    let reply = h.call(
        ctl::DAEMON_CONFIG_GET,
        br#"{"key":"rvc.realtime.startup_delay_ms"}"#,
    );
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert_eq!(reply.data["value"], 200);

    // 失败后 daemon 内存生效配置仍保持旧值 startup_delay_ms=400：
    // 切换 direct → rvc 重建流时，StartStream 必须仍携带 400 而非 TOML 里的 200。
    h.clear_log();
    let direct = h.call(ctl::DAEMON_ROUTE_SET, br#"{"route":"direct"}"#);
    assert_eq!(direct.status, "ok", "route set direct: {}", direct.reason);
    assert!(
        wait_until(POLL_TIMEOUT, || {
            let status = h.status();
            status.route.active == Route::Direct
                && status.rvc.as_ref().is_some_and(|rvc| rvc.state != "streaming")
        }),
        "route direct 未就绪"
    );
    h.set_route_rvc();
    let starts = h.stream_starts("realtime");
    assert!(!starts.is_empty(), "重建流应发送 StartStream");
    assert!(
        starts.iter().all(|body| body["startup_delay_ms"] == 400),
        "生效配置必须保留旧 startup_delay_ms=400，不得被失败请求污染：{starts:?}"
    );
}

/// 新参与旧参起流都失败：返回组合错误并留下可查询的持久降级原因。
#[test]
fn failed_restore_returns_combined_error_and_records_degraded() {
    // 第一次 StartStream（route set rvc）成功，之后全部失败。
    let h = Harness::new("degraded", &[("MOZART_RVC_STUB_FAIL_START_AFTER", "1")]);
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.realtime.startup_delay_ms", "200");
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

    // 失败后的恢复与轮询补流仍使用内存生效配置里的旧 startup_delay_ms=400。
    let starts = h.stream_starts("realtime");
    assert!(
        starts.last().is_some_and(|body| body["startup_delay_ms"] == 400),
        "失败后恢复/补流 StartStream 必须仍带旧 startup_delay_ms=400：{starts:?}"
    );
    // 写回其他会话参数（仅更新生效配置中的对应键）验证未混入未生效的 startup_delay_ms=200。
    let pitch_reply = h.config_set("rvc.pitch_shift", "3");
    assert_eq!(pitch_reply.status, "ok", "reason: {}", pitch_reply.reason);
    assert_eq!(h.rvc_params().pitch_shift, 3);
    h.clear_log();
    assert!(
        wait_until(POLL_TIMEOUT, || !h.stream_starts("realtime").is_empty()),
        "轮询应持续尝试按当前生效配置补起实时流"
    );
    assert!(
        h.stream_starts("realtime")
            .iter()
            .all(|body| body["startup_delay_ms"] == 400 && body["pitch_shift"] == 3),
        "合并新 pitch_shift 时不得混入失败的 startup_delay_ms=200：{:?}",
        h.stream_starts("realtime")
    );
}

/// 停流未生效（StopStream 应答成功但流仍在）：新参起流失败，旧参恢复遇
/// Busy 被视为已恢复，但在跑参数不确定，留可查询的持久降级原因。
#[test]
fn recovery_busy_records_unknown_running_params() {
    let h = Harness::new("busyrestore", &[("MOZART_RVC_STUB_STOP_KEEP_STREAM", "1")]);
    h.set_route_rvc();
    h.clear_log();

    let reply = h.config_set("rvc.realtime.startup_delay_ms", "200");
    assert_eq!(reply.status, "error", "流未停时新参无法起流，必须报错");
    assert!(
        reply.reason.contains("restore busy") && reply.reason.contains("running params unknown"),
        "错误应说明恢复遇 Busy、在跑参数未知：{}",
        reply.reason
    );
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

    // 失败未替换内存生效配置：仍为 startup_delay_ms=400，因此再写回 400
    // 判定未变化、不会触发 Stop/Start。
    h.clear_log();
    let noop = h.config_set("rvc.realtime.startup_delay_ms", "400");
    assert_eq!(noop.status, "ok", "reason: {}", noop.reason);
    assert_eq!(
        h.rvc_count(ctl::RVC_STOP_STREAM),
        0,
        "生效配置保留旧值 400 时写回 400 不得触发停流重起"
    );
    assert!(
        h.stream_starts("realtime").is_empty(),
        "生效配置保留旧值 400 时写回 400 不得重起流：{:?}",
        h.stream_starts("realtime")
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

/// `[rvc.realtime]` 旋钮经 `config set` 修改时触发实时流 StopStream→StartStream，
/// 并携带新值到 rvc；`model use <id>` 在实时流期间执行 StopStream→LoadModel→StartStream。
/// overlap 锁为引擎形状对应的 60 ms，故用仍可调的 `startup_delay_ms` 驱动编排；
/// 偏离定值的 crossfade 必须在入口拒绝，不下发任何 Stop/Start。
#[test]
fn realtime_knob_and_model_use_orchestrate_stop_start() {
    let h = Harness::new("rt-knob-model-use", &[]);
    h.set_route_rvc();

    let stop_before = h.rvc_count(ctl::RVC_STOP_STREAM);
    let reply = h.config_set("rvc.realtime.startup_delay_ms", "200");
    assert_eq!(reply.status, "ok", "reason: {}", reply.reason);
    assert!(
        h.rvc_count(ctl::RVC_STOP_STREAM) > stop_before,
        "改 rvc.realtime.startup_delay_ms 应触发 StopStream"
    );
    let last_start = h
        .stream_starts("realtime")
        .pop()
        .expect("应重起实时流");
    assert_eq!(last_start["startup_delay_ms"], 200);
    assert_eq!(last_start["crossfade_ms"], 50, "overlap 仍锁在 50 + 10");
    assert_eq!(last_start["sola_search_ms"], 10);

    // overlap 偏离定值：入口拒绝，不触发 Stop/Start，也不改 TOML 基线。
    let stop_before_bad = h.rvc_count(ctl::RVC_STOP_STREAM);
    let start_before_bad = h.stream_starts("realtime").len();
    let baseline_path = h._daemon.dir.join("config.toml");
    let baseline_before_bad = std::fs::read_to_string(&baseline_path).unwrap();
    let bad = h.config_set("rvc.realtime.crossfade_ms", "40");
    assert_eq!(bad.status, "error");
    assert_eq!(bad.code, ctl::CODE_INVALID_ARGUMENT);
    assert!(
        bad.reason.contains("must equal")
            && bad.reason.contains("fixed at the maximum overlap"),
        "拒绝原因应说明 overlap 是定值：{}",
        bad.reason
    );
    assert_eq!(
        h.rvc_count(ctl::RVC_STOP_STREAM),
        stop_before_bad,
        "被拒的旋钮不得停流"
    );
    assert_eq!(
        h.stream_starts("realtime").len(),
        start_before_bad,
        "被拒的旋钮不得起流"
    );
    assert_eq!(
        std::fs::read_to_string(&baseline_path).unwrap(),
        baseline_before_bad,
        "被拒的 set 不得改动 TOML 基线"
    );

    let stop_before_use = h.rvc_count(ctl::RVC_STOP_STREAM);
    let load_before_use = h.rvc_count(ctl::RVC_LOAD_MODEL);
    h.conn
        .send(ctl::DAEMON_MODEL_USE, 200, br#"{"model_id":"demo"}"#)
        .unwrap();
    let use_reply: mozart_core::ctl::schema::Response = serde_json::from_slice(
        &h.conn
            .recv(Some(Duration::from_secs(5)))
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(use_reply.status, "ok", "reason: {}", use_reply.reason);
    assert_eq!(use_reply.data["model_id"], "demo");
    assert!(h.rvc_count(ctl::RVC_STOP_STREAM) > stop_before_use);
    assert!(h.rvc_count(ctl::RVC_LOAD_MODEL) > load_before_use);
    let last_after_use = h
        .stream_starts("realtime")
        .pop()
        .expect("换模型后应重起实时流");
    assert_eq!(last_after_use["startup_delay_ms"], 200);

    // 不存在的模型：返回错误并恢复原模型流。
    h.conn
        .send(ctl::DAEMON_MODEL_USE, 201, br#"{"model_id":"missing-model"}"#)
        .unwrap();
    let bad_reply: mozart_core::ctl::schema::Response = serde_json::from_slice(
        &h.conn
            .recv(Some(Duration::from_secs(5)))
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(bad_reply.status, "error");
}
