use mozart_core::ctl::schema::{
    error_body, ok_body, DeviceEntry, FileJob, FileJobState, ModelEntry, ProcessState, Status,
};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct CtlEnv {
    dir: PathBuf,
    config: PathBuf,
}

impl CtlEnv {
    fn new(tag: &str) -> Self {
        Self::with_config(
            tag,
            "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\n",
        )
    }

    fn with_config(tag: &str, config_body: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mozart-ctl-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, config_body).unwrap();
        Self { dir, config }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mozart-ctl"));
        command
            .env("MOZART_RUN_DIR", &self.dir)
            .env("MOZART_DAEMON_BIN", profile_dir().join("mozart-daemon"))
            .env("MOZART_PRE_BIN", example("pre_stub"))
            .env("MOZART_RVC_BIN", example("rvc_stub"))
            .env("MOZART_ASR_BIN", example("asr_stub"))
            .env("MOZART_TTS_BIN", example("tts_stub"))
            .env("MOZART_BRIDGE_BIN", example("bridge_stub"))
            .env("MOZART_LLAMA_BIN", example("llama_stub"))
            .env("MOZART_POLL_INTERVAL_MS", "100")
            .arg("--config")
            .arg(&self.config);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("daemon.sock")
    }
}

impl Drop for CtlEnv {
    fn drop(&mut self) {
        let _ = self.command().arg("down").output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let target = exe
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "target"))
        .expect("测试二进制不在 target 目录下");
    let profile = exe
        .strip_prefix(target)
        .expect("测试二进制不在 target 目录下")
        .components()
        .next()
        .expect("缺少 profile 目录");
    target.join(profile.as_os_str())
}

