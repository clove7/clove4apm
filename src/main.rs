//! Standalone demo: prints one report. The library is the product; this is
//! just `cargo run` convenience. Focused demos live in `examples/`.

fn main() {
    let mut apm = clove4apm::Apm::new(clove4apm::Config::for_service("clove4apm-demo").top_n(5));

    // The first snapshot has no previous sample, so every rate in it is `None`
    // — including CPU, which sysinfo also derives from a delta. The second one
    // is the meaningful one.
    let _ = apm.snapshot();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let report = apm.snapshot();

    println!("{report:#?}");
    println!("\n{}", report.summary());
    for problem in report.problems() {
        println!("  ! {problem}");
    }
}
