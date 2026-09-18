//! Measure operations and read back the latency distribution.
//! `cargo run --example scope`
//!
//! The point of the output below: `fast_with_a_tail` and `steady` have almost
//! the same mean, and completely different behaviour. Only the percentiles
//! show it.

use clove4apm::{Apm, Config, Scope, measure};
use std::thread::sleep;
use std::time::Duration;

fn main() {
    let mut apm = Apm::new(Config::for_service("scope-demo"));

    // Opt in to memory attribution: costs a syscall each side, worth it for an
    // operation big enough to move the number.
    //
    // The buffer is *returned* from the block, so it outlives the scope and is
    // still alive when the scope records. Declaring the scope first and the
    // buffer second would drop the buffer first (reverse drop order) and
    // measure it after the free — see the `held_wrong` row below.
    let held: Vec<u8> = {
        let _s = Scope::with_memory("allocate_64mib");
        let v: Vec<u8> = vec![0; 64 << 20];
        std::hint::black_box(&v);
        v
    };

    // The same allocation measured the wrong way round, for contrast.
    {
        let _s = Scope::with_memory("allocate_64mib_freed_first");
        let v: Vec<u8> = vec![0; 64 << 20];
        std::hint::black_box(&v);
    }

    // Consistent: every call costs about the same.
    for _ in 0..40 {
        let _s = Scope::new("steady");
        sleep(Duration::from_millis(5));
    }

    // Bimodal: usually instant, occasionally terrible. An average calls this
    // healthy; p99 does not.
    for i in 0..40 {
        let _s = Scope::new("fast_with_a_tail");
        sleep(Duration::from_millis(if i % 20 == 0 { 100 } else { 1 }));
    }

    // Fallible work records its own outcome.
    for i in 0..10 {
        let _: Result<(), ()> =
            measure(
                "sometimes_fails",
                || if i % 5 == 0 { Err(()) } else { Ok(()) },
            );
    }

    let r = apm.snapshot();

    println!(
        "{:<18} {:>6} {:>7} {:>8} {:>8} {:>8} {:>8} {:>6}",
        "op", "count", "errors", "mean_ms", "p50_ms", "p95_ms", "p99_ms", "tail"
    );
    for op in &r.ops {
        let ms = |v: Option<u64>| v.map(|x| x as f64 / 1000.0).unwrap_or(f64::NAN);
        println!(
            "{:<18} {:>6} {:>7} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>5.1}x",
            op.name,
            op.count,
            op.errors,
            op.mean_us.unwrap_or(f64::NAN) / 1000.0,
            ms(op.p50_us),
            ms(op.p95_us),
            ms(op.p99_us),
            op.tail_ratio().unwrap_or(f64::NAN),
        );
    }

    println!();
    for op in &r.ops {
        if let Some(m) = op.last_memory {
            println!(
                "{}: d_rss={:+.1}MiB d_private={:+.1}MiB concurrent={} clean={}",
                op.name,
                m.d_rss as f64 / (1 << 20) as f64,
                m.d_private as f64 / (1 << 20) as f64,
                m.concurrent,
                m.is_isolated(),
            );
        }
    }

    drop(held);

    if let Some(slow) = r.slowest_op() {
        println!(
            "\nmost wall time: {} ({:.0} ms total)",
            slow.name,
            slow.total_ms()
        );
    }
}
