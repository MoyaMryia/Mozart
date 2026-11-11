//! 主机硬件指标：CPU 差值、内存与 Jetson GPU 负载。移植旧 SystemMonitor。

use mozart_core::ctl::schema::{GpuInfo, HardwareStatus, Memory};
use std::path::Path;

const GPU_LOAD_PATH: &str = "/sys/devices/platform/bus@0/17000000.gpu/load";
const GPU_DEVICES: [&str; 2] = ["/dev/nvidia0", "/dev/nvhost-gpu"];

#[derive(Clone, Copy)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

pub struct Monitor {
    previous_cpu: Option<CpuTimes>,
}

impl Monitor {
    pub fn new() -> Self {
        Self { previous_cpu: None }
    }

    pub fn sample(&mut self) -> HardwareStatus {
        let (memory_total, memory_used) = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| parse_meminfo(&text))
            .unwrap_or((0, 0));
        let cpu = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| parse_cpu(&text));
        let cpu_percent = match (self.previous_cpu, cpu) {
            (Some(previous), Some(current)) => cpu_usage(previous, current),
            _ => None,
        };
        if let Some(current) = cpu {
            self.previous_cpu = Some(current);
        }
        let gpu_load = std::fs::read_to_string(GPU_LOAD_PATH)
            .ok()
            .and_then(|text| text.trim().parse::<f64>().ok())
            .map(|raw| raw / 10.0);
        let gpu_available = GPU_DEVICES.iter().any(|path| Path::new(path).exists());
        HardwareStatus {
            cpu_percent,
            memory: Memory {
                total_bytes: memory_total,
                used_bytes: memory_used,
            },
            gpu: GpuInfo {
                available: gpu_available,
                load_percent: gpu_load,
                memory_total_bytes: memory_total,
                memory_used_bytes: memory_used,
            },
        }
    }
}

fn parse_cpu(text: &str) -> Option<CpuTimes> {
    let mut fields = text.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let values: Vec<u64> = fields
        .take(8)
        .map(|value| value.parse().ok())
        .collect::<Option<_>>()?;
    if values.len() < 8 {
        return None;
    }
    Some(CpuTimes {
        total: values.iter().sum(),
        idle: values[3] + values[4],
    })
}

fn cpu_usage(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total = current.total.checked_sub(previous.total)?;
    if total == 0 {
        return None;
    }
    let idle = current.idle.saturating_sub(previous.idle);
    Some(100.0 * (1.0 - idle as f64 / total as f64))
}

fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let mut total_kib: Option<u64> = None;
    let mut available_kib: Option<u64> = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("MemTotal:") => total_kib = parts.next().and_then(|value| value.parse().ok()),
            Some("MemAvailable:") => {
                available_kib = parts.next().and_then(|value| value.parse().ok())
            }
            _ => {}
        }
    }
    let total = total_kib? * 1024;
    let available = available_kib? * 1024;
    Some((total, total.saturating_sub(available)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_line() {
        let times = parse_cpu("cpu  10 0 20 70 0 0 0 0 0 0\ncpu0 1 2 3 4\n").unwrap();
        assert_eq!(times.total, 100);
        assert_eq!(times.idle, 70);

        assert!(parse_cpu("intr 1 2 3\n").is_none());
        assert!(parse_cpu("cpu  1 2 3\n").is_none());
    }

    #[test]
    fn computes_usage_from_deltas() {
        let previous = CpuTimes {
            total: 100,
            idle: 70,
        };
        let current = CpuTimes {
            total: 200,
            idle: 100,
        };
        let usage = cpu_usage(previous, current).unwrap();
        assert!((usage - 70.0).abs() < 1e-9);
        assert_eq!(
            cpu_usage(current, current),
            None,
            "无时间差时不报告 CPU 占用"
        );
    }

    #[test]
    fn parses_meminfo() {
        let (total, used) = parse_meminfo(
            "MemTotal:       8000000 kB\nMemFree:        1000 kB\nMemAvailable:   6000000 kB\n",
        )
        .unwrap();
        assert_eq!(total, 8_000_000 * 1024);
        assert_eq!(used, 2_000_000 * 1024);

        assert!(parse_meminfo("MemTotal: 1 kB\n").is_none());
    }
}
