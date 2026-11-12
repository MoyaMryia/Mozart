//! 周期性轮询 pre/rvc/asr/tts/bridge 状态，并让实际状态向 daemon 期望状态收敛。

use crate::hwmon::Monitor;
use crate::services::{self, Services};
use crate::state::{lock_state, State};
use crate::supervisor::sleep_unless_stopped;
use mozart_core::config::Config;
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
        config: Arc<Config>,
        state: Arc<Mutex<State>>,
        services: Arc<Services>,
        running: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let interval = Duration::from_millis(config.daemon.poll_interval_ms);
        let handle = thread::Builder::new()
            .name("mozart-poll".to_string())
            .spawn(move || {
                let mut monitor = Monitor::new();
                while running.load(Ordering::Acquire) {
                    services::poll(&state, &services, &config);
                    let hw = monitor.sample();
                    lock_state(&state).set_hw(Some(hw));
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
