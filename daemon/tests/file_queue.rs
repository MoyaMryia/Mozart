//! 文件队列/命令面集成测试：真实 daemon + 协议桩。
//! 数据面 worker 在 Task 5 落地，本任务占位 worker 把自动启动的任务标 failed
//! 带原因；队列/准入/取消/持久化/仲裁入口在此全链路验证。

mod common;

use common::{call, example, status, wait_exit, wait_until, DaemonProcess};
use mozart_core::ctl::schema::{Activity, FileJob, FileJobState, ProcessState, Response, Route};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// 拉起 daemon；storage 指向受管目录。fresh=false 用于重启续转（不清目录）。
fn spawn(tag: &str, storage: &Path, file_extra: &str, fresh: bool) -> DaemonProcess {
    let dir = std::env::temp_dir().join(format!("mozart-daemon-{}-{tag}", std::process::id()));
    if fresh {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    let body = format!(
        "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\n\n[file]\nstorage_dir = \"{}\"\n{file_extra}\n",
        storage.display()
    );
    std::fs::write(&config, body).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_mozart-daemon"))
        .arg("--config")
        .arg(&config)
        .env("MOZART_RUN_DIR", &dir)
        .env("MOZART_PRE_BIN", example("pre_stub"))
        .env("MOZART_RVC_BIN", example("rvc_stub"))
        .env("MOZART_ASR_BIN", example("asr_stub"))
        .env("MOZART_TTS_BIN", example("tts_stub"))
        .env("MOZART_BRIDGE_BIN", example("bridge_stub"))
        .env("MOZART_LLAMA_BIN", example("llama_stub"))
        .env("MOZART_POLL_INTERVAL_MS", "100")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let socket = dir.join("daemon.sock");
    DaemonProcess {
        child,
        dir,
        socket,
    }
}

fn wait_ready(conn: &Seqpacket) {
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(conn);
            status.processes.pre.state == ProcessState::Running
                && status.processes.rvc.state == ProcessState::Running
                && status.rvc.is_some()
        }),
        "children did not start"
    );
}

fn source(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"riff-placeholder").unwrap();
    path
}

fn submit(conn: &Seqpacket, path: &Path, model: Option<&str>) -> Response {
    let body = json!({"path": path, "model_id": model});
    call(conn, ctl::DAEMON_FILE_SUBMIT, body.to_string().as_bytes())
}

fn job_status(conn: &Seqpacket, job_id: &str) -> FileJob {
    let body = json!({"job_id": job_id});
    let reply = call(conn, ctl::DAEMON_FILE_STATUS, body.to_string().as_bytes());
    assert_eq!(reply.status, "ok", "file status: {}", reply.reason);
    serde_json::from_value(reply.data).unwrap()
}

fn wait_job_state(conn: &Seqpacket, job_id: &str, state: FileJobState) -> FileJob {
    let mut last = None;
    assert!(
        wait_until(Duration::from_secs(5), || {
            let job = job_status(conn, job_id);
            let reached = job.state == state;
            last = Some(job);
            reached
        }),
        "job {job_id} did not reach {state:?}: {last:?}"
    );
    last.unwrap()
}

