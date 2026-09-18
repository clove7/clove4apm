//! Every result type the framework returns. Plain data, no behaviour, no I/O.
//!
//! Two rules hold throughout:
//!
//! 1. A field that could not be measured is `None`, never a guessed number.
//!    A guessed number is worse than no number, because it gets believed.
//! 2. Cumulative counters are paired with their interval delta. A lifetime
//!    total answers "how much since boot"; only the delta answers "what is
//!    happening now", and an APM is asked the second question.

/// Health ladder. Ordered: `Healthy < Degraded < Unhealthy`, so `max()` over a
/// set of findings yields the worst one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    Healthy,
    Degraded,
    Unhealthy,
}

impl Health {
    pub fn is_healthy(self) -> bool {
        self == Health::Healthy
    }
}

/// Who produced the report.
#[derive(Debug, Clone)]
pub struct Identity {
    pub service: String,
    pub version: String,
    pub host: Option<String>,
    pub pid: u32,
}

/// Whole-machine view.
#[derive(Debug, Clone)]
pub struct SystemMetrics {
    /// Total CPU across all cores, 0..100.
    pub cpu_pct: f32,
    pub cpu_per_core: Vec<f32>,
    pub cores: usize,
    /// 1/5/15-minute load average. `None` on Windows, which has no equivalent.
    pub load_avg: Option<(f64, f64, f64)>,
    pub ram_total: u64,
    pub ram_used: u64,
    pub ram_free: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

impl SystemMetrics {
    pub fn ram_used_pct(&self) -> Option<f64> {
        (self.ram_total > 0).then(|| self.ram_used as f64 / self.ram_total as f64 * 100.0)
    }

    pub fn swap_used_pct(&self) -> Option<f64> {
        (self.swap_total > 0).then(|| self.swap_used as f64 / self.swap_total as f64 * 100.0)
    }
}

/// One process — this one, or one watched by name.
///
/// Two numbers that disagree on purpose: `rss` (working set, which shrinks when
/// the OS trims pages and so reads as a fake improvement) and `private_bytes`
/// (what the process actually still owns). Compare them before trusting either.
#[derive(Debug, Clone)]
pub struct ProcessMetrics {
    pub pid: u32,
    pub name: String,
    pub rss: u64,
    pub private_bytes: Option<u64>,
    pub peak_rss: Option<u64>,
    /// Percent of one core; above 100 means several cores busy.
    pub cpu_pct: f32,
    /// Kernel + user CPU burned since process start.
    pub cpu_time_ms: Option<u64>,
    pub threads: Option<u32>,
    /// Open kernel handles. A number that only ever climbs is a handle leak,
    /// which shows here long before it shows in memory.
    pub handles: Option<u32>,
    /// Page faults since start. Rising fast under flat RSS means thrashing.
    pub page_faults: Option<u64>,
    pub uptime_secs: u64,
    pub disk_read: u64,
    pub disk_written: u64,
    pub disk_read_ops: Option<u64>,
    pub disk_write_ops: Option<u64>,
}

impl ProcessMetrics {
    /// How far the working set has been trimmed below what the process owns.
    ///
    /// A large gap means `rss` is flattering: the pages are still committed,
    /// the OS has just paged them out of the resident set.
    pub fn trimmed_bytes(&self) -> Option<u64> {
        self.private_bytes.map(|p| p.saturating_sub(self.rss))
    }
}

#[derive(Debug, Clone)]
pub struct DiskMetrics {
    pub name: String,
    pub mount: String,
    pub total: u64,
    pub free: u64,
}

impl DiskMetrics {
    pub fn free_pct(&self) -> Option<f64> {
        (self.total > 0).then(|| self.free as f64 / self.total as f64 * 100.0)
    }
}

/// Network totals, the delta since the previous snapshot, and the rate that
/// delta works out to. The rate is the number worth alerting on.
#[derive(Debug, Clone)]
pub struct NetMetrics {
    pub rx_total: u64,
    pub tx_total: u64,
    pub rx_delta: u64,
    pub tx_delta: u64,
    /// `None` on the first snapshot: there is no interval to divide by yet.
    pub rx_per_sec: Option<f64>,
    pub tx_per_sec: Option<f64>,
}

/// Disk I/O for this process, as totals and as a rate.
#[derive(Debug, Clone)]
pub struct IoRates {
    pub read_total: u64,
    pub written_total: u64,
    pub read_delta: u64,
    pub written_delta: u64,
    pub read_per_sec: Option<f64>,
    pub written_per_sec: Option<f64>,
}

/// What happened to a watched process since the last snapshot. `Gone` and
/// `Switched` are events in their own right — a restart or a death is exactly
/// what a watcher exists to notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEvent {
    /// First time the pattern matched.
    Found,
    /// Same pattern, different pid — the process restarted.
    Switched { old_pid: u32 },
    /// Was there, now the pattern matches nothing.
    Gone,
}

