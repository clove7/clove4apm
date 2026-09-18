//! The two calls most consumers actually need: `summary()` for a log line and
//! `problems()` for what is wrong right now.
//! `cargo run --example health`
//!
//! Also shows the shape of a real polling loop: the first snapshot has no
//! interval, so every rate in it is `None`. Take a second one before believing
//! any rate — including CPU.

use clove4apm::{Apm, Config, Limits};

fn main() {
    let mut apm = Apm::new(
        Config::for_service("health-demo")
            .version("1.2.3")
            .top_n(5)
            .limits(Limits {
                // Deliberately strict so the demo has something to report on a
                // normal machine.
                cpu_pct: 20.0,
                cpu_pct_critical: 90.0,
                mem_growth_mib_per_hour: 20.0,
                ..Default::default()
            }),
    );

    for i in 0..3 {
        let r = apm.snapshot();

        println!("--- snapshot {i} ---");
        println!("{}", r.summary());

        match r.interval_ms {
            None => println!("  (first snapshot: no interval yet, so rates are None)"),
            Some(ms) => println!("  interval: {ms} ms"),
        }

        if let Some(p) = &r.process {
            let mib = |b: u64| b as f64 / (1 << 20) as f64;
            println!(
                "  rss={:.0} MiB private={} peak={} handles={} faults={}",
                mib(p.rss),
                p.private_bytes
                    .map(|v| format!("{:.0} MiB", mib(v)))
                    .unwrap_or("?".into()),
                p.peak_rss
                    .map(|v| format!("{:.0} MiB", mib(v)))
                    .unwrap_or("?".into()),
                p.handles.map(|v| v.to_string()).unwrap_or("?".into()),
                p.page_faults.map(|v| v.to_string()).unwrap_or("?".into()),
            );
            if let Some(t) = p.trimmed_bytes() {
                println!("  working set trimmed below committed by {:.0} MiB", mib(t));
            }
        }

        if let Some(t) = &r.memory.private.as_ref().or(r.memory.rss.as_ref()) {
            println!(
                "  memory window: {} samples over {}s floor={:.0}MiB peak={:.0}MiB growth={}",
                t.samples,
                t.window_secs,
                t.floor as f64 / (1 << 20) as f64,
                t.peak as f64 / (1 << 20) as f64,
                match t.slope_mib_per_hour {
                    Some(s) => format!("{s:+.1} MiB/h"),
                    // Honest: a few seconds of data cannot support an hourly rate.
                    None => "unknown (window too short)".into(),
                }
            );
        }

        let problems = r.problems();
        if problems.is_empty() {
            println!("  no problems");
        } else {
            for p in &problems {
                println!("  ! {p}");
            }
        }

        if !r.top_processes.is_empty() {
            println!("  heaviest on this machine:");
            for p in r.top_processes.iter().take(3) {
                println!(
                    "    {:<28} {:>7.0} MiB",
                    p.name,
                    p.rss as f64 / (1 << 20) as f64
                );
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(800));
    }
}
