//! Test the actual launcher and bind acknowledgment without a daemon or models.
use std::{io::{Read, Write}, net::{TcpListener, TcpStream}, path::PathBuf, process::{Command, Output}};

struct Service { root: tempfile::TempDir, script: PathBuf }
impl Service {
    fn call(&self, action: &str) -> Output {
        Command::new("bash").arg(&self.script).arg(self.root.path()).arg(action).output().unwrap()
    }
}
impl Drop for Service { fn drop(&mut self) { let _ = self.call("stop"); } }

#[test]
fn launcher_readiness_restart_and_pid_identity() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port(); drop(listener);
    std::fs::create_dir_all(root.path().join("bin")).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_mozart-web-adapter"), root.path().join("bin/mozart-web-adapter")).unwrap();
    std::fs::write(root.path().join("mozart.toml"), format!("[daemon]\nrun_dir = {:?}\n[web]\nlisten = \"127.0.0.1:{port}\"\nallowed_origins = [\"http://127.0.0.1:{port}\"]\nupload_dir = \"uploads\"\n", root.path().join("run"))).unwrap();
    let service = Service { root, script: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/web-service.sh") };
    for action in ["start", "status", "restart"] {
        let result = service.call(action);
        assert!(result.status.success(), "{action}: {}", String::from_utf8_lossy(&result.stderr));
    }
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
    write!(client, "GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = String::new(); client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"adapter\":\"running\""));
    assert!(service.call("stop").status.success());
    assert!(!service.call("status").status.success());
    // A recycled PID must not let the launcher signal an unrelated process.
    let mut other = Command::new("sleep").arg("30").spawn().unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", other.id())).unwrap();
    let start = stat.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap();
    std::fs::write(service.root.path().join("run/web-adapter.pid"), format!("{}:{start}\n", other.id())).unwrap();
    let result = service.call("stop");
    let alive = other.try_wait().unwrap().is_none();
    other.kill().unwrap(); other.wait().unwrap();
    assert!(result.status.success()); assert!(alive, "launcher signaled an unrelated PID");
}
