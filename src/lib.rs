//! clove4apm — Application Performance Monitoring that returns structs.
//!
//! No storage, no files, no writers, no background threads: [`Apm::snapshot`]
//! measures and returns an [`ApmReport`]. What the caller does with it — log
//! it, serve it on `/health`, ship it, ignore it — is the caller's business.
//!
//! The only state kept between calls is what a *rate* needs: the previous
//! sample, a bounded ring of memory readings, and in-RAM counters for scopes
//! and errors. All of it dies with the process.
//!
//! ```no_run
//! let mut apm = clove4apm::Apm::new(clove4apm::Config::for_service("admin"));
//! let report = apm.snapshot();
//! println!("{}", report.summary());
//! for problem in report.problems() {
//!     eprintln!("  {problem}");
//! }
//! ```
//!
//! # Reading a report
//!
//! * **Rates are `None` on the first snapshot.** There is no interval to divide
//!   by yet. Take a second one a few seconds later before believing any rate,
//!   including CPU — `sysinfo` computes CPU from a delta too.
//! * **`None` means unmeasured, not zero.** Nothing here guesses.
//! * **The floor, not the peak, is the leak signal.** See [`MemoryTrend`].
//! * **Percentiles, not averages.** An average hides the tail; see [`OpTiming`].

#[cfg(feature = "alloc-obs")]
pub mod alloc;
pub mod errors;
pub mod report;
pub mod scope;

mod checks;
mod disk;
mod hist;
mod net;
mod process;
mod system;
mod trend;
mod triggers;
mod verdict;
mod watch;

pub use report::{
    AllocStats, ApmReport, Check, CheckResult, DiskMetrics, ErrorSummary, Health, Identity,
    IoRates, MemoryHealth, Metric, NetMetrics, OpTiming, ProcessMetrics, ScopeMemory,
    SystemMetrics, Trigger, Verdict, WatchEvent, WatchedProcess,
};
pub use scope::{Scope, measure};
pub use trend::MemoryTrend;
pub use triggers::Thresholds;
pub use verdict::Limits;

use std::collections::HashMap;
use std::time::Instant;

/// Serialises the tests that assert on process-wide memory counters.
///
/// `live_bytes` and the OS memory counters are shared by the whole process, so
/// two tests allocating tens of megabytes at once read each other's work and
/// blow past any tolerance. They take this lock instead of guessing a bigger one.
#[cfg(test)]
pub(crate) fn memory_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Count one error into the next report's [`ErrorSummary`].
pub fn note_error(kind: &str, msg: impl std::fmt::Display) {
    errors::note(kind, msg);
}

/// Whether this platform can attribute memory to a [`Scope`] at all.
///
/// `false` means [`Scope::with_memory`] still works but reports zero deltas —
/// worth checking once at startup rather than wondering later.
pub fn memory_probe_available() -> bool {
    process::memory_probe_available()
}

#[derive(Debug, Clone)]
pub struct Config {
    pub service: String,
    pub version: String,
    /// Process name patterns to watch (`"server-*.exe"` style, one `*`
    /// wildcard; without one the match is exact, case-insensitive).
    pub watch: Vec<String>,
    pub checks: Vec<Check>,
    pub thresholds: Thresholds,
    pub limits: Limits,
    /// Report the `n` heaviest processes on the machine. `0` disables it, which
    /// is the default: it requires enumerating every process (see
    /// [`Apm::snapshot`]).
    pub top_n: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            service: "app".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            watch: Vec::new(),
            checks: Vec::new(),
            thresholds: Thresholds::default(),
            limits: Limits::default(),
            top_n: 0,
        }
    }
}

impl Config {
    pub fn for_service(name: impl Into<String>) -> Self {
        Config {
            service: name.into(),
            ..Default::default()
        }
    }

    pub fn version(mut self, v: impl Into<String>) -> Self {
        self.version = v.into();
        self
    }

    pub fn watch(mut self, pattern: impl Into<String>) -> Self {
        self.watch.push(pattern.into());
        self
    }

    pub fn check(mut self, c: Check) -> Self {
        self.checks.push(c);
        self
    }

    pub fn limits(mut self, l: Limits) -> Self {
        self.limits = l;
        self
    }

    pub fn thresholds(mut self, t: Thresholds) -> Self {
        self.thresholds = t;
        self
    }

    pub fn top_n(mut self, n: usize) -> Self {
        self.top_n = n;
        self
    }

