mod common;

use common::{example, status, wait_until, DaemonProcess};
use std::time::Duration;

#[test]
fn reports_hardware_metrics() {
    let daemon = DaemonProcess::start("hw", &example("pre_stub"), &example("rvc_stub"));
    let conn = daemon.connect();

    assert!(
        wait_until(Duration::from_secs(5), || {
            match status(&conn).hw {
                Some(hw) => hw.memory.total_bytes > 0 && hw.cpu_percent.is_some(),
                None => false,
            }
        }),
        "hardware metrics were not sampled"
    );

    let hw = status(&conn).hw.unwrap();
    assert!(hw.memory.used_bytes <= hw.memory.total_bytes);
    if let Some(load) = hw.gpu.load_percent {
        assert!((0.0..=100.0).contains(&load));
    }
}
