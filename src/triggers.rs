//! Delta triggers against the previous snapshot — the watchdog's rule:
//! nothing is reported until something actually moved, and the boundaries are
//! exactly the ones its `--selftest` proved (RAM strictly above, the rest `>=`).

use crate::report::{Metric, Trigger};

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// RAM change, bytes. Strictly greater trips.
    pub ram: u64,
    /// CPU change, percentage points. `>=` trips.
    pub cpu: f32,
    /// Disk read+write change, bytes. `>=` trips.
    pub disk: u64,
    /// Net in+out change, bytes. `>=` trips.
    pub net: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            ram: 10 << 20,
            cpu: 15.0,
            disk: 50 << 20,
            net: 20 << 20,
        }
    }
}

/// The comparable slice of a snapshot: this process's RAM/CPU/disk plus
/// system-wide net. RAM and disk are cumulative counters, CPU is a point value.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub ram: u64,
    pub cpu: f32,
    pub disk: u64,
    pub net: u64,
}

pub fn evaluate(th: &Thresholds, prev: &Sample, cur: &Sample) -> Vec<Trigger> {
    let mut out = Vec::new();

    let d_ram = cur.ram as i64 - prev.ram as i64;
    if d_ram.unsigned_abs() > th.ram {
        out.push(Trigger {
            metric: Metric::Ram,
            delta: d_ram as f64 / (1 << 20) as f64,
            unit: "MiB",
        });
    }
    let d_cpu = cur.cpu - prev.cpu;
    if d_cpu.abs() >= th.cpu {
        out.push(Trigger {
            metric: Metric::Cpu,
            delta: d_cpu as f64,
            unit: "pp",
        });
    }
    let d_disk = cur.disk as i64 - prev.disk as i64;
    if d_disk.unsigned_abs() >= th.disk {
        out.push(Trigger {
            metric: Metric::Disk,
            delta: d_disk as f64 / (1 << 20) as f64,
            unit: "MiB",
        });
    }
    let d_net = cur.net as i64 - prev.net as i64;
    if d_net.unsigned_abs() >= th.net {
        out.push(Trigger {
            metric: Metric::Net,
            delta: d_net as f64 / (1 << 20) as f64,
            unit: "MiB",
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Sample = Sample {
        ram: 100 << 20,
        cpu: 10.0,
        disk: 0,
        net: 0,
    };

    fn trips(d_ram: u64, d_cpu: f32, d_disk: u64, d_net: u64) -> Vec<Metric> {
        let cur = Sample {
            ram: BASE.ram + d_ram,
            cpu: BASE.cpu + d_cpu,
            disk: BASE.disk + d_disk,
            net: BASE.net + d_net,
        };
        evaluate(&Thresholds::default(), &BASE, &cur)
            .into_iter()
            .map(|t| t.metric)
            .collect()
    }

    /// The watchdog's boundary table: RAM is strictly above, the rest `>=`.
    #[test]
    fn boundaries_match_the_watchdog_selftest() {
        assert!(trips(1024, 0.0, 0, 0).is_empty(), "RAM +1 KiB");
        assert!(
            trips(10 << 20, 0.0, 0, 0).is_empty(),
            "RAM +10 MiB == limit"
        );
        assert_eq!(trips(11 << 20, 0.0, 0, 0), [Metric::Ram], "RAM +11 MiB");
        assert!(trips(0, 14.0, 0, 0).is_empty(), "CPU +14 pp");
        assert_eq!(trips(0, 16.0, 0, 0), [Metric::Cpu], "CPU +16 pp");
        assert!(trips(0, 0.0, 40 << 20, 0).is_empty(), "DISK +40 MiB");
        assert_eq!(trips(0, 0.0, 60 << 20, 0), [Metric::Disk], "DISK +60 MiB");
        assert!(trips(0, 0.0, 0, 15 << 20).is_empty(), "NET +15 MiB");
        assert_eq!(trips(0, 0.0, 0, 25 << 20), [Metric::Net], "NET +25 MiB");
    }

    #[test]
    fn negative_ram_delta_trips() {
        let prev = BASE;
        let cur = Sample {
            ram: BASE.ram - (100 << 20),
            ..BASE
        };
        let t = evaluate(&Thresholds::default(), &prev, &cur);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].metric, Metric::Ram);
        assert!(t[0].delta < 0.0);
    }
}