#[derive(Debug, Clone)]
pub struct WatchedProcess {
    pub pattern: String,
    pub metrics: Option<ProcessMetrics>,
    pub event: Option<WatchEvent>,
}

/// A dependency probe. Ran with a timeout; a check that hangs the app is a bug.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub status: Health,
    pub latency_ms: u64,
    pub message: Option<String>,
}

/// One configured probe.
#[derive(Debug, Clone)]
pub enum Check {
    /// TCP connect to `addr` ("host:port") within `timeout_ms`.
    Tcp {
        name: String,
        addr: String,
        timeout_ms: u64,
    },
    /// At least `min_free` bytes free on the filesystem mounted at `mount`.
    DiskFree {
        name: String,
        mount: String,
        min_free: u64,
    },
    /// Some process matches `pattern` (same matching as `watch`).
    ProcessAlive { name: String, pattern: String },
}

/// Memory attribution for one scope run. Only present when the scope opted in
/// with [`crate::Scope::with_memory`] — sampling costs a syscall per call.
#[derive(Debug, Clone, Copy)]
pub struct ScopeMemory {
    /// Working-set change across the operation, bytes.
    pub d_rss: i64,
    /// Private-bytes change across the operation, bytes.
    pub d_private: i64,
    /// How many scopes overlapped this run. Read this first: the counters are
    /// process-wide, so with `concurrent > 1` the delta includes other work.
    /// Only `concurrent == 1` is a clean attribution.
    pub concurrent: u32,
}

impl ScopeMemory {
    /// Whether this measurement attributes cleanly to one operation.
    pub fn is_isolated(&self) -> bool {
        self.concurrent == 1
    }
}

/// Operations measured through [`crate::Scope`], aggregated per name — one row
/// per operation kind, never one row per call.
///
/// Latency is in microseconds because sub-millisecond operations are real and
/// rounding them to `0 ms` destroys exactly the detail a profile needs. Use the
/// `_ms` helpers when milliseconds read better.
#[derive(Debug, Clone)]
pub struct OpTiming {
    pub name: &'static str,

    // --- lifetime ---
    pub count: u64,
    pub errors: u64,
    pub total_us: u64,
    pub min_us: Option<u64>,
    pub mean_us: Option<f64>,
    /// Typical case. Half the calls were faster than this.
    pub p50_us: Option<u64>,
    /// The number users actually notice.
    pub p95_us: Option<u64>,
    /// The tail. A p99 far above p50 means the average is lying to you.
    pub p99_us: Option<u64>,
    pub max_us: Option<u64>,

    // --- this interval ---
    pub count_delta: u64,
    pub errors_delta: u64,
    /// Throughput over the interval. `None` on the first snapshot.
    pub per_sec: Option<f64>,
    /// Failures as a fraction of calls *in this interval*, 0.0..=1.0. `None`
    /// when nothing ran — a quiet operation has no error rate, and reporting
    /// 0.0 would make silence look like success.
    pub error_rate: Option<f64>,

