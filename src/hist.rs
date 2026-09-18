//! Bounded latency histogram: percentiles without keeping samples.
//!
//! An average hides the tail, and the tail is what users feel. p50/p95/p99 is
//! the first thing an APM must answer, so it cannot be optional — but keeping
//! every sample to sort later is unbounded memory, which this framework does
//! not do.
//!
//! So values land in log-linear buckets, HDR-histogram style: an exponential
//! bracket split into [`SUB`] linear sub-buckets. Resolution is *relative*,
//! which is what latency needs — 6% of 2 ms is 0.12 ms, 6% of 20 s is 1.2 s,
//! and both are the right precision at their own scale.
//!
//! Cost is fixed: one `u32` per bucket, `BUCKETS` of them, allocated once per
//! operation name and never grown. Recording is an index computation and one
//! add — no allocation, no syscall.

/// Sub-buckets per power of two. 4 bits gives 16, i.e. at most 6.25% relative
/// error — the knob that trades accuracy against the fixed size below.
const SUB_BITS: u32 = 4;
const SUB: usize = 1 << SUB_BITS;

/// Enough brackets for every `u64`: 64 powers of two times [`SUB`].
const BUCKETS: usize = 64 * SUB;

/// Which bucket `v` belongs to. Monotonic in `v`, which is what makes the
/// cumulative walk in [`Histogram::quantile`] correct.
#[inline]
fn index_of(v: u64) -> usize {
    // Below SUB every value is its own bucket: exact, no error at all where
    // sub-microsecond operations live.
    if v < SUB as u64 {
        return v as usize;
    }
    let msb = 63 - v.leading_zeros() as usize;
    let shift = msb - SUB_BITS as usize;
    let sub = ((v >> shift) as usize) & (SUB - 1);
    (msb - SUB_BITS as usize + 1) * SUB + sub
}

/// The largest value that lands in `idx`. Reporting the top of the bucket makes
/// a percentile an upper bound: "99% finished within this", never less than the
/// truth.
#[inline]
fn value_at(idx: usize) -> u64 {
    if idx < SUB {
        return idx as u64;
    }
    let group = idx / SUB;
    let sub = idx % SUB;
    let shift = group - 1;
    // The bucket covers `base ..= base | mask`. Returning the top keeps the
    // percentile an upper bound; returning `base` would quietly under-report
    // every latency by up to one bucket width.
    let base = ((SUB + sub) as u64) << shift;
    base | ((1u64 << shift) - 1)
}

/// Fixed-size distribution. Exact `count`, `min` and `max`; bucketed quantiles.
#[derive(Clone)]
pub struct Histogram {
    buckets: Box<[u32; BUCKETS]>,
    count: u64,
    sum: u64,
    min: u64,
    max: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: Box::new([0; BUCKETS]),
            count: 0,
            sum: 0,
            min: u64::MAX,
            max: 0,
        }
    }
}

