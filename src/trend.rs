//! Memory trend over a rolling window — the difference between "busy" and
//! "leaking".
//!
//! A single memory reading cannot tell those apart, and neither can a peak: a
//! process that spikes to 2 GB and returns to 400 MB is healthy, while one
//! whose *quiet* level creeps from 400 MB to 900 MB is leaking even if it never
//! spikes at all.
//!
//! So the signal is the **floor**: the lowest level the process returns to. A
//! rising floor is a leak; a high peak with a flat floor is just work. The
//! slope is fitted by least squares over the window and reported in MiB/hour,
//! which is the unit the question is actually asked in — "will this survive the
//! night?"
//!
//! Bounded like everything else here: a fixed ring of [`CAPACITY`] samples,
//! oldest overwritten. No growth, no files.

/// Samples kept. At one snapshot every 30 s this covers four hours, which is
/// long enough for a slow leak to show a slope and short enough that a fixed
/// buffer stays small.
const CAPACITY: usize = 512;

/// Least-squares fit needs enough points to mean anything; below this the slope
/// is noise wearing a number's clothes.
const MIN_SAMPLES_FOR_SLOPE: usize = 8;

/// A window must span real time before a per-hour rate is honest. Five minutes
/// of data extrapolated to an hour is a 12x multiplication of noise.
const MIN_SPAN_SECS: f64 = 300.0;

/// Time segments the window is split into before taking each one's minimum.
/// The slope is fitted to those minima — see [`Trend::slope_mib_per_hour`].
const SEGMENTS: usize = 8;

/// Segments that must actually contain a sample. Fitting a line through two
/// points is not a trend.
const MIN_SEGMENTS: usize = 4;

#[derive(Debug, Clone, Copy)]
struct Point {
    t_secs: f64,
    bytes: u64,
}

/// Rolling memory history for one series (working set, or private bytes).
#[derive(Debug, Clone)]
pub struct Trend {
    ring: Vec<Point>,
    next: usize,
}

impl Default for Trend {
    fn default() -> Self {
        Trend {
            ring: Vec::with_capacity(CAPACITY),
            next: 0,
        }
    }
}

impl Trend {
    pub fn record(&mut self, t_secs: f64, bytes: u64) {
        let p = Point { t_secs, bytes };
        if self.ring.len() < CAPACITY {
            self.ring.push(p);
        } else {
            self.ring[self.next] = p;
            self.next = (self.next + 1) % CAPACITY;
        }
    }

    /// Summarise the window. `None` until there is at least one sample.
    pub fn summary(&self) -> Option<MemoryTrend> {
        if self.ring.is_empty() {
            return None;
        }
        let floor = self.ring.iter().map(|p| p.bytes).min().unwrap_or(0);
        let peak = self.ring.iter().map(|p| p.bytes).max().unwrap_or(0);

        let t_min = self.ring.iter().map(|p| p.t_secs).fold(f64::MAX, f64::min);
        let t_max = self.ring.iter().map(|p| p.t_secs).fold(f64::MIN, f64::max);
        let span = (t_max - t_min).max(0.0);

        Some(MemoryTrend {
            samples: self.ring.len() as u32,
            window_secs: span as u64,
            floor,
            peak,
            slope_mib_per_hour: self.slope_mib_per_hour(span),
        })
    }

    /// Least-squares slope of the **floor** against time, in MiB/hour.
    ///
    /// Fitted to per-segment minima, not to raw samples. Fitting raw samples
    /// makes the answer depend on where the spikes happen to land: a pure
    /// sawtooth that always returns to the same level reads as tens of MiB per
    /// hour of "growth" purely because each high sample sits later in time than
    /// the low one before it. Segment minima strip the spikes out and leave the
    /// resting level, which is the thing that actually has to stop rising.
    ///
    /// `None` rather than a number whenever the window cannot support one —
    /// too few points, too short a span, too few segments, or every sample at
    /// the same instant. A fabricated trend is the one output that would make
    /// this module worse than having no module.
    fn slope_mib_per_hour(&self, span: f64) -> Option<f64> {
        if self.ring.len() < MIN_SAMPLES_FOR_SLOPE || span < MIN_SPAN_SECS {
            return None;
        }
        let floors = self.segment_floors(span)?;

        let n = floors.len() as f64;
        let mean_t = floors.iter().map(|p| p.t_secs).sum::<f64>() / n;
        let mean_y = floors.iter().map(|p| p.bytes as f64).sum::<f64>() / n;

        let mut num = 0.0;
        let mut den = 0.0;
        for p in &floors {
            let dt = p.t_secs - mean_t;
            num += dt * (p.bytes as f64 - mean_y);
            den += dt * dt;
        }
        if den <= f64::EPSILON {
            return None;
        }
        // bytes/second -> MiB/hour
        let bytes_per_sec = num / den;
        Some(bytes_per_sec * 3600.0 / (1 << 20) as f64)
    }

    /// The lowest sample in each time segment, carrying its own timestamp
    /// rather than the segment midpoint — every returned point is a real
    /// observation. `None` if too few segments saw any traffic.
    fn segment_floors(&self, span: f64) -> Option<Vec<Point>> {
        let t0 = self.ring.iter().map(|p| p.t_secs).fold(f64::MAX, f64::min);
        let width = span / SEGMENTS as f64;
        if width <= 0.0 {
            return None;
        }

        let mut best: Vec<Option<Point>> = vec![None; SEGMENTS];
        for p in &self.ring {
            let k = (((p.t_secs - t0) / width) as usize).min(SEGMENTS - 1);
            match best[k] {
                Some(cur) if cur.bytes <= p.bytes => {}
                _ => best[k] = Some(*p),
            }
        }

        let floors: Vec<Point> = best.into_iter().flatten().collect();
        (floors.len() >= MIN_SEGMENTS).then_some(floors)
    }
}

