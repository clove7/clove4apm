//! `Scope` measures what one operation cost and records it on drop — covering
//! every exit path including `?`, early returns and panics. Results are
//! aggregated per name in RAM (one row per operation kind, never one per call)
//! and read out by the next snapshot. Nothing is written anywhere.
//!
//! # Cost
//!
//! The default path is a timestamp, a lock and a histogram increment: no
//! allocation, no syscall. Memory attribution is **opt-in** via
//! [`Scope::with_memory`] because sampling process counters costs a syscall on
//! entry and another on exit — affordable for a catalog reload that runs every
//! few minutes, not for an operation in a hot loop.
//!
//! # Reading the memory numbers
//!
//! Check [`ScopeMemory::is_isolated`] before trusting `d_rss`/`d_private`. The
//! counters are process-wide, so overlapping scopes attribute each other's
//! work; only a run with no peers measured itself alone.
//!
//! # Drop order is part of the measurement
//!
//! A scope measures until **it** drops, and Rust drops locals in reverse
//! declaration order. A buffer declared after the scope is therefore freed
//! *before* the scope records:
//!
//! ```ignore
//! let _s = Scope::with_memory("load");   // declared first...
//! let data = load_everything();          // ...so this is dropped FIRST
//! ```
//!
//! Keep the data alive past the scope when you want the cost of holding it:
//!
//! ```ignore
//! let data = {
//!     let _s = Scope::with_memory("load");
//!     load_everything()                  // returned, so it outlives the scope
//! };
//! ```
//!
//! What the freed case then reports is **not** reliably zero, which is the
//! second half of the lesson: `d_private` is an OS counter, and freeing memory
//! does not oblige the allocator to hand the pages back. A buffer that was
//! dropped can still show at full size. So `d_private` answers "did the process
//! grow", never "is this data still live" — `retention_bytes` on the report
//! (feature `alloc-obs`) is what separates the two.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use crate::hist::Histogram;
use crate::process;
use crate::report::{OpTiming, ScopeMemory};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// One global lock. Scope drops are short and touch no I/O, and a per-name lock
/// would cost more in bookkeeping than it saves in contention.
static OPS: LazyLock<Mutex<HashMap<&'static str, Agg>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn ops() -> Option<MutexGuard<'static, HashMap<&'static str, Agg>>> {
    // A poisoned lock means some other thread panicked mid-record. The
    // aggregate may have lost one update; that is not a reason to take the
    // application down with it, so recover and carry on.
    match OPS.lock() {
        Ok(g) => Some(g),
        Err(poisoned) => Some(poisoned.into_inner()),
    }
}

#[derive(Default)]
struct Agg {
    hist: Histogram,
    errors: u64,
    last_memory: Option<ScopeMemory>,
}

/// Measures one operation. Records on drop.
pub struct Scope {
    name: &'static str,
    started: Instant,
    /// `(rss, private)` at entry — `None` unless the caller opted in.
    mem0: Option<(u64, u64)>,
    peers: usize,
    failed: bool,
}

impl Scope {
    /// Time this operation. No syscalls; see [`Scope::with_memory`] to also
    /// attribute memory to it.
    pub fn new(name: &'static str) -> Self {
        Scope {
            name,
            started: Instant::now(),
            mem0: None,
            peers: ACTIVE.fetch_add(1, Ordering::Relaxed),
            failed: false,
        }
    }

    /// Time this operation *and* attribute memory to it.
    ///
    /// Costs one process-counter syscall here and one on drop. Worth it for
    /// operations big enough to move memory; wasteful for anything smaller.
    pub fn with_memory(name: &'static str) -> Self {
        let peers = ACTIVE.fetch_add(1, Ordering::Relaxed);
        let (rss, private, _) = process::mem_counters();
        Scope {
            name,
            started: Instant::now(),
            mem0: Some((rss, private)),
            peers,
            failed: false,
        }
    }

    /// Mark this run as failed. Counts toward the operation's error rate.
    pub fn fail(&mut self) {
        self.failed = true;
    }

    /// Record the outcome from a `Result` without unwrapping it.
    pub fn set_ok(&mut self, ok: bool) {
        self.failed = !ok;
    }

    /// Elapsed so far. Useful for logging a slow path before the scope ends.
    pub fn elapsed_us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
        let us = self.started.elapsed().as_micros() as u64;

