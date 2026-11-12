//! GPU 仲裁纯决策（D6）：无锁、无 I/O，poller 每 tick 调用一次，
//! 是实时/文件互斥与路由等待槽的唯一裁决点。
//!
//! 不变量：gpu_owner 为 None 当且仅当 route_active 为 Direct；
//! Realtime 由 route=rvc claim，File 由运行中文件任务 claim。

use mozart_core::ctl::schema::{Activity, Route};

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// 启动下一个排队任务（claim File）。
    StartJob(String),
    /// 应用等待槽路由（文件任务结束、GPU 释放后）。
    ApplyRoute(Route),
    Idle,
}

/// 裁决优先级：
/// 1. GPU 有主（Realtime 或 File）→ 等待，文件提交只入队不抢占。
/// 2. GPU 空闲且 desired != active → 应用等待槽路由（用户意图优先于排队任务）。
/// 3. 未暂停、rvc 可达且有排队任务 → 自动启动下一任务。
pub fn next_action(
    gpu_owner: Option<Activity>,
    paused: bool,
    next_queued: Option<&str>,
    route_desired: Route,
    route_active: Route,
    rvc_ready: bool,
) -> Action {
    if gpu_owner.is_some() {
        return Action::Idle;
    }
    if route_desired != route_active {
        return Action::ApplyRoute(route_desired);
    }
    if paused || !rvc_ready {
        return Action::Idle;
    }
    match next_queued {
        Some(job_id) => Action::StartJob(job_id.to_string()),
        None => Action::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECT: Route = Route::Direct;
    const RVC: Route = Route::Rvc;

    #[test]
    fn realtime_busy_keeps_file_queued() {
        assert_eq!(
            next_action(Some(Activity::Realtime), false, Some("job_1"), DIRECT, DIRECT, true),
            Action::Idle
        );
    }

    #[test]
    fn file_busy_defers_route_and_blocks_new_jobs() {
        // File 占用时即使 desired=rvc 也只等待（路由意图已由 route_set 记入 desired）。
        assert_eq!(
            next_action(Some(Activity::File), false, Some("job_2"), RVC, DIRECT, true),
            Action::Idle
        );
    }

    #[test]
    fn idle_applies_deferred_route_before_starting_jobs() {
        // 任务结束 GPU 释放：等待槽路由优先于剩余排队任务。
        assert_eq!(
            next_action(None, false, Some("job_2"), RVC, DIRECT, true),
            Action::ApplyRoute(RVC)
        );
        // 暂停不挡路由应用（暂停是队列级标志，独立于 GPU 仲裁）。
        assert_eq!(
            next_action(None, true, Some("job_2"), RVC, DIRECT, true),
            Action::ApplyRoute(RVC)
        );
    }

    #[test]
    fn idle_auto_starts_next_queued_job() {
        assert_eq!(
            next_action(None, false, Some("job_1"), DIRECT, DIRECT, true),
            Action::StartJob("job_1".to_string())
        );
    }

    #[test]
    fn pause_and_rvc_not_ready_block_start() {
        assert_eq!(
            next_action(None, true, Some("job_1"), DIRECT, DIRECT, true),
            Action::Idle
        );
        assert_eq!(
            next_action(None, false, Some("job_1"), DIRECT, DIRECT, false),
            Action::Idle
        );
    }

    #[test]
    fn empty_queue_idles() {
        assert_eq!(next_action(None, false, None, DIRECT, DIRECT, true), Action::Idle);
    }
}