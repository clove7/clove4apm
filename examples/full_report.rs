//! Everything at once: system, self process, a watched process, checks,
//! triggers, verdicts. `cargo run --example full_report`

use clove4apm::{Apm, Check, Config};

fn main() {
    let mut apm = Apm::new(Config {
        service: "demo".into(),
        watch: vec!["explorer.exe".into()],
        checks: vec![
            Check::Tcp {
                name: "local web".into(),
                addr: "127.0.0.1:80".into(),
                timeout_ms: 300,
            },
            Check::DiskFree {
                name: "system drive".into(),
                mount: "C:\\".into(),
                min_free: 1 << 30,
            },
            Check::ProcessAlive {
                name: "shell".into(),
                pattern: "explorer.exe".into(),
            },
        ],
        ..Default::default()
    });

    // First snapshot primes the CPU counters; the second one is meaningful.
    let _ = apm.snapshot();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let r = apm.snapshot();
    println!("{r:#?}");
}
