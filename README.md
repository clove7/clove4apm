# clove4apm

Application Performance Monitoring that returns a struct.

> **Under active development.** Pre-1.0, so the API can still change between
> releases. Feedback, issues and contributions to the framework are welcome —
> especially reports of a metric that read wrong on your workload.

No storage, no files, no writers, no background threads. You call `snapshot()`,
you get an `ApmReport`. What happens to it — logged, served on `/health`,
shipped somewhere, ignored — is entirely yours.

```rust
let mut apm = Apm::new(Config::for_service("admin"));

let report = apm.snapshot();
println!("{}", report.summary());
for problem in report.problems() {
    eprintln!("  {problem}");
}
```

```
admin Degraded rss=793MiB cpu=2.1% growth=+18.4MiB/h ops=6 errors=0 problems=2
  memory_growth: 18.4MiB/h (limit 20.0MiB/h)
  op_errors [catalog_reconcile]: 4.0% (limit 1.0%)
```

## Four rules it keeps

**`None` means unmeasured, never zero.** A guessed number is worse than no
number, because it gets believed. Every probe that cannot measure says so.

**Cumulative counters always come with their interval delta.** A lifetime total
answers "how much since boot". Only the delta answers "what is happening now",
and that is the question a monitor is asked.

**Percentiles, not averages.** An average hides the tail, and the tail is what
users feel.

**The floor, not the peak, is the leak signal.**

## What it measures

| Area | What you get |
|---|---|
| **Operations** | count, errors, p50/p95/p99/max, throughput, error rate, tail ratio |
| **Memory** | working set *and* private bytes, peak, floor/peak over a window, growth in MiB/h |
| **Process** | CPU, CPU time, threads, open handles, page faults, disk I/O + rates |
| **System** | per-core CPU, RAM, swap, load average, disks |
| **Network** | totals, deltas and per-second rates |
| **Watched processes** | found / restarted / **gone**, by name pattern |
| **Checks** | TCP connect, disk free, process alive — each with its own latency |
| **Allocator** | live bytes, churn, and retention vs the OS (feature `alloc-obs`) |
| **Findings** | delta triggers, threshold verdicts, one `Health` roll-up |

## The two things worth understanding

### Percentiles come from a fixed-size histogram

Keeping every sample to sort later is unbounded memory, which this crate does
not do. Values land in log-linear buckets instead — 16 sub-buckets per power of
two, so resolution is *relative* (≤6.25% error), which is exactly what latency
needs: 6% of 2 ms is 0.12 ms, 6% of 20 s is 1.2 s, and both are right at their
own scale.

Cost is fixed at 4 KiB per operation name, no matter how many calls. `count`,
`min` and `max` stay exact. Quantiles report the **top** of their bucket, so a
percentile is an upper bound — never a number the operation did not reach.

Why it matters, from the `scope` example:

```
op                  count   mean_ms   p50_ms   p95_ms   p99_ms   tail
fast_with_a_tail       40      6.40     1.53     2.30   100.53  65.5x
steady                 40      5.36     5.63     5.63     6.16   1.1x
```

Nearly identical means. One of them stalls for a tenth of a second every
twentieth call. Only the percentiles say which.

### A rising floor is a leak; a high peak is not

A process that spikes to 2 GB and returns to 400 MB is healthy. One whose
*quiet* level creeps from 400 MB to 900 MB is leaking, even if it never spikes.
Neither a single reading nor a peak can tell those apart.

So the trend is fitted to the **floor** — the level the process returns to —
and reported in MiB/hour, the unit the question is actually asked in: *will this
survive the night?*

The slope is fitted to per-segment minima, not raw samples. Fitting raw samples
makes the answer depend on where spikes happen to land: a pure sawtooth that
always returns to the same level reads as tens of MiB/hour of "growth" purely
because each high sample sits later in time than the low one before it.

And when the window is too short or too sparse to support a rate, the slope is
`None`. It never extrapolates five minutes of data into an hourly number.

## Measuring operations

```rust
// Timing only: no syscalls, no allocation.
{
    let _s = Scope::new("catalog_reconcile");
    reconcile()?;                      // records on every exit path, including `?`
}

// Fallible work records its own outcome.
let rows = measure("load_games", || db.load_all())?;

// Opt in to memory attribution when the operation is big enough to move it.
let snapshot = {
    let _s = Scope::with_memory("snapshot_reload");
    load_everything()                  // returned, so it outlives the scope
};
```

Memory attribution is opt-in because it costs a syscall on each side —
affordable for a reload that runs every few minutes, not for a hot loop.

Two things to know before trusting `d_private`:

- **Check `is_isolated()` first.** The counters are process-wide, so
  overlapping scopes attribute each other's work. Only `concurrent == 1` is a
  clean measurement.
- **Drop order is part of the measurement.** A scope records when *it* drops,
  and Rust drops locals in reverse declaration order — so a buffer declared
  after the scope is freed first. The example above returns the data out of the
  block for exactly this reason.

## Cost

Enumerating processes is the expensive part, so it only happens when something
you configured actually needs it: a `watch` pattern, a `ProcessAlive` check, or
`top_n`. With none of those, only your own process is refreshed. A monitor that
is itself a load is a monitor people turn off.

## Reading the first report

Every rate is `None` in it. There is no interval to divide by yet, and CPU needs
two samples as well. Take a second snapshot a few seconds later before believing
any rate.

## Features

- `alloc-obs` — a counting global allocator. `live_bytes` compared against
  private bytes separates live data from allocator retention; the two have
  opposite fixes, and no process-level metric can tell them apart. Costs two
  relaxed atomic adds per allocation.

```rust
#[global_allocator]
static A: clove4apm::alloc::CountingAlloc = clove4apm::alloc::CountingAlloc;
```

## Platforms

Windows and Linux get the full set of native counters. Everywhere else the
platform-specific probes return `None` rather than zero — the crate still builds
and runs, and tells you what it could not measure. `memory_probe_available()`
answers that at startup.

## Examples

```bash
cargo run --example health          # summary() and problems(), the usual entry point
cargo run --example scope           # latency distributions and memory attribution
cargo run --example checks          # dependency probes and how they roll up
cargo run --example watch_process   # notice a restart or a death
cargo run --example full_report     # the whole struct
```

## Status and contributing

The crate is pre-1.0 and under active development. The measurements and the
rules described above are implemented and tested; what is still moving is the
shape of the API around them, so expect breaking changes between releases until
1.0 and pin an exact version if that matters to you.

Contributions and support are welcome. The most useful thing you can report is
**a metric that told you the wrong thing** — a leak it missed, a spike it called
a leak, a percentile that did not match what you observed. Those are the reports
that improve a monitoring tool, because the failure mode that matters here is
not a crash, it is a number that gets believed.

Two rules for anything added:

- A probe that cannot measure returns `None`. Never a zero, never a guess.
- No storage, no files, no background threads. Values in, one struct out.