        let memory = self.mem0.map(|(rss0, priv0)| {
            let (rss1, priv1, _) = process::mem_counters();
            ScopeMemory {
                d_rss: rss1 as i64 - rss0 as i64,
                d_private: priv1 as i64 - priv0 as i64,
                concurrent: self.peers as u32 + 1,
            }
        });

        if let Some(mut ops) = ops() {
            let a = ops.entry(self.name).or_default();
            a.hist.record(us);
            if self.failed {
                a.errors += 1;
            }
            if memory.is_some() {
                a.last_memory = memory;
            }
        }
    }
}

/// Measure a fallible operation, recording success or failure automatically.
///
/// ```
/// # use clove4apm::measure;
/// let out: Result<u32, &str> = measure("parse", || "7".parse::<u32>().map_err(|_| "bad"));
/// assert_eq!(out, Ok(7));
/// ```
pub fn measure<T, E>(name: &'static str, f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let mut s = Scope::new(name);
    let out = f();
    s.set_ok(out.is_ok());
    out
}

/// Counters carried between snapshots so each report can show what happened in
/// *this* interval, not only since boot.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrevOp {
    pub count: u64,
    pub errors: u64,
}

/// Aggregated timings, heaviest total time first.
///
/// `prev` holds the counts from the previous snapshot and is updated in place,
/// which is what turns lifetime counters into per-interval rates.
pub fn snapshot(
    prev: &mut HashMap<&'static str, PrevOp>,
    interval_secs: Option<f64>,
) -> Vec<OpTiming> {
    let Some(ops) = ops() else {
        return Vec::new();
    };

    let mut out: Vec<OpTiming> = ops
        .iter()
        .map(|(name, a)| {
            let count = a.hist.count();
            let was = prev.get(name).copied().unwrap_or_default();
            let count_delta = count.saturating_sub(was.count);
            let errors_delta = a.errors.saturating_sub(was.errors);
            prev.insert(
                name,
                PrevOp {
                    count,
                    errors: a.errors,
                },
            );

            OpTiming {
                name,
                count,
                errors: a.errors,
                total_us: a.hist.sum(),
                min_us: a.hist.min(),
                mean_us: a.hist.mean(),
                p50_us: a.hist.quantile(0.50),
                p95_us: a.hist.quantile(0.95),
                p99_us: a.hist.quantile(0.99),
                max_us: a.hist.max(),
                count_delta,
                errors_delta,
                per_sec: interval_secs
                    .filter(|s| *s > 0.0)
                    .map(|s| count_delta as f64 / s),
                // A quiet operation has no error rate. Reporting 0.0 would make
                // "nothing ran" indistinguishable from "everything succeeded".
                error_rate: (count_delta > 0).then(|| errors_delta as f64 / count_delta as f64),
                last_memory: a.last_memory,
            }
        })
        .collect();

    // Heaviest total wall time first: the profile question is "where did
    // the time go", not "what is slowest per call".
    out.sort_by_key(|o| std::cmp::Reverse(o.total_us));
    out
}

