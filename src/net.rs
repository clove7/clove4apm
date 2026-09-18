//! Network totals across all interfaces, the delta against the previous
//! snapshot, and the rate that delta implies. The rate is what gets alerted on;
//! the totals exist to give it context.

use crate::report::NetMetrics;

pub fn totals(nets: &sysinfo::Networks) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    for (_name, data) in nets {
        rx += data.total_received();
        tx += data.total_transmitted();
    }
    (rx, tx)
}

/// `None` only when there is no interface to read at all.
///
/// Zero traffic is a measurement, not a failure: an idle server genuinely
/// transferring nothing must report zero, or "quiet" and "broken" become the
/// same reading.
pub fn collect(
    nets: &sysinfo::Networks,
    prev: Option<(u64, u64)>,
    interval_secs: Option<f64>,
) -> Option<NetMetrics> {
    nets.iter().next()?;
    let (rx, tx) = totals(nets);
    let (rx_delta, tx_delta) = match prev {
        Some((pr, pt)) => (rx.saturating_sub(pr), tx.saturating_sub(pt)),
        None => (0, 0),
    };
    // Rates need both a previous sample and an interval to divide by.
    let per_sec = |d: u64| {
        prev.and(interval_secs)
            .filter(|s| *s > 0.0)
            .map(|s| d as f64 / s)
    };
    Some(NetMetrics {
        rx_total: rx,
        tx_total: tx,
        rx_delta,
        tx_delta,
        rx_per_sec: per_sec(rx_delta),
        tx_per_sec: per_sec(tx_delta),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_interface_reports_zero_rather_than_nothing() {
        let nets = sysinfo::Networks::new_with_refreshed_list();
        if nets.iter().next().is_none() {
            return; // no interfaces on this box; nothing to assert
        }
        let m = collect(&nets, Some((u64::MAX, u64::MAX)), Some(1.0)).expect("interfaces exist");
        // Totals below `prev` saturate to a zero delta instead of underflowing.
        assert_eq!(m.rx_delta, 0);
        assert_eq!(m.rx_per_sec, Some(0.0), "quiet is a measurement");
    }

    #[test]
    fn the_first_sample_has_no_rate() {
        let nets = sysinfo::Networks::new_with_refreshed_list();
        if nets.iter().next().is_none() {
            return;
        }
        let m = collect(&nets, None, Some(1.0)).unwrap();
        assert_eq!(m.rx_per_sec, None, "nothing to compare against yet");
        assert_eq!(m.rx_delta, 0);
    }
}