fn example(name: &str) -> PathBuf {
    let path = profile_dir().join("examples").join(name);
    assert!(
        path.is_file(),
        "找不到 {}；先运行 cargo build -p mozart-core --examples",
        path.display()
    );
    path
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn status_json(env: &CtlEnv) -> Status {
    let output = env.run(&["status", "--json"]);
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn up_status_route_mute_down() {
    let env = CtlEnv::new("flow");

    let up = env.run(&["up"]);
    assert_success(&up);
    assert!(
        String::from_utf8_lossy(&up.stdout).contains("daemon started"),
        "stdout: {}",
        String::from_utf8_lossy(&up.stdout)
    );

    let again = env.run(&["up"]);
    assert_success(&again);
    assert!(String::from_utf8_lossy(&again.stdout).contains("already running"));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = status_json(&env);
        if status.pre.is_some() && status.rvc.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "children did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }

    let route = env.run(&["route", "set", "rvc"]);
    assert_success(&route);
    assert!(String::from_utf8_lossy(&route.stdout).contains("route: rvc"));

    let mute = env.run(&["mute", "on"]);
    assert_success(&mute);
    let status = status_json(&env);
    assert!(status.mute);

    let direct = env.run(&["route", "set", "direct"]);
    assert_success(&direct);

    let down = env.run(&["down"]);
    assert_success(&down);
    assert!(!env.socket().exists());

    let stopped = env.run(&["status"]);
    assert_eq!(
        stopped.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );

    let down_again = env.run(&["down"]);
    assert_success(&down_again);
}

#[test]
fn route_rejection_exits_4() {
    let env = CtlEnv::with_config("reject", "[daemon]\nrpc_timeout_ms = 300\n");
    let up = env.run(&["up"]);
    assert_success(&up);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = status_json(&env);
        if status.pre.is_some() && status.rvc.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "children did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }

    let output = env.run(&["route", "set", "rvc"]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("NotReady"));
}

#[test]
fn concurrent_up_both_succeed() {
    let env = CtlEnv::new("race");
    let first = env
        .command()
        .arg("up")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let second = env
        .command()
        .arg("up")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let first_status = first.wait_with_output().unwrap().status;
    let second_status = second.wait_with_output().unwrap().status;
    assert!(
        first_status.success(),
        "first up failed: {:?}",
        first_status.code()
    );
    assert!(
        second_status.success(),
        "second up failed: {:?}",
        second_status.code()
    );
}

#[test]
fn rejects_unknown_route_argument() {
    let env = CtlEnv::new("usage");
    let output = env.run(&["route", "set", "bogus"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn translate_and_speech_forward_to_bridge() {
    let env = CtlEnv::new("language");
    assert_success(&env.run(&["up"]));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = status_json(&env);
        if status.bridge.is_some() && status.asr.is_some() && status.tts.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "language children did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 开关命令的应答带桥的活状态；整机 status 是轮询读模型，下一秒才汇总。
    let off = env.run(&["translate", "off", "--json"]);
    assert_success(&off);
    let data: serde_json::Value = serde_json::from_slice(&off.stdout).unwrap();
    assert_eq!(data["translate_enabled"], false);
    assert!(wait_bridge(&env, |bridge| !bridge.translate_enabled));

    let speech_off = env.run(&["speech", "off"]);
    assert_success(&speech_off);
    assert!(String::from_utf8_lossy(&speech_off.stdout).contains("speech: off"));
    assert!(wait_bridge(&env, |bridge| !bridge.speech_enabled));

    let stop = env.run(&["speech", "stop"]);
    assert_success(&stop);
    assert!(String::from_utf8_lossy(&stop.stdout).contains("speech: stopped"));

    let on = env.run(&["translate", "on", "--json"]);
    assert_success(&on);
    let data: serde_json::Value = serde_json::from_slice(&on.stdout).unwrap();
    assert_eq!(data["translate_enabled"], true);
    assert!(wait_bridge(&env, |bridge| bridge.translate_enabled));
}

/// 轮询读模型有传播延迟：在时限内等整机 status 的桥快照满足谓词。
fn wait_bridge(env: &CtlEnv, predicate: impl Fn(&mozart_core::ctl::schema::BridgeStatus) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(bridge) = status_json(env).bridge {
            if predicate(&bridge) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn rejects_unknown_language_arguments() {
    let env = CtlEnv::new("langusage");
    assert_eq!(env.run(&["translate", "bogus"]).status.code(), Some(2));
    assert_eq!(env.run(&["speech", "bogus"]).status.code(), Some(2));
    assert_eq!(env.run(&["speech"]).status.code(), Some(2));
}

// ---- 文件/模型命令：桩 daemon 校验组消息、--json schema、退出码与 fetch 复制 ----

/// 桩 daemon：按命令号回固定应答，记录收到的请求（type + JSON 体）。
/// 不依赖真实 daemon/ffmpeg，精确控制 fetch 的结果路径与缺省名。
struct StubDaemon {
    requests: Arc<Mutex<Vec<(u16, Value)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StubDaemon {
    fn start(socket: &Path, replies: Vec<(u16, Vec<u8>)>) -> Self {
        let listener = Seqpacket::bind(socket).unwrap();
        listener.listen(4).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                if listener
                    .wait_readable(Some(Duration::from_millis(50)))
                    .is_err()
                {
                    continue;
                }
                let Ok(conn) = listener.accept() else {
                    continue;
                };
                while let Ok(msg) = conn.recv(Some(Duration::from_secs(1))) {
                    let body = serde_json::from_slice(&msg.body).unwrap_or(Value::Null);
                    thread_requests
                        .lock()
                        .unwrap()
                        .push((msg.header.type_, body));
                    let reply = replies
                        .iter()
                        .find(|(type_, _)| *type_ == msg.header.type_)
                        .map(|(_, reply)| reply.clone())
                        .unwrap_or_else(|| {
                            error_body(ctl::CODE_UNKNOWN_COMMAND, "stub: unknown command")
                        });
                    if conn
                        .send(msg.header.type_, msg.header.request_id, &reply)
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<(u16, Value)> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for StubDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn stdout_json(output: &Output) -> Value {
    assert_success(output);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout 不是 JSON：{err}\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn file_submit_builds_request_and_formats_reply() {
    let env = CtlEnv::new("file-submit");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_SUBMIT,
            ok_body(json!({"job_id": "job_1", "state": "queued", "queue_position": 1})),
        )],
    );

    let output = env.run(&["file", "submit", "/tmp/a.wav", "--model", "demo", "--json"]);
    let data = stdout_json(&output);
    assert_eq!(
        data,
        json!({"job_id": "job_1", "state": "queued", "queue_position": 1})
    );
    let requests = stub.requests();
    assert_eq!(requests[0].0, ctl::DAEMON_FILE_SUBMIT);
    assert_eq!(
        requests[0].1,
        json!({"path": "/tmp/a.wav", "model_id": "demo"})
    );

    // 缺省模型：model_id 为 null；人读输出带 job_id 与 state。
    let human = env.run(&["file", "submit", "/tmp/a.wav"]);
    assert_success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("job_1") && text.contains("queued"), "stdout: {text}");
    assert_eq!(
        stub.requests()[1].1,
        json!({"path": "/tmp/a.wav", "model_id": null})
    );
}

#[test]
fn file_cancel_rejection_exits_4() {
    let env = CtlEnv::new("file-cancel");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_CANCEL,
            error_body(ctl::CODE_INVALID_ARGUMENT, "unknown job_id: job_9"),
        )],
    );
    let output = env.run(&["file", "cancel", "job_9"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", stderr_text(&output));
    assert!(stderr_text(&output).contains("InvalidArgument"));
    assert!(output.stdout.is_empty());
    assert_eq!(stub.requests()[0].1, json!({"job_id": "job_9"}));
}

#[test]
fn file_and_model_commands_unreachable_exit_3() {
    let env = CtlEnv::new("file-unreachable");
    for args in [
        vec!["file", "list"],
        vec!["file", "status", "job_1"],
        vec!["file", "submit", "/tmp/a.wav"],
        vec!["file", "fetch", "job_1"],
        vec!["file", "pause"],
        vec!["model", "list"],
    ] {
        let output = env.run(&args);
        assert_eq!(output.status.code(), Some(3), "args: {args:?}");
    }
}

#[test]
fn file_fetch_copies_result_to_output() {
    let env = CtlEnv::new("file-fetch");
    let source = env.dir.join("result.wav");
    std::fs::write(&source, b"wav-payload").unwrap();
    let dest = env.dir.join("out.wav");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_FETCH,
            ok_body(json!({
                "job_id": "job_1",
                "output_path": source.to_str().unwrap(),
                "name": "tone.wav",
            })),
        )],
    );

    let output = env.run(&[
        "file",
        "fetch",
        "job_1",
        "-o",
        dest.to_str().unwrap(),
        "--json",
    ]);
    let data = stdout_json(&output);
    assert_eq!(data["job_id"], "job_1");
    assert_eq!(data["output_path"], dest.to_str().unwrap());
    assert_eq!(std::fs::read(&dest).unwrap(), b"wav-payload");
    let requests = stub.requests();
    assert_eq!(requests[0].0, ctl::DAEMON_FILE_FETCH);
    assert_eq!(
        requests[0].1,
        json!({"job_id": "job_1", "output": dest.to_str().unwrap()})
    );
}

#[test]
fn file_fetch_default_name_lands_in_cwd() {
    let env = CtlEnv::new("file-fetch-default");
    let source = env.dir.join("result.wav");
    std::fs::write(&source, b"wav-payload").unwrap();
    let cwd = env.dir.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_FETCH,
            ok_body(json!({
                "job_id": "job_1",
                "output_path": source.to_str().unwrap(),
                "name": "tone.wav",
            })),
        )],
    );

    let output = env
        .command()
        .current_dir(&cwd)
        .args(["file", "fetch", "job_1", "--json"])
        .output()
        .unwrap();
    let data = stdout_json(&output);
    assert_eq!(data["output_path"], "tone.wav");
    assert_eq!(std::fs::read(cwd.join("tone.wav")).unwrap(), b"wav-payload");
    assert_eq!(stub.requests()[0].1, json!({"job_id": "job_1", "output": null}));
}

#[test]
fn file_fetch_refuses_to_clobber_existing_destination() {
    let env = CtlEnv::new("file-fetch-clobber");
    let source = env.dir.join("result.wav");
    std::fs::write(&source, b"wav-payload").unwrap();
    let dest = env.dir.join("out.wav");
    std::fs::write(&dest, b"user-data").unwrap();
    let _stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_FETCH,
            ok_body(json!({
                "job_id": "job_1",
                "output_path": source.to_str().unwrap(),
                "name": "tone.wav",
            })),
        )],
    );

    let output = env.run(&["file", "fetch", "job_1", "-o", dest.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr_text(&output));
    assert!(stderr_text(&output).contains("exists"));
    // 用户数据未被覆盖。
    assert_eq!(std::fs::read(&dest).unwrap(), b"user-data");
}

