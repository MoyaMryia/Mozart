//! 协议桩集成测试：验证 pre/rvc 桩的应答与状态流转。
//! 运行前需要构建示例：cargo build -p mozart-core --examples。

use mozart_core::ctl::{self, Message, Seqpacket};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mozart-stub-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("ctl.sock")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct StubChild(Child);

impl Drop for StubChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn example(name: &str) -> PathBuf {
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

fn spawn(name: &str, dir: &TempDir, extra: &[&str]) -> StubChild {
    let child = Command::new(example(name))
        .arg("--socket")
        .arg(dir.socket())
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|err| panic!("spawn {name}: {err}；先运行 cargo build -p mozart-core --examples"));
    StubChild(child)
}

fn connect(dir: &TempDir) -> Seqpacket {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match Seqpacket::connect(&dir.socket()) {
            Ok(conn) => return conn,
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!("connect {}: {err}", dir.socket().display());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn call(conn: &Seqpacket, type_: u16, body: &[u8]) -> Value {
    conn.send(type_, 1, body).unwrap();
    let reply = conn.recv(Some(Duration::from_secs(2))).unwrap();
    let reply_type = reply.header.type_;
    assert_eq!(reply_type, type_, "响应 type 必须回显");
    reply_json(&reply)
}

fn reply_json(msg: &Message) -> Value {
    serde_json::from_slice(&msg.body).unwrap()
}

#[test]
fn pre_stub_serves_control_plane() {
    let dir = TempDir::new("pre");
    let _child = spawn("pre_stub", &dir, &[]);
    let conn = connect(&dir);

    let hello = call(&conn, ctl::PRE_HELLO, b"");
    assert_eq!(hello["status"], "ok");

    let route = call(&conn, ctl::PRE_SET_ROUTE, br#"{"route":"rvc"}"#);
    assert_eq!(route["data"]["route"], "rvc");

    let mute = call(&conn, ctl::PRE_SET_MUTE, br#"{"mute":true}"#);
    assert_eq!(mute["data"]["mute"], true);

    let status = call(&conn, ctl::PRE_GET_STATUS, b"");
    assert_eq!(status["data"]["route"], "rvc");
    assert_eq!(status["data"]["mute"], true);

    let invalid = call(&conn, ctl::PRE_SET_ROUTE, br#"{"route":"bogus"}"#);
    assert_eq!(invalid["status"], "error");
    assert_eq!(invalid["code"], "InvalidArgument");
}

#[test]
fn rvc_stub_enforces_stream_order() {
    let dir = TempDir::new("rvc");
    let _child = spawn("rvc_stub", &dir, &[]);
    let conn = connect(&dir);

    call(&conn, ctl::RVC_HELLO, b"");

    let early = call(&conn, ctl::RVC_START_STREAM, br#"{"kind":"realtime"}"#);
    assert_eq!(early["code"], "NotLoaded");

    let loaded = call(&conn, ctl::RVC_LOAD_MODEL, br#"{"model_id":"demo"}"#);
    assert_eq!(loaded["status"], "ok");
    assert_eq!(loaded["data"]["model_handle"], 1);

    let started = call(&conn, ctl::RVC_START_STREAM, br#"{"kind":"realtime"}"#);
    assert_eq!(started["status"], "ok");

    let status = call(&conn, ctl::RVC_GET_STATUS, b"");
    assert_eq!(status["data"]["state"], "streaming");

    let stopped = call(&conn, ctl::RVC_STOP_STREAM, b"");
    assert_eq!(stopped["status"], "ok");

    let status = call(&conn, ctl::RVC_GET_STATUS, b"");
    assert_eq!(status["data"]["state"], "loaded");
}

#[test]
fn stub_exits_after_delay() {
    let dir = TempDir::new("exit");
    let mut child = spawn("pre_stub", &dir, &["--exit-after-ms", "100"]);
    let status = child.0.wait().unwrap();
    assert!(status.success());
}
