//! Dependency probes with timeouts, and how they roll up into `overall`.
//! `cargo run --example checks`

use clove4apm::{Apm, Check, Config};

fn main() {
    let mut apm = Apm::new(Config {
        checks: vec![
            Check::Tcp {
                name: "dns".into(),
                addr: "8.8.8.8:53".into(),
                timeout_ms: 500,
            },
            Check::Tcp {
                name: "closed port".into(),
                addr: "127.0.0.1:1".into(),
                timeout_ms: 200,
            },
            Check::DiskFree {
                name: "system drive".into(),
                mount: "C:\\".into(),
                min_free: 10 << 30,
            },
        ],
        ..Default::default()
    });

    let r = apm.snapshot();
    for c in &r.checks {
        println!(
            "{:<14} {:?} in {} ms{}",
            c.name,
            c.status,
            c.latency_ms,
            c.message
                .as_ref()
                .map(|m| format!(" — {m}"))
                .unwrap_or_default()
        );
    }
    println!("overall: {:?}", r.overall);
}