    /// Memory cost of the most recent run, if the scope opted in.
    pub last_memory: Option<ScopeMemory>,
}

impl OpTiming {
    pub fn p50_ms(&self) -> Option<f64> {
        self.p50_us.map(|v| v as f64 / 1000.0)
    }
    pub fn p95_ms(&self) -> Option<f64> {
        self.p95_us.map(|v| v as f64 / 1000.0)
    }
    pub fn p99_ms(&self) -> Option<f64> {
        self.p99_us.map(|v| v as f64 / 1000.0)
    }
    pub fn max_ms(&self) -> Option<f64> {
        self.max_us.map(|v| v as f64 / 1000.0)
    }
    pub fn total_ms(&self) -> f64 {
        self.total_us as f64 / 1000.0
    }

    /// How far the tail stretches past the typical case.
    ///
    /// Around 1.0 the operation is consistent. A large ratio means a bimodal
    /// operation — a cache that sometimes misses, a lock that sometimes waits —
    /// and averages will never show it.
    ///
    /// A sub-microsecond p50 is floored at 1 µs so the ratio stays defined.
    /// That is the histogram's own resolution rather than an invented number,
    /// and without it the metric would be `None` for exactly the fast
    /// operations whose occasional 100 ms outlier matters most.
    pub fn tail_ratio(&self) -> Option<f64> {
        match (self.p99_us, self.p50_us) {
            (Some(p99), Some(p50)) => Some(p99 as f64 / p50.max(1) as f64),
            _ => None,
        }
    }
}

/// Errors counted through [`crate::note_error`].
#[derive(Debug, Clone, Default)]
pub struct ErrorSummary {
    pub total: u64,
    /// Since the previous snapshot — the number that says whether errors are
    /// happening *now* rather than at some point since boot.
    pub delta: u64,
    pub per_sec: Option<f64>,
    pub per_kind: Vec<(String, u64)>,
    pub last: Option<String>,
}

/// Allocator counters (feature `alloc-obs`).
#[derive(Debug, Clone, Copy)]
pub struct AllocStats {
    /// `allocated - freed`: bytes the allocator holds for live values.
    pub live_bytes: u64,
    pub allocated_total: u64,
    pub freed_total: u64,
    pub alloc_count: u64,
    pub dealloc_count: u64,
    /// Allocation churn over the interval. High churn with flat `live_bytes`
    /// is pure overhead — memory being recycled rather than needed.
    pub allocs_per_sec: Option<f64>,
}

/// Memory seen over time rather than at one instant.
#[derive(Debug, Clone)]
pub struct MemoryHealth {
    /// Working set over the window.
    pub rss: Option<crate::trend::MemoryTrend>,
    /// Private bytes over the window — the honest series on Windows, because
    /// it does not fall when the OS merely trims the working set.
    pub private: Option<crate::trend::MemoryTrend>,
    /// `rss - live_bytes`: pages held by the allocator with no live data in
    /// them. Large and growing means fragmentation or retention, not a leak in
    /// your own data. Needs feature `alloc-obs`.
    pub retention_bytes: Option<u64>,
}

/// Which metric moved, in [`Trigger`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Ram,
    Cpu,
    Disk,
    Net,
}

/// A threshold crossing against the previous snapshot: nothing is reported
/// until something actually moved.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub metric: Metric,
    /// Signed change since the previous snapshot, in `unit`.
    pub delta: f64,
    pub unit: &'static str,
}

/// A rule evaluated against the current snapshot.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub rule: &'static str,
    pub level: Health,
    pub actual: f64,
    pub threshold: f64,
    pub unit: &'static str,
    /// What this is about, when the rule can fire for more than one thing
    /// (which disk, which operation).
    pub subject: Option<String>,
}

/// The one struct a caller gets back. Everything else is a field of it.
#[derive(Debug, Clone)]
pub struct ApmReport {
    pub identity: Identity,
    pub ts_unix_ms: u64,
    pub uptime_secs: u64,
    /// Time since the previous snapshot. `None` on the first one, which is why
    /// every rate beside it is `None` too.
    pub interval_ms: Option<u64>,