#[test]
fn file_fetch_same_file_is_idempotent() {
    let env = CtlEnv::new("file-fetch-same");
    let source = env.dir.join("result.wav");
    std::fs::write(&source, b"wav-payload").unwrap();
    // 硬链接：路径不同但同一 inode，视为已取回。
    let dest = env.dir.join("hardlink.wav");
    std::fs::hard_link(&source, &dest).unwrap();
    let _stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_FETCH,
            ok_body(json!({
                "job_id": "job_1",
                "output_path": source.to_str().unwrap(),
                "name": "tone.wav",
            })),
        )],
    );

    let output = env.run(&["file", "fetch", "job_1", "-o", dest.to_str().unwrap()]);
    assert_success(&output);
    assert_eq!(std::fs::read(&dest).unwrap(), b"wav-payload");
    // -o 指向结果文件自身同样幂等成功。
    let output = env.run(&["file", "fetch", "job_1", "-o", source.to_str().unwrap()]);
    assert_success(&output);
}

#[test]
fn file_fetch_rejects_dangling_symlink_destination() {
    let env = CtlEnv::new("file-fetch-dangling");
    let source = env.dir.join("result.wav");
    std::fs::write(&source, b"wav-payload").unwrap();
    // 悬空符号链接：dest 指向不存在的目标。旧代码 dest.exists() 跟随链接判为
    // 「不存在」，fs::copy 写穿链接在目标处落盘；新代码按已存在拒绝，不写穿。
    let link_target = env.dir.join("link-target.wav");
    let dest = env.dir.join("out.wav");
    std::os::unix::fs::symlink(&link_target, &dest).unwrap();
    let _stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_FETCH,
            ok_body(json!({
                "job_id": "job_1",
                "output_path": source.to_str().unwrap(),
                "name": "tone.wav",
            })),
        )],
    );

    let output = env.run(&["file", "fetch", "job_1", "-o", dest.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr_text(&output));
    assert!(stderr_text(&output).contains("exists"), "stderr: {}", stderr_text(&output));
    // 未写穿链接：目标处不应出现文件，dest 仍是悬空链接。
    assert!(!link_target.exists(), "fetch 写穿了悬空符号链接");
    assert!(dest.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(std::fs::metadata(&dest).is_err());
}

#[test]
fn file_fetch_missing_source_exits_1() {
    let env = CtlEnv::new("file-fetch-missing");
    let gone = env.dir.join("gone.wav");
    let dest = env.dir.join("out.wav");
    let _stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_FETCH,
            ok_body(json!({
                "job_id": "job_1",
                "output_path": gone.to_str().unwrap(),
                "name": "tone.wav",
            })),
        )],
    );

    let output = env.run(&["file", "fetch", "job_1", "-o", dest.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr_text(&output));
    assert!(!dest.exists());
}