/// Forget every recorded operation. Mainly for tests and for a caller that
/// wants a clean window; snapshots already give per-interval numbers without it.
pub fn reset() {
    if let Some(mut ops) = ops() {
        ops.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global, so tests must not assert on rows other
    /// tests own. Each one uses its own operation name.
    fn row(name: &str) -> OpTiming {
        let mut prev = HashMap::new();
        snapshot(&mut prev, None)
            .into_iter()
            .find(|o| o.name == name)
            .expect("row")
    }

    #[test]
    fn drop_records_one_aggregated_row() {
        for _ in 0..2 {
            let _s = Scope::new("t_aggregated");
        }
        let r = row("t_aggregated");
        assert_eq!(r.count, 2, "two calls, one row");
        assert_eq!(r.errors, 0);
        assert!(r.p50_us.is_some());
    }

    #[test]
    fn latency_percentiles_are_populated() {
        for _ in 0..20 {
            let _s = Scope::new("t_latency");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let r = row("t_latency");
        assert_eq!(r.count, 20);
        // 2 ms of sleep is at least 2000 us; scheduling makes it more, never less.
        assert!(r.p50_us.unwrap() >= 2_000, "p50 = {:?}", r.p50_us);
        assert!(r.max_us.unwrap() >= r.p50_us.unwrap());
        assert!(r.min_us.unwrap() <= r.p50_us.unwrap());
        assert!(r.p99_ms().unwrap() >= 2.0);
    }

    #[test]
    fn failures_are_counted_but_quiet_ops_have_no_rate() {
        {
            let mut s = Scope::new("t_failures");
            s.fail();
        }
        {
            let _s = Scope::new("t_failures");
        }
        let mut prev = HashMap::new();
        let first = snapshot(&mut prev, Some(1.0))
            .into_iter()
            .find(|o| o.name == "t_failures")
            .unwrap();
        assert_eq!(first.count, 2);
        assert_eq!(first.errors, 1);
        assert_eq!(first.error_rate, Some(0.5));
        assert_eq!(first.per_sec, Some(2.0));

        // Nothing ran in the second interval: no rate at all, not a zero.
        let second = snapshot(&mut prev, Some(1.0))
            .into_iter()
            .find(|o| o.name == "t_failures")
            .unwrap();
        assert_eq!(second.count, 2, "lifetime count is unchanged");
        assert_eq!(second.count_delta, 0);
        assert_eq!(second.error_rate, None, "silence is not success");
        assert_eq!(second.per_sec, Some(0.0));
    }

    #[test]
    fn measure_records_the_error_outcome() {
        let ok: Result<u8, u8> = measure("t_measure", || Ok(1));
        assert!(ok.is_ok());
        let err: Result<u8, u8> = measure("t_measure", || Err(2));
        assert!(err.is_err());
        let r = row("t_measure");
        assert_eq!(r.count, 2);
        assert_eq!(r.errors, 1);
    }

    #[test]
    fn memory_is_absent_unless_opted_in() {
        {
            let _s = Scope::new("t_no_mem");
        }
        assert!(row("t_no_mem").last_memory.is_none());
    }

    #[test]
    fn is_isolated_is_exactly_the_no_peers_case() {
        // Pure logic, no globals — the integration tests below cannot assert
        // this reliably because `ACTIVE` is process-wide and other tests run
        // their own scopes at the same time.
        let alone = ScopeMemory {
            d_rss: 0,
            d_private: 0,
            concurrent: 1,
        };
        let shared = ScopeMemory {
            d_rss: 0,
            d_private: 0,
            concurrent: 2,
        };
        assert!(alone.is_isolated());
        assert!(!shared.is_isolated());
    }

    #[test]
    fn opting_in_attributes_memory_that_is_still_alive() {
        let _guard = crate::memory_test_lock();
        // The buffer must outlive the scope, or reverse drop order frees it
        // first and the scope measures nothing. See the module docs.
        let v: Vec<u8> = {
            let _s = Scope::with_memory("t_mem");
            let v: Vec<u8> = vec![7; 32 << 20];
            std::hint::black_box(&v);
            v
        };
        let m = row("t_mem").last_memory.expect("memory was requested");
        assert!(m.concurrent >= 1);
        #[cfg(any(windows, target_os = "linux"))]
        assert!(m.d_private > 0, "32 MiB should show: {}", m.d_private);
        drop(v);
    }

    #[test]
    fn a_freed_buffer_still_records_a_measurement() {
        let _guard = crate::memory_test_lock();
        // Declaring the scope first means the buffer drops first, so this
        // records after the free. The measurement still exists — and is not
        // reliably zero, because the allocator is under no obligation to return
        // the pages to the OS. That gap is what `retention_bytes` is for, and
        // asserting a direction here would be asserting allocator policy.
        {
            let _s = Scope::with_memory("t_drop_order");
            let v: Vec<u8> = vec![7; 32 << 20];
            std::hint::black_box(&v);
        }
        assert!(row("t_drop_order").last_memory.is_some());
    }

    #[test]
    fn nested_scopes_report_that_they_overlapped() {
        let _outer = Scope::with_memory("t_outer");
        {
            let _inner = Scope::with_memory("t_inner");
        }
        let m = row("t_inner").last_memory.unwrap();
        assert!(m.concurrent >= 2, "inner ran inside outer");
        assert!(!m.is_isolated(), "an overlapping measurement is not clean");
    }

    #[test]
    fn tail_ratio_exposes_a_bimodal_operation() {
        for _ in 0..50 {
            let _s = Scope::new("t_bimodal");
        }
        {
            let _s = Scope::new("t_bimodal");
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let r = row("t_bimodal");
        assert!(r.tail_ratio().unwrap() > 2.0, "ratio {:?}", r.tail_ratio());
    }
}
