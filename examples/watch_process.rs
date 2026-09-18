//! Watch a process by name and notice when it appears, restarts, or dies.
//! `cargo run --example watch_process -- "server-*.exe"`

use clove4apm::{Apm, Config};

fn main() {
    let pattern = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "explorer.exe".into());
    println!("watching {pattern} — 5 snapshots, 2s apart");

    let mut apm = Apm::new(Config {
        watch: vec![pattern],
        ..Default::default()
    });

    for _ in 0..5 {
        let r = apm.snapshot();
        for w in &r.watched {
            match &w.metrics {
                Some(m) => println!(
                    "{:<20} event={:?} pid={} rss={:.1} MiB cpu={:.1}%",
                    w.pattern,
                    w.event,
                    m.pid,
                    m.rss as f64 / (1 << 20) as f64,
                    m.cpu_pct
                ),
                None => println!("{:<20} event={:?} not running", w.pattern, w.event),
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