#[test]
fn file_fetch_rejection_exits_4() {
    let env = CtlEnv::new("file-fetch-reject");
    let _stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_FILE_FETCH,
            error_body(ctl::CODE_NOT_READY, "result not available: job job_1 not completed"),
        )],
    );
    let output = env.run(&["file", "fetch", "job_1"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", stderr_text(&output));
    assert!(stderr_text(&output).contains("result not available"));
}

#[test]
fn file_status_and_list_render_job_snapshots() {
    let env = CtlEnv::new("file-status");
    let completed = json!({
        "job_id": "job_1", "name": "tone.wav", "model_id": "demo",
        "state": "completed",
        "progress": {"stage": "publish", "percent": 100, "blocks_done": null, "blocks_total": null},
        "error": null, "result_ready": true, "created_unix_ms": 1_700_000_000_000u64
    });
    let processing = json!({
        "job_id": "job_2", "name": "song.mp3", "model_id": "demo",
        "state": "processing",
        "progress": {"stage": "infer", "percent": 60, "blocks_done": 12, "blocks_total": 20},
        "error": null, "result_ready": false, "created_unix_ms": 1_700_000_000_001u64
    });
    let stub = StubDaemon::start(
        &env.socket(),
        vec![
            (ctl::DAEMON_FILE_STATUS, ok_body(completed.clone())),
            (
                ctl::DAEMON_FILE_LIST,
                ok_body(json!({"jobs": [processing, completed], "paused": true})),
            ),
        ],
    );

    let status = env.run(&["file", "status", "job_1", "--json"]);
    let job: FileJob = serde_json::from_value(stdout_json(&status)).unwrap();
    assert_eq!(job.state, FileJobState::Completed);
    assert!(job.result_ready);
    assert_eq!(stub.requests()[0].1, json!({"job_id": "job_1"}));

    let human = env.run(&["file", "status", "job_1"]);
    assert_success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("completed") && text.contains("ready"), "stdout: {text}");

    let list = env.run(&["file", "list", "--json"]);
    let data = stdout_json(&list);
    assert_eq!(data["paused"], true);
    let jobs: Vec<FileJob> = serde_json::from_value(data["jobs"].clone()).unwrap();
    assert_eq!(jobs.len(), 2);
    // 请求顺序：status(--json)、status(人读)、list(--json)、list(人读)。
    assert_eq!(stub.requests()[2].0, ctl::DAEMON_FILE_LIST);

    let human = env.run(&["file", "list"]);
    assert_success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("job_2") && text.contains("processing") && text.contains("60%"),
        "stdout: {text}"
    );
    assert!(text.contains("queue paused"), "stdout: {text}");
}

