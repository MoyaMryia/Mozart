#![allow(dead_code)]

use mozart_core::ctl::schema::{FileJob, FileJobState, ProcessState, Response, Status};
use mozart_core::ctl::{self, Seqpacket};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn example(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let target = exe
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|file| file == "target"))
        .expect("测试二进制不在 target 目录下");
    let profile = exe
        .strip_prefix(target)
        .expect("测试二进制不在 target 目录下")
        .components()
        .next()
        .expect("缺少 profile 目录");
    let path = target
        .join(profile.as_os_str())
        .join("examples")
        .join(name);
    assert!(
        path.is_file(),
        "找不到 {}；先运行 cargo build -p mozart-core --examples",
        path.display()
    );
    path
}

pub struct DaemonProcess {
    pub child: Child,
    pub dir: PathBuf,
    pub socket: PathBuf,
}

impl DaemonProcess {
    pub fn start(tag: &str, pre_bin: &Path, rvc_bin: &Path) -> Self {
        Self::start_with_config(
            tag,
            pre_bin,
            rvc_bin,
            "[daemon]\nrpc_timeout_ms = 300\n\n[rvc]\nmodel_id = \"demo\"\n",
        )
    }

    pub fn start_with_config(tag: &str, pre_bin: &Path, rvc_bin: &Path, config_body: &str) -> Self {
        Self::start_full(
            tag,
            pre_bin,
            rvc_bin,
            &example("asr_stub"),
            &example("tts_stub"),
            &example("bridge_stub"),
            &example("llama_stub"),
            config_body,
        )
    }

    /// 全量指定六个被监管进程的二进制；语言链测试用自定义桩或 wrapper 时走这里。
    pub fn start_full(
        tag: &str,
        pre_bin: &Path,
        rvc_bin: &Path,
        asr_bin: &Path,
        tts_bin: &Path,
        bridge_bin: &Path,
        llama_bin: &Path,
        config_body: &str,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!("mozart-daemon-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, config_body).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_mozart-daemon"))
            .arg("--config")
            .arg(&config)
            .env("MOZART_RUN_DIR", &dir)
            .env("MOZART_PRE_BIN", pre_bin)
            .env("MOZART_RVC_BIN", rvc_bin)
            .env("MOZART_ASR_BIN", asr_bin)
            .env("MOZART_TTS_BIN", tts_bin)
            .env("MOZART_BRIDGE_BIN", bridge_bin)
            .env("MOZART_LLAMA_BIN", llama_bin)
            .env("MOZART_POLL_INTERVAL_MS", "100")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let socket = dir.join("daemon.sock");
        Self {
            child,
            dir,
            socket,
        }
    }

    /// 文件链路测试用：`storage` 指向受管目录，`file_extra` 追加 `[file]` 键，
    /// `extra_env` 传给 daemon（子进程继承，如 rvc 桩的批处理延迟旋钮）。
    /// `fresh=false` 保留目录，用于重启续转。
    pub fn start_file(
        tag: &str,
        storage: &Path,
        file_extra: &str,
        fresh: bool,
        extra_env: &[(&str, String)],
    ) -> Self {
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_mozart-daemon"));
        command
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
            .stderr(Stdio::inherit());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().unwrap();
        let socket = dir.join("daemon.sock");
        Self {
            child,
            dir,
            socket,
        }
    }

    pub fn connect(&self) -> Seqpacket {
        connect(&self.socket)
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn connect(socket: &Path) -> Seqpacket {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Seqpacket::connect(socket) {
            Ok(conn) => return conn,
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!("connect {}: {err}", socket.display());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

pub fn call(conn: &Seqpacket, type_: u16, body: &[u8]) -> Response {
    call_timeout(conn, type_, body, Duration::from_secs(2))
}

pub fn call_timeout(conn: &Seqpacket, type_: u16, body: &[u8], timeout: Duration) -> Response {
    conn.send(type_, 7, body).unwrap();
    let msg = conn.recv(Some(timeout)).unwrap();
    let reply_type = msg.header.type_;
    assert_eq!(reply_type, type_, "响应 type 必须回显");
    serde_json::from_slice(&msg.body).unwrap()
}

pub fn status(conn: &Seqpacket) -> Status {
    serde_json::from_value(call(conn, ctl::DAEMON_GET_STATUS, b"").data).unwrap()
}

pub fn wait_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("daemon did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 等被监管进程起来且 rvc 状态已轮询到（仲裁要求 rvc_ready）。
pub fn wait_ready(conn: &Seqpacket) {
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

pub fn submit_file(conn: &Seqpacket, path: &Path, model: Option<&str>) -> Response {
    let body = json!({"path": path, "model_id": model});
    call(conn, ctl::DAEMON_FILE_SUBMIT, body.to_string().as_bytes())
}

pub fn file_job(conn: &Seqpacket, job_id: &str) -> FileJob {
    let body = json!({"job_id": job_id});
    let reply = call(conn, ctl::DAEMON_FILE_STATUS, body.to_string().as_bytes());
    assert_eq!(reply.status, "ok", "file status: {}", reply.reason);
    serde_json::from_value(reply.data).unwrap()
}

pub fn wait_job_state(
    conn: &Seqpacket,
    job_id: &str,
    state: FileJobState,
    timeout: Duration,
) -> FileJob {
    let mut last = None;
    assert!(
        wait_until(timeout, || {
            let job = file_job(conn, job_id);
            let reached = job.state == state;
            last = Some(job);
            reached
        }),
        "job {job_id} did not reach {state:?}: {last:?}"
    );
    last.unwrap()
}
