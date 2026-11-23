//! Same-host monotonic clock for speech admission and cancellation fences.

use serde::{Deserialize, Serialize};

/// Prevent a supervised bridge from emitting before the selected policy is reasserted.
pub const SUPERVISED_BRIDGE_BOOT_ENV: &str = "MOZART_SUPERVISED_BRIDGE_BOOT";

pub fn monotonic_ns() -> u64 {
    let mut stamp = std::mem::MaybeUninit::uninit();
    // SAFETY: Linux supports CLOCK_MONOTONIC; clock_gettime initializes stamp on success.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, stamp.as_mut_ptr()) };
    assert_eq!(status, 0, "CLOCK_MONOTONIC is unavailable");
    let stamp = unsafe { stamp.assume_init() };
    stamp.tv_sec as u64 * 1_000_000_000 + stamp.tv_nsec as u64
}

pub fn valid_timestamp(value: u64) -> bool {
    value != 0 && value <= monotonic_ns()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopFence {
    pub cutoff_ns: u64,
}