#[test]
fn file_status_shows_error() {
    let env = CtlEnv::new("file-status-error");
    let failed = json!({
        "job_id": "job_1", "name": "tone.wav", "model_id": "demo",
        "state": "failed",
        "progress": {"stage": "decode", "percent": 0, "blocks_done": null, "blocks_total": null},
        "error": "decode failed", "result_ready": false, "created_unix_ms": 1_700_000_000_000u64
    });
    let _stub = StubDaemon::start(
        &env.socket(),
        vec![(ctl::DAEMON_FILE_STATUS, ok_body(failed))],
    );
    let output = env.run(&["file", "status", "job_1"]);
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("failed") && text.contains("decode failed"), "stdout: {text}");
}

#[test]
fn file_pause_and_resume_report_queue_flag() {
    let env = CtlEnv::new("file-pause");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![
            (ctl::DAEMON_FILE_PAUSE, ok_body(json!({"paused": true}))),
            (ctl::DAEMON_FILE_RESUME, ok_body(json!({"paused": false}))),
        ],
    );
    let pause = env.run(&["file", "pause", "--json"]);
    assert_eq!(stdout_json(&pause), json!({"paused": true}));
    let resume = env.run(&["file", "resume"]);
    assert_success(&resume);
    assert!(String::from_utf8_lossy(&resume.stdout).contains("running"));
    let requests = stub.requests();
    assert_eq!(requests[0].0, ctl::DAEMON_FILE_PAUSE);
    assert_eq!(requests[0].1, json!({}));
    assert_eq!(requests[1].0, ctl::DAEMON_FILE_RESUME);
}

#[test]
fn model_list_json_and_table() {
    let env = CtlEnv::new("model-list");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_MODEL_LIST,
            ok_body(json!({"models": [
                {"id": "demo", "exists": true, "loaded": true},
                {"id": "other", "exists": false, "loaded": false},
            ]})),
        )],
    );
    let output = env.run(&["model", "list", "--json"]);
    let data = stdout_json(&output);
    let models: Vec<ModelEntry> = serde_json::from_value(data["models"].clone()).unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "demo");
    assert!(models[0].loaded);
    assert_eq!(stub.requests()[0].0, ctl::DAEMON_MODEL_LIST);
    assert_eq!(stub.requests()[0].1, json!({}));

    let human = env.run(&["model", "list"]);
    assert_success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("demo") && text.contains("other"), "stdout: {text}");
    assert!(text.contains("yes") && text.contains("no"), "stdout: {text}");
}

#[test]
fn rejects_unknown_file_and_model_arguments() {
    let env = CtlEnv::new("file-usage");
    for args in [
        vec!["file"],
        vec!["file", "bogus"],
        vec!["file", "submit"],
        vec!["file", "fetch"],
        vec!["file", "cancel"],
        vec!["file", "status"],
        vec!["model"],
        vec!["model", "bogus"],
    ] {
        assert_eq!(env.run(&args).status.code(), Some(2), "args: {args:?}");
    }
    assert_success(&env.run(&["file", "--help"]));
    assert_success(&env.run(&["file", "fetch", "--help"]));
    assert_success(&env.run(&["model", "list", "--help"]));
}

