//! 进程线程工具（DRY）：panic 传播、致命错误停止、依序回收、具名 spawn、
//! SEQPACKET 绑定。asr/bridge/tts/pre 各 main 复用；停止标志经参数传入，
//! 由各进程绑定自己的全局 STOP（见各 main 的薄封装）。

use crate::ctl::Seqpacket;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::JoinHandle;

/// 把 panic 转成 Err，走与致命错误同一条传播链（避免半死进程）。
/// 闭包经 AssertUnwindSafe 包装：线程退出路径不需要 unwind 一致性保证。
pub fn guard_panic<F>(body: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => Err(payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panicked".to_string())),
    }
}

/// 线程致命错误传播：返回 Err 前置停止标志，其余线程据此有序退出。
/// 设备线程必须自己传播停止：只靠 main 在 join 结果里补置时，排在死亡线程
/// 之前的健康线程不会退出，main 永远阻塞在它的 join 上。
pub fn stop_on_fatal(stop: &AtomicBool, result: Result<(), String>) -> Result<(), String> {
    if result.is_err() {
        stop.store(true, Relaxed);
    }
    result
}

/// 拉起具名线程；失败由调用方退出（先拉的线程随进程退出回收）。
pub fn spawn_thread<F>(name: &str, body: F) -> std::io::Result<JoinHandle<Result<(), String>>>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    std::thread::Builder::new().name(name.to_string()).spawn(body)
}

/// 依序回收线程；任一线程 Err/panic 置停止标志并记录首个失败。
/// 线程返回 Err 前已自置标志（stop_on_fatal）；此处幂等补置兜底。
pub fn join_threads(
    stop: &AtomicBool,
    handles: Vec<(&'static str, JoinHandle<Result<(), String>>)>,
) -> Option<String> {
    let mut failure = None;
    for (name, handle) in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                stop.store(true, Relaxed);
                failure = failure.or(Some(format!("{name}: {message}")));
            }
            Err(_) => {
                stop.store(true, Relaxed);
                failure = failure.or(Some(format!("{name}: panicked")));
            }
        }
    }
    failure
}

/// SEQPACKET 绑定：清理陈旧文件后绑定（进程重启无需外部清理）。
pub fn bind_socket(path: &Path) -> Result<Seqpacket, String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("清理陈旧 socket {}：{err}", path.display())),
    }
    Seqpacket::bind(path).map_err(|err| format!("绑定 {}：{err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_panic_converts_panic_to_err() {
        assert!(guard_panic(|| Ok(())).is_ok());
        assert_eq!(guard_panic(|| Err("fatal".to_string())), Err("fatal".to_string()));
        assert_eq!(
            guard_panic(|| panic!("boom {}", 42)),
            Err("boom 42".to_string())
        );
        assert_eq!(guard_panic(|| std::panic::panic_any(7_i32)), Err("panicked".to_string()));
    }

    #[test]
    fn stop_on_fatal_sets_flag_only_on_err() {
        let stop = AtomicBool::new(false);
        assert!(stop_on_fatal(&stop, Ok(())).is_ok());
        assert!(!stop.load(Relaxed));
        assert!(stop_on_fatal(&stop, Err("fatal".to_string())).is_err());
        assert!(stop.load(Relaxed));
    }

    #[test]
    fn join_threads_records_first_failure_and_sets_flag() {
        let stop = AtomicBool::new(false);
        let ok = std::thread::spawn(|| Ok::<(), String>(()));
        assert_eq!(join_threads(&stop, vec![("ok", ok)]), None);
        assert!(!stop.load(Relaxed));

        let err = std::thread::spawn(|| Err::<(), String>("device error".to_string()));
        assert_eq!(
            join_threads(&stop, vec![("err", err)]),
            Some("err: device error".to_string())
        );
        assert!(stop.load(Relaxed));
    }
}