#[test]
fn queue_admission_pause_cancel_and_drain() {
    let dir = std::env::temp_dir().join(format!("mozart-daemon-{}-file-queue", std::process::id()));
    let storage = dir.join("files");
    let daemon = spawn("file-queue", &storage, "queue_capacity = 2", true);
    let conn = daemon.connect();
    wait_ready(&conn);

    // model list 转发 rvc ListModels。
    let models = call(&conn, ctl::DAEMON_MODEL_LIST, b"");
    assert_eq!(models.status, "ok");
    assert!(models.data["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == "demo" && model["exists"] == true));

    // 未加载模型且未指定 → NotReady；模型不存在 → InvalidArgument。
    let src = source(&daemon.dir, "a.wav");
    let reply = submit(&conn, &src, None);
    assert_eq!(reply.code, ctl::CODE_NOT_READY);
    let reply = submit(&conn, &src, Some("nope"));
    assert_eq!(reply.code, ctl::CODE_INVALID_ARGUMENT);

    // 暂停后入队两个，第三个容量拒收（Busy）。
    let reply = call(&conn, ctl::DAEMON_FILE_PAUSE, b"");
    assert_eq!(reply.status, "ok");
    assert_eq!(reply.data["paused"], true);
    let first = submit(&conn, &src, Some("demo"));
    assert_eq!(first.status, "ok", "{}", first.reason);
    assert_eq!(first.data["state"], "queued");
    assert_eq!(first.data["queue_position"], 1);
    let job_a = first.data["job_id"].as_str().unwrap().to_string();
    let src_b = source(&daemon.dir, "b.wav");
    let second = submit(&conn, &src_b, Some("demo"));
    assert_eq!(second.data["queue_position"], 2);
    let job_b = second.data["job_id"].as_str().unwrap().to_string();
    let src_c = source(&daemon.dir, "c.wav");
    let third = submit(&conn, &src_c, Some("demo"));
    assert_eq!(third.code, ctl::CODE_BUSY);

    // list/status 快照。
    let list = call(&conn, ctl::DAEMON_FILE_LIST, b"");
    assert_eq!(list.status, "ok");
    assert_eq!(list.data["paused"], true);
    assert_eq!(list.data["jobs"].as_array().unwrap().len(), 2);
    let job = job_status(&conn, &job_a);
    assert_eq!(job.name, "a.wav");
    assert_eq!(job.model_id, "demo");
    assert_eq!(job.state, FileJobState::Queued);

    // 取消排队任务：终态 cancelled，源副本删除。
    let body = json!({"job_id": job_a});
    let reply = call(&conn, ctl::DAEMON_FILE_CANCEL, body.to_string().as_bytes());
    assert_eq!(reply.status, "ok");
    assert_eq!(reply.data["state"], "cancelled");
    assert!(!storage.join(format!("{job_a}.input.wav")).exists());
    let reply = call(&conn, ctl::DAEMON_FILE_CANCEL, body.to_string().as_bytes());
    assert_eq!(reply.code, ctl::CODE_INVALID_ARGUMENT);
    let body = json!({"job_id": "job_42"});
    let reply = call(&conn, ctl::DAEMON_FILE_STATUS, body.to_string().as_bytes());
    assert_eq!(reply.code, ctl::CODE_INVALID_ARGUMENT);
    let reply = call(&conn, ctl::DAEMON_FILE_CANCEL, body.to_string().as_bytes());
    assert_eq!(reply.code, ctl::CODE_INVALID_ARGUMENT);

    // 请求体 deny_unknown_fields。
    let reply = call(
        &conn,
        ctl::DAEMON_FILE_SUBMIT,
        br#"{"path":"/tmp/x.wav","extra":1}"#,
    );
    assert_eq!(reply.code, ctl::CODE_INVALID_ARGUMENT);

    // 恢复后自动启动：占位 worker 标 failed 带原因，队列继续。
    let reply = call(&conn, ctl::DAEMON_FILE_RESUME, b"");
    assert_eq!(reply.data["paused"], false);
    let failed = wait_job_state(&conn, &job_b, FileJobState::Failed);
    assert!(failed.error.is_some());
    let fourth = submit(&conn, &src_c, Some("demo"));
    assert_eq!(fourth.status, "ok", "终态释放容量：{}", fourth.reason);
    let job_c = fourth.data["job_id"].as_str().unwrap().to_string();
    wait_job_state(&conn, &job_c, FileJobState::Failed);
}

#[test]
fn realtime_busy_file_waits_then_auto_starts() {
    let dir = std::env::temp_dir().join(format!("mozart-daemon-{}-file-busy", std::process::id()));
    let storage = dir.join("files");
    let daemon = spawn("file-busy", &storage, "", true);
    let conn = daemon.connect();
    wait_ready(&conn);

    // 实时占用 GPU。
    let reply = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"rvc"}"#);
    assert_eq!(reply.status, "ok");
    assert!(
        wait_until(Duration::from_secs(5), || {
            let status = status(&conn);
            status.route.active == Route::Rvc
                && status.arbiter.gpu.owner == Some(Activity::Realtime)
        }),
        "route rvc was not applied"
    );

    // 文件提交遇 Realtime → 入队等待，不拒绝、不抢占。
    let src = source(&daemon.dir, "a.wav");
    let reply = submit(&conn, &src, Some("demo"));
    assert_eq!(reply.status, "ok", "{}", reply.reason);
    let job_id = reply.data["job_id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(job_status(&conn, &job_id).state, FileJobState::Queued);
    assert_eq!(status(&conn).arbiter.gpu.owner, Some(Activity::Realtime));

    // 释放实时后 poller 自动启动（占位 worker 标 failed）。
    let reply = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"direct"}"#);
    assert_eq!(reply.status, "ok");
    wait_job_state(&conn, &job_id, FileJobState::Failed);
    assert!(
        wait_until(Duration::from_secs(2), || {
            status(&conn).arbiter.gpu.owner.is_none()
        }),
        "GPU 应在任务收尾后释放"
    );
}

#[test]
fn queue_persists_across_restart() {
    let dir_a =
        std::env::temp_dir().join(format!("mozart-daemon-{}-file-persist-a", std::process::id()));
    let storage = dir_a.join("files");
    let mut first = spawn("file-persist-a", &storage, "", true);
    let conn = first.connect();
    wait_ready(&conn);

    call(&conn, ctl::DAEMON_FILE_PAUSE, b"");
    let src = source(&first.dir, "a.wav");
    let reply = submit(&conn, &src, Some("demo"));
    assert_eq!(reply.status, "ok", "{}", reply.reason);
    let job_a = reply.data["job_id"].as_str().unwrap().to_string();
    let src_b = source(&first.dir, "b.wav");
    let reply = submit(&conn, &src_b, Some("demo"));
    let job_b = reply.data["job_id"].as_str().unwrap().to_string();
    assert!(storage.join("queue.json").is_file());
    drop(conn);

    // 崩溃（SIGKILL）后重启：队列与暂停标志读回，任务续转。
    first.child.kill().unwrap();
    wait_exit(&mut first.child, Duration::from_secs(5));
    let second = spawn("file-persist-b", &storage, "", true);
    let conn = second.connect();
    wait_ready(&conn);
    let list = call(&conn, ctl::DAEMON_FILE_LIST, b"");
    assert_eq!(list.data["paused"], true);
    let jobs = list.data["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().all(|job| job["state"] == "queued"));

    call(&conn, ctl::DAEMON_FILE_RESUME, b"");
    wait_job_state(&conn, &job_a, FileJobState::Failed);
    wait_job_state(&conn, &job_b, FileJobState::Failed);
    // second 先 drop（清 dir_b），first 后 drop（清 dir_a 含 storage）。
    drop(second);
    drop(first);
}