// ---- 文件命令：真实 daemon + 协议桩端到端 ----

#[test]
fn file_commands_round_trip_with_daemon() {
    let env = CtlEnv::new("file-flow");
    assert_success(&env.run(&["up"]));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = status_json(&env);
        if status.rvc.is_some() && status.processes.rvc.state == ProcessState::Running {
            break;
        }
        assert!(Instant::now() < deadline, "rvc did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }

    // model list 转发 rvc ListModels。
    let models = stdout_json(&env.run(&["model", "list", "--json"]));
    assert!(models["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == "demo"));

    // 暂停入队：占位源不启动数据面，无需 ffmpeg。
    assert_success(&env.run(&["file", "pause"]));
    let src = env.dir.join("a.wav");
    std::fs::write(&src, b"riff-placeholder").unwrap();
    let submit = stdout_json(&env.run(&[
        "file",
        "submit",
        src.to_str().unwrap(),
        "--model",
        "demo",
        "--json",
    ]));
    let job_id = submit["job_id"].as_str().unwrap().to_string();
    assert_eq!(submit["state"], "queued");

    let job: FileJob =
        serde_json::from_value(stdout_json(&env.run(&["file", "status", &job_id, "--json"])))
            .unwrap();
    assert_eq!(job.state, FileJobState::Queued);
    assert_eq!(job.name, "a.wav");

    let list = stdout_json(&env.run(&["file", "list", "--json"]));
    assert_eq!(list["paused"], true);
    assert!(list["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|job| job["job_id"] == job_id));

    // 未完成取结果 → 拒绝（exit 4）。
    let fetch = env.run(&["file", "fetch", &job_id]);
    assert_eq!(fetch.status.code(), Some(4), "stderr: {}", stderr_text(&fetch));

    let cancel = stdout_json(&env.run(&["file", "cancel", &job_id, "--json"]));
    assert_eq!(cancel["state"], "cancelled");
    // 终态再取消 → 拒绝。
    assert_eq!(env.run(&["file", "cancel", &job_id]).status.code(), Some(4));
    assert_success(&env.run(&["file", "resume"]));
}

// ---- restart / device list：桩 daemon 校验组消息与 --json，真实 daemon 端到端 ----

#[test]
fn restart_builds_request_and_formats_reply() {
    let env = CtlEnv::new("restart");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_RESTART,
            ok_body(json!({"service": "rvc", "state": "restarting"})),
        )],
    );

    let output = env.run(&["restart", "rvc", "--json"]);
    assert_eq!(
        stdout_json(&output),
        json!({"service": "rvc", "state": "restarting"})
    );
    assert_eq!(stub.requests()[0].0, ctl::DAEMON_RESTART);
    assert_eq!(stub.requests()[0].1, json!({"service": "rvc"}));

    let human = env.run(&["restart", "pre"]);
    assert_success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("pre: restarting"), "stdout: {text}");
    assert_eq!(stub.requests()[1].1, json!({"service": "pre"}));
}

#[test]
fn device_list_json_and_table() {
    let env = CtlEnv::new("device-list");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_DEVICE_LIST,
            ok_body(json!({"devices": [
                {"id": "hw:0,0", "label": "Loopback Capture", "kind": "capture"},
                {"id": "hw:0,0", "label": "Loopback Playback", "kind": "playback"},
            ]})),
        )],
    );

    let output = env.run(&["device", "list", "--json"]);
    let data = stdout_json(&output);
    let devices: Vec<DeviceEntry> = serde_json::from_value(data["devices"].clone()).unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].kind, "capture");
    assert_eq!(stub.requests()[0].0, ctl::DAEMON_DEVICE_LIST);
    assert_eq!(stub.requests()[0].1, json!({"all": false}));

    let human = env.run(&["device", "list"]);
    assert_success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("hw:0,0") && text.contains("capture") && text.contains("playback"),
        "stdout: {text}"
    );
    assert!(text.contains("Loopback Capture"), "stdout: {text}");
}