impl Histogram {
    pub fn record(&mut self, v: u64) {
        // Saturating: a bucket that overflowed would silently corrupt every
        // percentile after it. Freezing at u32::MAX only flattens the
        // distribution, and that takes 4 billion samples in one bucket.
        let b = &mut self.buckets[index_of(v)];
        *b = b.saturating_add(1);
        self.count += 1;
        self.sum = self.sum.saturating_add(v);
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn sum(&self) -> u64 {
        self.sum
    }

    /// Exact, not bucketed. `None` before the first sample — a zero here would
    /// read as "instant" rather than "never ran".
    pub fn min(&self) -> Option<u64> {
        (self.count > 0).then_some(self.min)
    }

    /// Exact, not bucketed.
    pub fn max(&self) -> Option<u64> {
        (self.count > 0).then_some(self.max)
    }

    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum as f64 / self.count as f64)
    }

    /// Value at quantile `q` (0.0..=1.0), as an upper bound.
    ///
    /// Clamped to the exact `max`, so a percentile can never report a number
    /// the operation did not actually reach.
    pub fn quantile(&self, q: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let q = q.clamp(0.0, 1.0);
        // Rank is 1-based: q=1.0 must select the last sample, not one past it.
        let rank = (q * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &c) in self.buckets.iter().enumerate() {
            seen += c as u64;
            if seen >= rank {
                return Some(value_at(i).min(self.max));
            }
        }
        Some(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_monotonic_and_in_range() {
        let mut last = 0;
        for v in 0..10_000u64 {
            let i = index_of(v);
            assert!(i < BUCKETS, "v={v} idx={i}");
            assert!(i >= last, "not monotonic at {v}");
            last = i;
        }
        // Extremes must not escape the array.
        assert!(index_of(u64::MAX) < BUCKETS);
        assert_eq!(index_of(0), 0);
    }

    #[test]
    fn value_at_round_trips_its_own_bucket() {
        // value_at(index_of(v)) is the top of the bucket, so it is never below v.
        for v in [0u64, 1, 15, 16, 17, 1_000, 999_999, 1 << 40] {
            let back = value_at(index_of(v));
            assert!(back >= v, "v={v} back={back}");
            assert!(
                back <= v + v / (SUB as u64) + 1,
                "v={v} back={back} exceeds the promised resolution"
            );
        }
    }

    #[test]
    fn small_values_are_exact() {
        // Below SUB there is no bucketing, so no error is acceptable there.
        for v in 0..SUB as u64 {
            assert_eq!(value_at(index_of(v)), v);
        }
    }

    #[test]
    fn empty_histogram_reports_nothing_rather_than_zero() {
        let h = Histogram::default();
        assert_eq!(h.count(), 0);
        assert_eq!(h.min(), None);
        assert_eq!(h.max(), None);
        assert_eq!(h.mean(), None);
        assert_eq!(h.quantile(0.5), None);
    }

    #[test]
    fn quantiles_track_a_known_distribution() {
        let mut h = Histogram::default();
        for v in 1..=1000u64 {
            h.record(v);
        }
        assert_eq!(h.count(), 1000);
        assert_eq!(h.min(), Some(1));
        assert_eq!(h.max(), Some(1000));

        // Each quantile is an upper bound within the promised relative error.
        for (q, want) in [(0.5, 500.0), (0.95, 950.0), (0.99, 990.0)] {
            let got = h.quantile(q).unwrap() as f64;
            assert!(got >= want, "p{q} = {got}, must not undershoot {want}");
            let err = (got - want) / want;
            assert!(err <= 1.0 / SUB as f64, "p{q} = {got}, error {err} too big");
        }
    }

    #[test]
    fn p99_sees_the_tail_that_the_mean_hides() {
        // The reason percentiles exist: 980 fast calls and 20 slow ones.
        //
        // 2% slow, so the slow ones own everything above p98 and p99 lands
        // inside them. With exactly 1% slow, p99 would sit precisely on the
        // boundary and still report the fast value — correctly, which is why
        // the tail here is deliberately wider than the quantile being asserted.
        let mut h = Histogram::default();
        for _ in 0..980 {
            h.record(1);
        }
        for _ in 0..20 {
            h.record(10_000);
        }
        assert!(h.mean().unwrap() < 250.0, "the mean looks fine");
        assert_eq!(h.quantile(0.5), Some(1), "half the calls really are fast");
        assert!(
            h.quantile(0.99).unwrap() >= 10_000,
            "p99 must expose the tail"
        );
        assert_eq!(h.max(), Some(10_000));
    }

    #[test]
    fn extremes_are_clamped_to_the_real_max() {
        let mut h = Histogram::default();
        h.record(7);
        // q=1.0 selects the last sample, and bucket tops never exceed the truth.
        assert_eq!(h.quantile(1.0), Some(7));
        assert_eq!(h.quantile(0.0), Some(7));
        // Out-of-range quantiles are clamped, not a panic.
        assert_eq!(h.quantile(-5.0), Some(7));
        assert_eq!(h.quantile(9.0), Some(7));
    }

    #[test]
    fn size_stays_fixed_regardless_of_sample_count() {
        let mut h = Histogram::default();
        for i in 0..100_000u64 {
            h.record(i);
        }
        assert_eq!(std::mem::size_of_val(&*h.buckets), BUCKETS * 4);
        assert_eq!(h.count(), 100_000);
    }
}
