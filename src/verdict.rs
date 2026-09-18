//! Rules evaluated against the current snapshot. Each crossing is a [`Verdict`]
//! naming the rule, the actual value, and the line it crossed.
//!
//! Every resource rule has two lines, not one. A single threshold can only ever
//! produce `Degraded`, which means `overall` could never reach `Unhealthy` from
//! a resource limit and "nearly full" would page the same as "full". The warn
//! line says pay attention; the critical line says act now.

use crate::report::{DiskMetrics, Health, OpTiming, ProcessMetrics, SystemMetrics, Verdict};
use crate::trend::MemoryTrend;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// System CPU above this percent is degraded.
    pub cpu_pct: f64,
    /// ...and above this it is unhealthy.
    pub cpu_pct_critical: f64,

    pub ram_used_pct: f64,
    pub ram_used_pct_critical: f64,

    /// Any filesystem *below* this free percent is degraded.
    pub disk_free_pct: f64,
    /// ...and below this it is unhealthy.
    pub disk_free_pct_critical: f64,

    /// Swap in use means RAM is overflowing.
    pub swap_used_pct: f64,

    /// Sustained growth of the memory floor, MiB/hour. This is the leak rule:
    /// it fires on the resting level climbing, never on a spike.
    pub mem_growth_mib_per_hour: f64,

    /// Fraction of calls in an interval that may fail before an operation is
    /// degraded, 0.0..=1.0.
    pub op_error_rate: f64,
    /// ...and above this it is unhealthy.
    pub op_error_rate_critical: f64,

    /// p99 latency budget for any measured operation, milliseconds. `None`
    /// disables the rule, which is the default: a budget belongs to a specific
    /// operation and a generic one would fire on every batch job.
    pub op_p99_ms: Option<f64>,

    /// Open handles above this is a handle leak. `None` disables the rule.
    pub handles: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            cpu_pct: 85.0,
            cpu_pct_critical: 95.0,
            ram_used_pct: 90.0,
            ram_used_pct_critical: 97.0,
            disk_free_pct: 5.0,
            disk_free_pct_critical: 1.0,
            swap_used_pct: 25.0,
            mem_growth_mib_per_hour: 20.0,
            op_error_rate: 0.01,
            op_error_rate_critical: 0.10,
            op_p99_ms: None,
            handles: None,
        }
    }
}

/// Everything the rules look at. Grouped so adding a rule does not mean
/// threading another argument through every caller.
pub struct Inputs<'a> {
    pub system: Option<&'a SystemMetrics>,
    pub process: Option<&'a ProcessMetrics>,
    pub disks: &'a [DiskMetrics],
    pub memory_trend: Option<&'a MemoryTrend>,
    pub ops: &'a [OpTiming],
}

/// Pick the level for a value that is bad when it rises.
fn rising(actual: f64, warn: f64, critical: f64) -> Option<(Health, f64)> {
    if actual > critical {
        Some((Health::Unhealthy, critical))
    } else if actual > warn {
        Some((Health::Degraded, warn))
    } else {
        None
    }
}

/// Pick the level for a value that is bad when it falls.
fn falling(actual: f64, warn: f64, critical: f64) -> Option<(Health, f64)> {
    if actual < critical {
        Some((Health::Unhealthy, critical))
    } else if actual < warn {
        Some((Health::Degraded, warn))
    } else {
        None
    }
}