#[test]
fn device_list_all_flag_forwarded() {
    let env = CtlEnv::new("device-all");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(ctl::DAEMON_DEVICE_LIST, ok_body(json!({"devices": []})))],
    );
    assert_success(&env.run(&["device", "list", "--all", "--json"]));
    assert_eq!(stub.requests()[0].1, json!({"all": true}));
}

#[test]
fn config_set_builds_request_and_restart_hint() {
    let env = CtlEnv::new("config-set");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(
            ctl::DAEMON_CONFIG_SET,
            ok_body(json!({
                "key": "pre.capture_device",
                "value": "plughw:CARD=X,DEV=0",
                "restart_required": true,
                "affected": ["pre"],
            })),
        )],
    );
    let output = env.run(&[
        "config", "set", "pre.capture_device", "plughw:CARD=X,DEV=0", "--json",
    ]);
    assert_eq!(stdout_json(&output)["restart_required"], json!(true));
    assert_eq!(stub.requests()[0].0, ctl::DAEMON_CONFIG_SET);
    assert_eq!(
        stub.requests()[0].1,
        json!({"key": "pre.capture_device", "value": "plughw:CARD=X,DEV=0"})
    );

    let human = env.run(&["config", "set", "pre.capture_device", "plughw:CARD=X,DEV=0"]);
    assert_success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("restart pre"), "stdout: {text}");
}

#[test]
fn config_get_builds_request() {
    let env = CtlEnv::new("config-get");
    let stub = StubDaemon::start(
        &env.socket(),
        vec![(ctl::DAEMON_CONFIG_GET, ok_body(json!({"value": "default"})))],
    );
    let output = env.run(&["config", "get", "pre.capture_device", "--json"]);
    assert_eq!(stdout_json(&output)["value"], json!("default"));
    assert_eq!(stub.requests()[0].1, json!({"key": "pre.capture_device"}));
}

#[test]
fn device_list_empty_renders_no_devices() {
    let env = CtlEnv::new("device-empty");
    let _stub = StubDaemon::start(
        &env.socket(),
        vec![(ctl::DAEMON_DEVICE_LIST, ok_body(json!({"devices": []})))],
    );
    let output = env.run(&["device", "list", "--json"]);
    assert_eq!(stdout_json(&output), json!({"devices": []}));
    let human = env.run(&["device", "list"]);
    assert_success(&human);
    assert!(String::from_utf8_lossy(&human.stdout).contains("no devices"));
}

#[test]
fn restart_and_device_unreachable_exit_3() {
    let env = CtlEnv::new("control-unreachable");
    for args in [vec!["restart", "rvc"], vec!["device", "list"]] {
        let output = env.run(&args);
        assert_eq!(output.status.code(), Some(3), "args: {args:?}");
    }
}

#[test]
fn rejects_unknown_restart_and_device_arguments() {
    let env = CtlEnv::new("control-usage");
    for args in [
        vec!["restart"],
        vec!["restart", "bogus"],
        vec!["device"],
        vec!["device", "bogus"],
    ] {
        assert_eq!(env.run(&args).status.code(), Some(2), "args: {args:?}");
    }
    assert_success(&env.run(&["restart", "--help"]));
    assert_success(&env.run(&["device", "list", "--help"]));
}

#[test]
fn restart_and_device_round_trip_with_daemon() {
    let env = CtlEnv::new("control-flow");
    assert_success(&env.run(&["up"]));
    let deadline = Instant::now() + Duration::from_secs(5);
    let old_pid = loop {
        let status = status_json(&env);
        if status.rvc.is_some() && status.processes.rvc.state == ProcessState::Running {
            break status.processes.rvc.pid.unwrap();
        }
        assert!(Instant::now() < deadline, "rvc did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    };

    let restart = stdout_json(&env.run(&["restart", "rvc", "--json"]));
    assert_eq!(restart["service"], "rvc");
    assert_eq!(restart["state"], "restarting");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = status_json(&env);
        if status.processes.rvc.state == ProcessState::Running
            && status.processes.rvc.pid.is_some_and(|pid| pid != old_pid)
        {
            break;
        }
        assert!(Instant::now() < deadline, "rvc was not restarted");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 设备清单来自 daemon 的 ALSA 枚举；主机可能无声卡，容忍空数组。
    let devices = stdout_json(&env.run(&["device", "list", "--json"]));
    assert!(devices["devices"].is_array());
}
