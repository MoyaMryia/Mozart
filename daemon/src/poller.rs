//! 周期性轮询 pre/rvc/asr/tts/bridge 状态，并让实际状态向 daemon 期望状态收敛；
//! 随后驱动文件仲裁（空闲自动启动任务、应用等待槽路由）。

use crate::file::FileCoordinator;
use crate::hwmon::Monitor;
use crate::preset::PresetManager;
use crate::runtime::SharedConfig;
use crate::services::{self, Services};
use crate::state::{lock_state, State};
use crate::state_file::{BridgeRestore, StateFile};
use crate::supervisor::sleep_unless_stopped;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct Poller {
    handle: thread::JoinHandle<()>,
}

impl Poller {
    pub fn spawn(
        config: SharedConfig,
        state: Arc<Mutex<State>>,
        services: Arc<Services>,
        presets: Arc<PresetManager>,
        file: Arc<FileCoordinator>,
        bridge_restore: Arc<Mutex<BridgeRestore>>,
        state_file: Arc<Mutex<StateFile>>,
        running: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let handle = thread::Builder::new()
            .name("mozart-poll".to_string())
            .spawn(move || {
                let mut monitor = Monitor::new();
                let mut last_bridge_pid: Option<u32> = None;
                while running.load(Ordering::Acquire) {
                    // 每轮读最新生效配置：poll_interval_ms 等运行时键即时生效。
                    let config = config.snapshot();
                    services::poll(&state, &services, &config, &presets, file.handoff_active());
                    let bridge_pid = lock_state(&state).bridge_pid();
                    {
                        // 持 bridge_restore 锁覆盖重载与下发，锁序 bridge_restore → state_file / bridge，
                        // 避免与并发的开关命令交错覆写待恢复状态。
                        let mut restore = bridge_restore
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if bridge_pid.is_some() && bridge_pid != last_bridge_pid {
                            last_bridge_pid = bridge_pid;
                            *restore = state_file
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .bridge_restore();
                        }
                        services::restore_bridge_switches(&services, &mut restore);
                    }
                    file.tick();
                    let hw = monitor.sample();
                    lock_state(&state).set_hw(Some(hw));
                    let interval = Duration::from_millis(config.daemon.poll_interval_ms);
                    if !sleep_unless_stopped(interval, &running) {
                        break;
                    }
                }
            })?;
        Ok(Self { handle })
    }

    pub fn join(self) {
        let _ = self.handle.join();
    }
}