pub fn evaluate(limits: &Limits, inputs: &Inputs<'_>) -> Vec<Verdict> {
    let mut out = Vec::new();
    let mut push = |rule, level, actual, threshold, unit, subject| {
        out.push(Verdict {
            rule,
            level,
            actual,
            threshold,
            unit,
            subject,
        })
    };

    if let Some(s) = inputs.system {
        if let Some((level, th)) = rising(s.cpu_pct as f64, limits.cpu_pct, limits.cpu_pct_critical)
        {
            push("cpu_high", level, s.cpu_pct as f64, th, "%", None);
        }
        if let Some(used) = s.ram_used_pct()
            && let Some((level, th)) =
                rising(used, limits.ram_used_pct, limits.ram_used_pct_critical)
        {
            push("ram_pressure", level, used, th, "%", None);
        }
        if let Some(used) = s.swap_used_pct()
            && used > limits.swap_used_pct
        {
            push(
                "swap_in_use",
                Health::Degraded,
                used,
                limits.swap_used_pct,
                "%",
                None,
            );
        }
    }

    for d in inputs.disks {
        let Some(free) = d.free_pct() else { continue };
        if let Some((level, th)) =
            falling(free, limits.disk_free_pct, limits.disk_free_pct_critical)
        {
            push("disk_low", level, free, th, "%", Some(d.mount.clone()));
        }
    }

    // The leak rule. Deliberately keyed on the floor's slope rather than any
    // current reading: a process that spikes and recovers is working, and only
    // a resting level that keeps climbing runs out of memory eventually.
    if let Some(t) = inputs.memory_trend
        && let Some(slope) = t.slope_mib_per_hour
        && slope > limits.mem_growth_mib_per_hour
    {
        push(
            "memory_growth",
            Health::Degraded,
            slope,
            limits.mem_growth_mib_per_hour,
            "MiB/h",
            None,
        );
    }

    if let (Some(p), Some(limit)) = (inputs.process, limits.handles)
        && let Some(h) = p.handles
        && h > limit
    {
        push(
            "handle_leak",
            Health::Degraded,
            h as f64,
            limit as f64,
            "handles",
            None,
        );
    }

    for op in inputs.ops {
        // `None` means nothing ran this interval — no calls is not 0% errors.
        if let Some(rate) = op.error_rate
            && let Some((level, th)) =
                rising(rate, limits.op_error_rate, limits.op_error_rate_critical)
        {
            push(
                "op_errors",
                level,
                rate * 100.0,
                th * 100.0,
                "%",
                Some(op.name.to_string()),
            );
        }
        if let (Some(budget), Some(p99)) = (limits.op_p99_ms, op.p99_ms())
            && p99 > budget
        {
            push(
                "op_slow",
                Health::Degraded,
                p99,
                budget,
                "ms",
                Some(op.name.to_string()),
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys(
        cpu: f32,
        ram_used: u64,
        ram_total: u64,
        swap_used: u64,
        swap_total: u64,
    ) -> SystemMetrics {
        SystemMetrics {
            cpu_pct: cpu,
            cpu_per_core: vec![],
            cores: 1,
            load_avg: None,
            ram_total,
            ram_used,
            ram_free: ram_total - ram_used,
            swap_total,
            swap_used,
        }
    }

    fn inputs<'a>(system: Option<&'a SystemMetrics>, disks: &'a [DiskMetrics]) -> Inputs<'a> {
        Inputs {
            system,
            process: None,
            disks,
            memory_trend: None,
            ops: &[],
        }
    }

    fn op(name: &'static str, error_rate: Option<f64>, p99_us: Option<u64>) -> OpTiming {
        OpTiming {
            name,
            count: 100,
            errors: 0,
            total_us: 0,
            min_us: None,
            mean_us: None,
            p50_us: None,
            p95_us: None,
            p99_us,
            max_us: None,
            count_delta: 100,
            errors_delta: 0,
            per_sec: None,
            error_rate,
            last_memory: None,
        }
    }

    #[test]
    fn quiet_system_has_no_verdicts() {
        let s = sys(10.0, 1, 100, 0, 100);
        assert!(evaluate(&Limits::default(), &inputs(Some(&s), &[])).is_empty());
    }

    #[test]
    fn warn_and_critical_are_distinct_levels() {
        // Between the two lines: degraded.
        let warm = sys(90.0, 1, 100, 0, 100);
        let v = evaluate(&Limits::default(), &inputs(Some(&warm), &[]));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "cpu_high");
        assert_eq!(v[0].level, Health::Degraded);

        // Past the critical line: unhealthy. Without this tier `overall` could
        // never be worse than Degraded from a resource limit.
        let hot = sys(99.0, 1, 100, 0, 100);
        let v = evaluate(&Limits::default(), &inputs(Some(&hot), &[]));
        assert_eq!(v[0].level, Health::Unhealthy);
        assert_eq!(v[0].threshold, Limits::default().cpu_pct_critical);
    }

    #[test]
    fn each_rule_fires() {
        let hot = sys(95.0, 95, 100, 50, 100);
        let v = evaluate(&Limits::default(), &inputs(Some(&hot), &[]));
        let rules: Vec<_> = v.iter().map(|x| x.rule).collect();
        assert!(rules.contains(&"cpu_high"));
        assert!(rules.contains(&"ram_pressure"));
        assert!(rules.contains(&"swap_in_use"));
    }

    #[test]
    fn disk_verdicts_name_the_mount_that_is_full() {
        let disks = vec![
            DiskMetrics {
                name: "C:".into(),
                mount: "C:\\".into(),
                total: 100,
                free: 50,
            },
            DiskMetrics {
                name: "D:".into(),
                mount: "D:\\".into(),
                total: 100,
                free: 2,
            },
        ];
        let v = evaluate(&Limits::default(), &inputs(None, &disks));
        assert_eq!(v.len(), 1, "only the full one");
        assert_eq!(v[0].rule, "disk_low");
        assert_eq!(v[0].subject.as_deref(), Some("D:\\"));
        assert_eq!(v[0].level, Health::Degraded);
    }

    #[test]
    fn a_rising_floor_is_a_leak_but_a_spike_is_not() {
        let leaking = MemoryTrend {
            samples: 60,
            window_secs: 3600,
            floor: 900 << 20,
            peak: 950 << 20,
            slope_mib_per_hour: Some(60.0),
        };
        let v = evaluate(
            &Limits::default(),
            &Inputs {
                system: None,
                process: None,
                disks: &[],
                memory_trend: Some(&leaking),
                ops: &[],
            },
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "memory_growth");

        // Huge peak, flat floor: that is work, not a leak.
        let spiky = MemoryTrend {
            samples: 60,
            window_secs: 3600,
            floor: 400 << 20,
            peak: 4096 << 20,
            slope_mib_per_hour: Some(0.5),
        };
        let v = evaluate(
            &Limits::default(),
            &Inputs {
                system: None,
                process: None,
                disks: &[],
                memory_trend: Some(&spiky),
                ops: &[],
            },
        );
        assert!(v.is_empty(), "a spike is not a leak: {v:?}");
    }

    #[test]
    fn an_unknown_slope_never_produces_a_leak_verdict() {
        let unknown = MemoryTrend {
            samples: 3,
            window_secs: 60,
            floor: 0,
            peak: u64::MAX,
            slope_mib_per_hour: None,
        };
        let v = evaluate(
            &Limits::default(),
            &Inputs {
                system: None,
                process: None,
                disks: &[],
                memory_trend: Some(&unknown),
                ops: &[],
            },
        );
        assert!(v.is_empty());
    }

    #[test]
    fn op_error_rate_escalates_and_silence_is_exempt() {
        let ops = [
            op("quiet", None, None),
            op("flaky", Some(0.05), None),
            op("broken", Some(0.50), None),
        ];
        let v = evaluate(
            &Limits::default(),
            &Inputs {
                system: None,
                process: None,
                disks: &[],
                memory_trend: None,
                ops: &ops,
            },
        );
        assert_eq!(v.len(), 2, "the quiet op has no error rate to judge");

        let flaky = v
            .iter()
            .find(|x| x.subject.as_deref() == Some("flaky"))
            .unwrap();
        assert_eq!(flaky.level, Health::Degraded);
        assert!((flaky.actual - 5.0).abs() < 1e-9, "reported as a percent");

        let broken = v
            .iter()
            .find(|x| x.subject.as_deref() == Some("broken"))
            .unwrap();
        assert_eq!(broken.level, Health::Unhealthy);
    }

    #[test]
    fn the_latency_budget_is_off_until_asked_for() {
        let ops = [op("slow", None, Some(5_000_000))]; // 5 s
        let base = Inputs {
            system: None,
            process: None,
            disks: &[],
            memory_trend: None,
            ops: &ops,
        };
        assert!(
            evaluate(&Limits::default(), &base).is_empty(),
            "no budget set"
        );

        let limits = Limits {
            op_p99_ms: Some(100.0),
            ..Default::default()
        };
        let v = evaluate(&limits, &base);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "op_slow");
        assert_eq!(v[0].subject.as_deref(), Some("slow"));
    }
}
