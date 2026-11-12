//! 文件队列/命令面集成测试：真实 daemon + 协议桩。
//! 源文件是占位字节，解码必失败，任务落 failed 带原因；
//! 队列/准入/取消/持久化/仲裁入口在此全链路验证（数据面见 file_worker.rs）。

mod common;

use common::{
    call, file_job, status, submit_file, wait_exit, wait_job_state, wait_ready, wait_until,
    DaemonProcess,
};
use mozart_core::ctl::schema::{Activity, FileJobState, Route};
use mozart_core::ctl;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 拉起 daemon；storage 指向受管目录。fresh=false 用于重启续转（不清目录）。
fn spawn(tag: &str, storage: &Path, file_extra: &str, fresh: bool) -> DaemonProcess {
    DaemonProcess::start_file(tag, storage, file_extra, fresh, &[])
}

/// 占位源字节：不是可解码音频，数据面必失败（本文件只验证队列/命令面）。
fn source(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"riff-placeholder").unwrap();
    path
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
    let reply = submit_file(&conn, &src, None);
    assert_eq!(reply.code, ctl::CODE_NOT_READY);
    let reply = submit_file(&conn, &src, Some("nope"));
    assert_eq!(reply.code, ctl::CODE_INVALID_ARGUMENT);

    // 暂停后入队两个，第三个容量拒收（Busy）。
    let reply = call(&conn, ctl::DAEMON_FILE_PAUSE, b"");
    assert_eq!(reply.status, "ok");
    assert_eq!(reply.data["paused"], true);
    let first = submit_file(&conn, &src, Some("demo"));
    assert_eq!(first.status, "ok", "{}", first.reason);
    assert_eq!(first.data["state"], "queued");
    assert_eq!(first.data["queue_position"], 1);
    let job_a = first.data["job_id"].as_str().unwrap().to_string();
    let src_b = source(&daemon.dir, "b.wav");
    let second = submit_file(&conn, &src_b, Some("demo"));
    assert_eq!(second.data["queue_position"], 2);
    let job_b = second.data["job_id"].as_str().unwrap().to_string();
    let src_c = source(&daemon.dir, "c.wav");
    let third = submit_file(&conn, &src_c, Some("demo"));
    assert_eq!(third.code, ctl::CODE_BUSY);

    // list/status 快照。
    let list = call(&conn, ctl::DAEMON_FILE_LIST, b"");
    assert_eq!(list.status, "ok");
    assert_eq!(list.data["paused"], true);
    assert_eq!(list.data["jobs"].as_array().unwrap().len(), 2);
    let job = file_job(&conn, &job_a);
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
    let failed = wait_job_state(&conn, &job_b, FileJobState::Failed, Duration::from_secs(5));
    assert!(failed.error.is_some());
    let fourth = submit_file(&conn, &src_c, Some("demo"));
    assert_eq!(fourth.status, "ok", "终态释放容量：{}", fourth.reason);
    let job_c = fourth.data["job_id"].as_str().unwrap().to_string();
    wait_job_state(&conn, &job_c, FileJobState::Failed, Duration::from_secs(5));
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
    let reply = submit_file(&conn, &src, Some("demo"));
    assert_eq!(reply.status, "ok", "{}", reply.reason);
    let job_id = reply.data["job_id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(file_job(&conn, &job_id).state, FileJobState::Queued);
    assert_eq!(status(&conn).arbiter.gpu.owner, Some(Activity::Realtime));

    // 释放实时后 poller 自动启动（占位 worker 标 failed）。
    let reply = call(&conn, ctl::DAEMON_ROUTE_SET, br#"{"route":"direct"}"#);
    assert_eq!(reply.status, "ok");
    wait_job_state(&conn, &job_id, FileJobState::Failed, Duration::from_secs(5));
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
    let reply = submit_file(&conn, &src, Some("demo"));
    assert_eq!(reply.status, "ok", "{}", reply.reason);
    let job_a = reply.data["job_id"].as_str().unwrap().to_string();
    let src_b = source(&first.dir, "b.wav");
    let reply = submit_file(&conn, &src_b, Some("demo"));
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
    wait_job_state(&conn, &job_a, FileJobState::Failed, Duration::from_secs(5));
    wait_job_state(&conn, &job_b, FileJobState::Failed, Duration::from_secs(5));
    // second 先 drop（清 dir_b），first 后 drop（清 dir_a 含 storage）。
    drop(second);
    drop(first);
}