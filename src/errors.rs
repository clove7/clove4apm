//! Error counters. RAM only: atomics plus the last message. Nothing is written
//! anywhere; the counts die with the process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::report::ErrorSummary;

static TOTAL: AtomicU64 = AtomicU64::new(0);
/// One global lock for both maps; error paths are cold by definition.
static KINDS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static LAST: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

/// Count one error. `kind` groups it ("db", "io", ...), `msg` becomes `last`.
pub fn note(kind: &str, msg: impl std::fmt::Display) {
    TOTAL.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut k) = KINDS.lock() {
        *k.entry(kind.to_string()).or_insert(0) += 1;
    }
    if let Ok(mut l) = LAST.lock() {
        *l = Some(format!("{kind}: {msg}"));
    }
}

/// `prev` is the total at the previous snapshot; it is updated in place so the
/// summary can report what happened in *this* interval, not only since boot.
pub fn summary(prev: &mut u64, interval_secs: Option<f64>) -> ErrorSummary {
    let per_kind = KINDS
        .lock()
        .map(|k| {
            let mut v: Vec<(String, u64)> = k.iter().map(|(k, v)| (k.clone(), *v)).collect();
            // Most frequent first, ties broken by name so the order is stable
            // between snapshots instead of shuffling with HashMap iteration.
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v
        })
        .unwrap_or_default();
    let last = LAST.lock().ok().and_then(|l| l.clone());

    let total = TOTAL.load(Ordering::Relaxed);
    let delta = total.saturating_sub(*prev);
    *prev = total;

    ErrorSummary {
        total,
        delta,
        per_sec: interval_secs.filter(|s| *s > 0.0).map(|s| delta as f64 / s),
        per_kind,
        last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_counts_only_the_interval() {
        // The counters are process-global and other tests call `note` while
        // this one runs, so absolute totals are not assertable. The contract
        // that *is* exact regardless of concurrency is the relationship
        // between consecutive summaries: each delta is the growth since the
        // previous one, and the deltas therefore sum back to the total.
        let mut prev = 0;
        let first = summary(&mut prev, None);

        note("t_delta", "one");
        note("t_delta", "two");

        let second = summary(&mut prev, Some(2.0));
        assert_eq!(
            second.delta,
            second.total - first.total,
            "delta must be exactly the growth since the previous summary"
        );
        assert!(second.delta >= 2, "this test alone added two");
        assert_eq!(second.per_sec, Some(second.delta as f64 / 2.0));
        // `last` is a single global slot — whichever test noted most recently
        // owns it, so only its presence can be asserted from a parallel run.
        assert!(second.last.is_some());

        let third = summary(&mut prev, Some(2.0));
        assert_eq!(third.delta, third.total - second.total);
        assert_eq!(third.per_sec, Some(third.delta as f64 / 2.0));
        // Lifetime totals never go backwards.
        assert!(third.total >= second.total);
    }

    #[test]
    fn summary_advances_the_baseline_it_was_given() {
        // This is what makes a quiet interval report zero: the baseline is left
        // sitting exactly on the total just reported, so the next summary sees
        // only what happened after it. Exact under concurrency, unlike any
        // assertion about the total itself.
        let mut prev = 0;
        let s = summary(&mut prev, Some(4.0));
        assert_eq!(prev, s.total);
        assert_eq!(s.per_sec, Some(s.delta as f64 / 4.0));
    }

    #[test]
    fn no_interval_means_no_rate() {
        let mut prev = 0;
        assert_eq!(summary(&mut prev, None).per_sec, None);
        // A zero-length interval would divide by zero; it reports nothing too.
        assert_eq!(summary(&mut prev, Some(0.0)).per_sec, None);
    }

    #[test]
    fn kinds_are_ordered_stably() {
        for _ in 0..5 {
            note("t_kind_a", "x");
        }
        note("t_kind_b", "x");
        let mut prev = 0;
        let s = summary(&mut prev, None);
        let a = s.per_kind.iter().position(|(k, _)| k == "t_kind_a");
        let b = s.per_kind.iter().position(|(k, _)| k == "t_kind_b");
        assert!(a < b, "more frequent kind must sort first");
    }
}