    pub system: Option<SystemMetrics>,
    /// This process.
    pub process: Option<ProcessMetrics>,
    /// This process over the window: floor, peak, and growth.
    pub memory: MemoryHealth,
    /// This process's disk I/O as totals and rates.
    pub io: Option<IoRates>,
    /// Processes watched by name.
    pub watched: Vec<WatchedProcess>,
    /// Heaviest processes on the machine, when `Config::top_n` asks for them.
    pub top_processes: Vec<ProcessMetrics>,
    pub disks: Vec<DiskMetrics>,
    pub net: Option<NetMetrics>,
    pub checks: Vec<CheckResult>,
    pub ops: Vec<OpTiming>,
    pub errors: ErrorSummary,
    pub alloc: Option<AllocStats>,
    pub triggers: Vec<Trigger>,
    pub verdicts: Vec<Verdict>,
    pub overall: Health,
}

impl ApmReport {
    pub fn is_healthy(&self) -> bool {
        self.overall.is_healthy()
    }

    /// Everything currently wrong, worst first, as readable lines. Empty when
    /// healthy — so an empty result is itself the answer.
    pub fn problems(&self) -> Vec<String> {
        let mut out: Vec<(Health, String)> = Vec::new();

        for v in &self.verdicts {
            let subject = v
                .subject
                .as_ref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            out.push((
                v.level,
                format!(
                    "{}{}: {:.1}{} (limit {:.1}{})",
                    v.rule, subject, v.actual, v.unit, v.threshold, v.unit
                ),
            ));
        }
        for c in &self.checks {
            if !c.status.is_healthy() {
                let msg = c.message.as_deref().unwrap_or("failed");
                out.push((
                    c.status,
                    format!("check {}: {} ({}ms)", c.name, msg, c.latency_ms),
                ));
            }
        }
        for w in &self.watched {
            match w.event {
                Some(WatchEvent::Gone) => {
                    out.push((Health::Unhealthy, format!("process gone: {}", w.pattern)))
                }
                Some(WatchEvent::Switched { old_pid }) => out.push((
                    Health::Degraded,
                    format!("process restarted: {} (was pid {old_pid})", w.pattern),
                )),
                _ => {}
            }
        }

        // Stable, so equally severe findings keep the order they were
        // collected in: verdicts, then checks, then watch events.
        out.sort_by_key(|(level, _)| std::cmp::Reverse(*level));
        out.into_iter().map(|(_, s)| s).collect()
    }

    /// One line fit for a log or a status bar.
    pub fn summary(&self) -> String {
        let mib = |b: u64| b as f64 / (1 << 20) as f64;
        let rss = self
            .process
            .as_ref()
            .map(|p| format!("{:.0}MiB", mib(p.rss)))
            .unwrap_or_else(|| "?".into());
        let cpu = self
            .process
            .as_ref()
            .map(|p| format!("{:.1}%", p.cpu_pct))
            .unwrap_or_else(|| "?".into());
        let growth = self
            .memory
            .private
            .as_ref()
            .or(self.memory.rss.as_ref())
            .and_then(|t| t.slope_mib_per_hour)
            .map(|s| format!(" growth={s:+.1}MiB/h"))
            .unwrap_or_default();
        let problems = self.problems();
        let tail = if problems.is_empty() {
            String::new()
        } else {
            format!(" problems={}", problems.len())
        };
        format!(
            "{} {:?} rss={rss} cpu={cpu}{growth} ops={} errors={}{tail}",
            self.identity.service,
            self.overall,
            self.ops.len(),
            self.errors.total,
        )
    }

    /// The operation that burned the most wall time. The first place to look
    /// when something is slow and nothing is obviously broken.
    pub fn slowest_op(&self) -> Option<&OpTiming> {
        self.ops.iter().max_by_key(|o| o.total_us)
    }
}
