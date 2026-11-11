#![allow(dead_code)]

use mozart_core::ctl::schema::{Response, Status};
use mozart_core::ctl::{self, Seqpacket};
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
