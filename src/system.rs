//! Whole-machine CPU and memory, via `sysinfo`.

use crate::report::SystemMetrics;

/// `sys` must have been refreshed by the caller ([`crate::Apm::snapshot`]).
pub fn collect(sys: &sysinfo::System) -> Option<SystemMetrics> {
    let cpus = sys.cpus();
    if cpus.is_empty() {
        return None;
    }
    let cpu_pct = cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / cpus.len() as f32;

    // Windows has no load average; sysinfo returns zeros there rather than
    // failing, and a zero would read as "idle" instead of "not a thing here".
    let la = sysinfo::System::load_average();
    let load_avg = (!cfg!(windows)).then_some((la.one, la.five, la.fifteen));

    Some(SystemMetrics {
        cpu_pct,
        cpu_per_core: cpus.iter().map(|c| c.cpu_usage()).collect(),
        cores: cpus.len(),
        load_avg,
        ram_total: sys.total_memory(),
        ram_used: sys.used_memory(),
        ram_free: sys.available_memory(),
        swap_total: sys.total_swap(),
        swap_used: sys.used_swap(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_derive_from_the_raw_totals() {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let m = collect(&sys).expect("a machine has cpus");
        assert!(m.cores > 0);
        assert_eq!(m.cpu_per_core.len(), m.cores);
        assert!(m.ram_total > 0);
        let used = m.ram_used_pct().expect("ram_total > 0");
        assert!((0.0..=100.0).contains(&used), "ram_used_pct = {used}");
        #[cfg(windows)]
        assert_eq!(m.load_avg, None, "Windows has no load average");
    }

    #[test]
    fn percentages_are_none_when_the_total_is_zero() {
        let m = SystemMetrics {
            cpu_pct: 0.0,
            cpu_per_core: vec![],
            cores: 1,
            load_avg: None,
            ram_total: 0,
            ram_used: 0,
            ram_free: 0,
            swap_total: 0,
            swap_used: 0,
        };
        assert_eq!(m.ram_used_pct(), None);
        assert_eq!(m.swap_used_pct(), None, "a box with no swap is not 0% swap");
    }
}
