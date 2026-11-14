//! ffmpeg 子进程运行：解码与编码共用一份等待/超时/取消逻辑。
//! 只在文件数据面的离线线程调用，不在实时路径上。

use super::StepError;
use std::ffi::OsString;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 等待子进程期间的取消检查间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 跑一次 ffmpeg：成功退出返回 Ok，其余一律带可查询原因失败。
/// 超时与取消都杀进程并收尸，不留僵尸；`cancel` 由调用方给取消检查点。
/// stderr 以 `-v error` 级别输出，量小，退出后再读（管道写满时子进程阻塞，
/// 由超时兜底杀掉，不会死等）。
pub(super) fn run(
    program: &str,
    args: &[OsString],
    timeout: Duration,
    cancel: &dyn Fn() -> bool,
) -> Result<(), StepError> {
    if cancel() {
        return Err(StepError::Cancelled);
    }
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| StepError::Failed(format!("spawn {program}: {err}")))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr = read_stderr(&mut child);
                if status.success() {
                    return Ok(());
                }
                return Err(StepError::Failed(format!(
                    "{program} exited with {status}: {stderr}"
                )));
            }
            Ok(None) => {}
            Err(err) => {
                kill_and_reap(&mut child);
                return Err(StepError::Failed(format!("wait {program}: {err}")));
            }
        }
        if cancel() {
            kill_and_reap(&mut child);
            return Err(StepError::Cancelled);
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child);
            return Err(StepError::Failed(format!(
                "{program} timeout after {} ms",
                timeout.as_millis()
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 读尽 stderr 供失败原因；进程已退出或被杀，读到 EOF 即止。
fn read_stderr(child: &mut Child) -> String {
    let Some(mut pipe) = child.stderr.take() else {
        return String::new();
    };
    let mut text = String::new();
    let _ = pipe.read_to_string(&mut text);
    text.trim().to_string()
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}