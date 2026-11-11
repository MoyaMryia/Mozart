use mozart_core::ctl::schema::Status;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
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