    /// Whether anything configured needs the full process list.
    fn needs_all_processes(&self) -> bool {
        !self.watch.is_empty()
            || self.top_n > 0
            || self
                .checks
                .iter()
                .any(|c| matches!(c, Check::ProcessAlive { .. }))
    }
}

pub struct Apm {
    cfg: Config,
    sys: sysinfo::System,
    started: Instant,
    last_snapshot: Option<Instant>,
    prev_sample: Option<triggers::Sample>,
    prev_net: Option<(u64, u64)>,
    prev_io: Option<(u64, u64)>,
    prev_watch: HashMap<String, Option<u32>>,
    prev_ops: HashMap<&'static str, scope::PrevOp>,
    prev_errors: u64,
    #[cfg(feature = "alloc-obs")]
    prev_allocs: u64,
    rss_trend: trend::Trend,
    private_trend: trend::Trend,
}

impl Apm {
    pub fn new(cfg: Config) -> Self {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_all(); // prime the counters so the first snapshot is less blind
        Apm {
            cfg,
            sys,
            started: Instant::now(),
            last_snapshot: None,
            prev_sample: None,
            prev_net: None,
            prev_io: None,
            prev_watch: HashMap::new(),
            prev_ops: HashMap::new(),
            prev_errors: 0,
            #[cfg(feature = "alloc-obs")]
            prev_allocs: 0,
            rss_trend: trend::Trend::default(),
            private_trend: trend::Trend::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Measure everything and return it.
    ///
    /// Fail-open: a collector that fails leaves its field as `None`; it never
    /// fails the report, because a monitor that refuses to answer during an
    /// incident is worse than one that answers partially.
    ///
    /// # Cost
    ///
    /// The expensive part is enumerating processes, which is why it only
    /// happens when something configured actually needs it — a watch pattern, a
    /// `ProcessAlive` check, or `top_n`. With none of those, only this process
    /// is refreshed. The difference is large on a busy machine, and a monitor
    /// that is itself a load is a monitor people turn off.
    pub fn snapshot(&mut self) -> ApmReport {
        let now = Instant::now();
        let interval = self.last_snapshot.map(|t| now.duration_since(t));
        let interval_secs = interval.map(|d| d.as_secs_f64());
        self.last_snapshot = Some(now);

        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        if self.cfg.needs_all_processes() {
            self.sys
                .refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        } else {
            let me = sysinfo::Pid::from_u32(std::process::id());
            self.sys
                .refresh_processes(sysinfo::ProcessesToUpdate::Some(&[me]), true);
        }
        // Recreated per snapshot: cheap, and sidesteps stale-list bookkeeping.
        let nets = sysinfo::Networks::new_with_refreshed_list();
        let disks = sysinfo::Disks::new_with_refreshed_list();

        let system = system::collect(&self.sys);
        let proc_self = process::self_metrics(&self.sys);
        let net = net::collect(&nets, self.prev_net, interval_secs);
        let disk_list = disk::collect(&disks);
        let watched = watch::scan(&self.cfg.watch, &self.sys, &mut self.prev_watch);
        let top_processes = process::top_by_rss(&self.sys, self.cfg.top_n);
        let check_results: Vec<CheckResult> = self
            .cfg
            .checks
            .iter()
            .map(|c| checks::run(c, &self.sys, &disks))
            .collect();

        // --- memory over time -------------------------------------------------
        let t = self.started.elapsed().as_secs_f64();
        if let Some(p) = &proc_self {
            self.rss_trend.record(t, p.rss);
            if let Some(pb) = p.private_bytes {
                self.private_trend.record(t, pb);
            }
        }
        let memory = MemoryHealth {
            rss: self.rss_trend.summary(),
            private: self.private_trend.summary(),
            retention_bytes: self.retention_bytes(proc_self.as_ref()),
        };

        // --- rates ------------------------------------------------------------
        let io = proc_self.as_ref().map(|p| {
            let (pr, pw) = self.prev_io.unwrap_or((p.disk_read, p.disk_written));
            let read_delta = p.disk_read.saturating_sub(pr);
            let written_delta = p.disk_written.saturating_sub(pw);
            let per_sec = |d: u64| {
                self.prev_io
                    .and(interval_secs)
                    .filter(|s| *s > 0.0)
                    .map(|s| d as f64 / s)
            };
            IoRates {
                read_total: p.disk_read,
                written_total: p.disk_written,
                read_delta,
                written_delta,
                read_per_sec: per_sec(read_delta),
                written_per_sec: per_sec(written_delta),
            }
        });

        let ops = scope::snapshot(&mut self.prev_ops, interval_secs);
        let error_summary = errors::summary(&mut self.prev_errors, interval_secs);
        let alloc_stats = self.alloc_stats(interval_secs);

        // --- findings ---------------------------------------------------------
        let cur_sample = triggers::Sample {
            ram: proc_self.as_ref().map_or(0, |p| p.rss),
            cpu: proc_self.as_ref().map_or(0.0, |p| p.cpu_pct),
            disk: proc_self
                .as_ref()
                .map_or(0, |p| p.disk_read + p.disk_written),
            net: net.as_ref().map_or(0, |n| n.rx_total + n.tx_total),
        };
        let trig = match &self.prev_sample {
            Some(prev) => triggers::evaluate(&self.cfg.thresholds, prev, &cur_sample),
            None => Vec::new(),
        };

        // Private bytes is the honest series where it exists: the working set
        // falls when the OS trims pages, which would read as a leak curing
        // itself.
        let leak_series = memory.private.as_ref().or(memory.rss.as_ref());
        let verd = verdict::evaluate(
            &self.cfg.limits,
            &verdict::Inputs {
                system: system.as_ref(),
                process: proc_self.as_ref(),
                disks: &disk_list,
                memory_trend: leak_series,
                ops: &ops,
            },
        );

        let mut overall = Health::Healthy;
        for c in &check_results {
            overall = overall.max(c.status);
        }
        for v in &verd {
            overall = overall.max(v.level);
        }
        // A watched process disappearing is the loudest thing this can observe;
        // it outranks any threshold.
        if watched.iter().any(|w| w.event == Some(WatchEvent::Gone)) {
            overall = Health::Unhealthy;
        }

        self.prev_net = net.as_ref().map(|n| (n.rx_total, n.tx_total));
        self.prev_io = proc_self.as_ref().map(|p| (p.disk_read, p.disk_written));
        self.prev_sample = Some(cur_sample);

        ApmReport {
            identity: Identity {
                service: self.cfg.service.clone(),
                version: self.cfg.version.clone(),
                host: sysinfo::System::host_name(),
                pid: std::process::id(),
            },
            ts_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            uptime_secs: self.started.elapsed().as_secs(),
            interval_ms: interval.map(|d| d.as_millis() as u64),
            system,
            process: proc_self,
            memory,
            io,
            watched,
            top_processes,
            disks: disk_list,
            net,
            checks: check_results,
            ops,
            errors: error_summary,
            alloc: alloc_stats,
            triggers: trig,
            verdicts: verd,
            overall,
        }
    }

    #[cfg(feature = "alloc-obs")]
    fn retention_bytes(&self, p: Option<&ProcessMetrics>) -> Option<u64> {
        let p = p?;
        // Against private bytes, not the working set: the gap should mean
        // "committed but not live", and a trimmed working set would make it
        // look as though retention had improved.
        let committed = p.private_bytes.unwrap_or(p.rss);
        Some(committed.saturating_sub(alloc::live_bytes()))
    }

    #[cfg(not(feature = "alloc-obs"))]
    fn retention_bytes(&self, _p: Option<&ProcessMetrics>) -> Option<u64> {
        None
    }

    #[cfg(feature = "alloc-obs")]
    fn alloc_stats(&mut self, interval_secs: Option<f64>) -> Option<AllocStats> {
        let (allocated_total, freed_total) = alloc::totals();
        let (alloc_count, dealloc_count) = alloc::counts();
        let delta = alloc_count.saturating_sub(self.prev_allocs);
        let had_prev = self.prev_allocs > 0;
        self.prev_allocs = alloc_count;
        Some(AllocStats {
            live_bytes: alloc::live_bytes(),
            allocated_total,
            freed_total,
            alloc_count,
            dealloc_count,
            allocs_per_sec: interval_secs
                .filter(|s| *s > 0.0 && had_prev)
                .map(|s| delta as f64 / s),
        })
    }

    #[cfg(not(feature = "alloc-obs"))]
    fn alloc_stats(&mut self, _interval_secs: Option<f64>) -> Option<AllocStats> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_smoke() {
        let mut apm = Apm::new(
            Config::for_service("test")
                .watch("definitely-not-running-xyz.exe")
                .check(Check::DiskFree {
                    name: "any".into(),
                    mount: if cfg!(windows) {
                        "C:\\".into()
                    } else {
                        "/".into()
                    },
                    min_free: 0,
                }),
        );
        let r = apm.snapshot();
        assert_eq!(r.identity.service, "test");
        assert_eq!(r.identity.pid, std::process::id());
        assert!(r.ts_unix_ms > 0);
        assert!(r.system.is_some(), "system metrics");
        let p = r.process.as_ref().expect("self process");
        assert!(p.rss > 0);
        #[cfg(windows)]
        {
            assert!(p.private_bytes.is_some(), "private bytes on Windows");
            assert!(p.handles.is_some(), "handle count on Windows");
            assert!(p.page_faults.is_some(), "page faults on Windows");
        }
        assert!(!r.disks.is_empty());
        assert_eq!(r.checks.len(), 1);
        assert!(
            r.triggers.is_empty(),
            "first snapshot has no prev to compare"
        );
    }

    #[test]
    fn the_first_snapshot_has_no_interval_and_therefore_no_rates() {
        let mut apm = Apm::new(Config::for_service("t"));
        let first = apm.snapshot();
        assert_eq!(first.interval_ms, None);
        assert!(first.io.as_ref().is_none_or(|io| io.read_per_sec.is_none()));
        assert!(first.net.as_ref().is_none_or(|n| n.rx_per_sec.is_none()));

        std::thread::sleep(std::time::Duration::from_millis(60));
        let second = apm.snapshot();
        let ms = second.interval_ms.expect("second snapshot has an interval");
        assert!(ms >= 50, "interval {ms}ms");
        assert!(
            second
                .io
                .as_ref()
                .is_none_or(|io| io.read_per_sec.is_some())
        );
    }

    #[test]
    fn memory_trend_accumulates_across_snapshots() {
        let mut apm = Apm::new(Config::for_service("t"));
        for _ in 0..3 {
            apm.snapshot();
        }
        let r = apm.snapshot();
        let t = r.memory.rss.expect("trend after several snapshots");
        assert_eq!(t.samples, 4);
        assert!(t.peak >= t.floor);
        // Four samples in a few milliseconds cannot support an hourly rate.
        assert_eq!(t.slope_mib_per_hour, None, "refuses to extrapolate");
    }

    #[test]
    fn a_vanished_watch_target_makes_the_whole_report_unhealthy() {
        let mut apm = Apm::new(Config::for_service("t").watch("ghost-process-xyz.exe"));
        apm.snapshot();
        // Pretend it was there a moment ago, then confirm it is gone.
        apm.prev_watch
            .insert("ghost-process-xyz.exe".into(), Some(4242));
        let r = apm.snapshot();
        assert_eq!(r.watched[0].event, Some(WatchEvent::Gone));
        assert_eq!(r.overall, Health::Unhealthy);
        assert!(r.problems().iter().any(|p| p.contains("process gone")));
    }

    #[test]
    fn a_healthy_report_lists_no_problems() {
        let mut apm = Apm::new(Config::for_service("t"));
        let r = apm.snapshot();
        if r.overall.is_healthy() {
            assert!(r.problems().is_empty());
        }
        // The summary always renders, healthy or not.
        assert!(r.summary().contains("t"));
    }

    #[test]
    fn top_processes_are_off_unless_requested() {
        let mut apm = Apm::new(Config::for_service("t"));
        assert!(apm.snapshot().top_processes.is_empty());

        let mut apm = Apm::new(Config::for_service("t").top_n(3));
        let r = apm.snapshot();
        assert!(!r.top_processes.is_empty());
        assert!(r.top_processes.len() <= 3);
    }

    #[test]
    fn process_enumeration_only_happens_when_something_needs_it() {
        assert!(!Config::for_service("t").needs_all_processes());
        assert!(Config::for_service("t").watch("x").needs_all_processes());
        assert!(Config::for_service("t").top_n(1).needs_all_processes());
        assert!(
            Config::for_service("t")
                .check(Check::ProcessAlive {
                    name: "p".into(),
                    pattern: "x".into()
                })
                .needs_all_processes()
        );
        // A disk check does not justify walking every process on the machine.
        assert!(
            !Config::for_service("t")
                .check(Check::DiskFree {
                    name: "d".into(),
                    mount: "/".into(),
                    min_free: 0
                })
                .needs_all_processes()
        );
    }

    #[test]
    fn note_error_accumulates() {
        let mut apm = Apm::new(Config::for_service("t"));
        apm.snapshot();
        note_error("test", "boom");
        let r = apm.snapshot();
        assert!(r.errors.delta >= 1);
        assert!(r.errors.last.is_some(), "the last message is recorded");
        assert!(
            r.errors.per_kind.iter().any(|(k, _)| k == "test"),
            "the kind is what groups reliably; `last` is a shared global slot"
        );
    }
}