/// What the window says. Read `floor` and `slope_mib_per_hour` together: a
/// rising floor is the leak signal, and `peak` alone never is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryTrend {
    pub samples: u32,
    /// Time actually covered, not the nominal window.
    pub window_secs: u64,
    /// Lowest value in the window — the level the process returns to.
    pub floor: u64,
    pub peak: u64,
    /// Fitted growth in MiB/hour. `None` when the window is too short or too
    /// sparse to support one.
    pub slope_mib_per_hour: Option<f64>,
}

impl MemoryTrend {
    /// Headroom the spikes need above the resting level.
    pub fn spike_headroom(&self) -> u64 {
        self.peak.saturating_sub(self.floor)
    }

    /// Whether the floor is climbing faster than `limit` MiB/hour.
    ///
    /// `false` while the slope is unknown: this answers "do I have evidence of
    /// a leak", and no evidence is not a leak.
    pub fn is_growing(&self, limit_mib_per_hour: f64) -> bool {
        self.slope_mib_per_hour
            .is_some_and(|s| s > limit_mib_per_hour)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn feed(points: impl IntoIterator<Item = (f64, u64)>) -> Trend {
        let mut t = Trend::default();
        for (s, b) in points {
            t.record(s, b);
        }
        t
    }

    #[test]
    fn empty_trend_says_nothing() {
        assert!(Trend::default().summary().is_none());
    }

    #[test]
    fn floor_and_peak_are_the_window_extremes() {
        let t = feed([(0.0, 500 * MIB), (60.0, 900 * MIB), (120.0, 520 * MIB)]);
        let s = t.summary().unwrap();
        assert_eq!(s.floor, 500 * MIB);
        assert_eq!(s.peak, 900 * MIB);
        assert_eq!(s.spike_headroom(), 400 * MIB);
        assert_eq!(s.samples, 3);
        assert_eq!(s.window_secs, 120);
    }

    #[test]
    fn a_short_window_refuses_to_guess_a_slope() {
        // Three points over 2 minutes is not an hourly rate.
        let t = feed([(0.0, 100 * MIB), (60.0, 200 * MIB), (120.0, 300 * MIB)]);
        assert_eq!(t.summary().unwrap().slope_mib_per_hour, None);
    }

    #[test]
    fn steady_sawtooth_is_not_a_leak() {
        // Spikes to 1 GiB every other sample, always returning to 400 MiB.
        let pts: Vec<(f64, u64)> = (0..40)
            .map(|i| {
                let t = i as f64 * 60.0;
                let b = if i % 2 == 0 { 400 * MIB } else { 1024 * MIB };
                (t, b)
            })
            .collect();
        let s = feed(pts).summary().unwrap();
        assert_eq!(s.floor, 400 * MIB);
        assert_eq!(s.peak, 1024 * MIB);
        let slope = s.slope_mib_per_hour.expect("long enough window");
        assert!(slope.abs() < 10.0, "sawtooth read as a trend: {slope}");
        assert!(!s.is_growing(10.0));
    }

    #[test]
    fn a_climbing_floor_is_detected_in_mib_per_hour() {
        // +60 MiB/hour: one MiB per minute, sampled each minute for an hour.
        let pts: Vec<(f64, u64)> = (0..60)
            .map(|i| (i as f64 * 60.0, 400 * MIB + i as u64 * MIB))
            .collect();
        let s = feed(pts).summary().unwrap();
        let slope = s.slope_mib_per_hour.expect("slope");
        assert!(
            (slope - 60.0).abs() < 1.0,
            "expected ~60 MiB/h, got {slope}"
        );
        assert!(s.is_growing(10.0));
        assert!(!s.is_growing(100.0));
    }

    #[test]
    fn falling_memory_gives_a_negative_slope() {
        let pts: Vec<(f64, u64)> = (0..60)
            .map(|i| (i as f64 * 60.0, 1000 * MIB - i as u64 * MIB))
            .collect();
        let s = feed(pts).summary().unwrap();
        assert!(s.slope_mib_per_hour.unwrap() < -50.0);
        assert!(!s.is_growing(0.0));
    }

    #[test]
    fn unknown_slope_never_counts_as_growing() {
        let s = MemoryTrend {
            samples: 2,
            window_secs: 10,
            floor: 0,
            peak: 0,
            slope_mib_per_hour: None,
        };
        assert!(!s.is_growing(0.0), "no evidence is not a leak");
    }

    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest() {
        let mut t = Trend::default();
        for i in 0..(CAPACITY * 3) {
            t.record(i as f64, i as u64);
        }
        let s = t.summary().unwrap();
        assert_eq!(s.samples as usize, CAPACITY, "the ring is bounded");
        // Oldest samples were overwritten, so the floor has moved up with them.
        assert_eq!(s.peak, (CAPACITY * 3 - 1) as u64);
        assert_eq!(s.floor, (CAPACITY * 2) as u64);
    }
}
