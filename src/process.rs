//! Process metrics: this process (enriched with native counters) and processes
//! found by name (sysinfo only — a handle to a foreign process is not opened).
//!
//! Two numbers that disagree on purpose: working set (`rss`) shrinks when the
//! OS trims pages and reads as a fake improvement; `private_bytes` is what the
//! process actually still owns. Both are reported.
//!
//! Every platform-specific probe returns `None`, not zero, where it cannot
//! measure. A silent zero is indistinguishable from a real measurement and is
//! how a monitoring tool ends up lying.

use crate::report::ProcessMetrics;

// ---------------------------------------------------------------------------
// Windows: Win32 counters for THIS process.
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod native {
    /// PROCESS_MEMORY_COUNTERS_EX (x64) field offsets:
    /// `cb` u32 @0, `PageFaultCount` u32 @4, then SIZE_T fields —
    /// PeakWorkingSetSize @8, WorkingSetSize @16, …, PrivateUsage @72.
    const CB: u32 = 80;

    fn memory_info() -> Option<[u8; CB as usize]> {
        use windows_sys::Win32::System::ProcessStatus::GetProcessMemoryInfo;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let mut buf = [0u8; CB as usize];
        buf[0..4].copy_from_slice(&CB.to_le_bytes());
        let ok =
            unsafe { GetProcessMemoryInfo(GetCurrentProcess(), buf.as_mut_ptr() as *mut _, CB) };
        (ok != 0).then_some(buf)
    }

    /// `(working set, private bytes, peak working set)`, or zeros on failure.
    /// Kept as a tuple and zero-on-failure because `Scope` calls it on a hot
    /// path and maps the zeros itself.
    pub fn mem_counters() -> (u64, u64, u64) {
        let Some(buf) = memory_info() else {
            return (0, 0, 0);
        };
        let rd = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap_or([0; 8]));
        // Peak comes from the OS, not from a userspace maximum that only sees
        // the instants it happens to sample.
        (rd(16), rd(72), rd(8))
    }

    /// Page faults since process start.
    pub fn page_faults() -> Option<u64> {
        let buf = memory_info()?;
        Some(u32::from_le_bytes(buf[4..8].try_into().ok()?) as u64)
    }

    /// Open kernel handles. A count that only climbs is a handle leak.
    pub fn handle_count() -> Option<u32> {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

        let mut n: u32 = 0;
        let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) };
        (ok != 0).then_some(n)
    }

    /// `(read bytes, written bytes, read ops, write ops)` since process start.
    pub fn io_counters() -> Option<(u64, u64, u64, u64)> {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetProcessIoCounters, IO_COUNTERS,
        };

        let mut c = IO_COUNTERS::default();
        let ok = unsafe { GetProcessIoCounters(GetCurrentProcess(), &mut c) };
        (ok != 0).then_some((
            c.ReadTransferCount,
            c.WriteTransferCount,
            c.ReadOperationCount,
            c.WriteOperationCount,
        ))
    }

    /// Kernel + user CPU time since process start, in milliseconds.
    pub fn cpu_time_ms() -> Option<u64> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        if ok == 0 {
            return None;
        }
        // FILETIME is 100-nanosecond intervals split across two u32 halves.
        let as_u64 = |f: &FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        Some((as_u64(&kernel) + as_u64(&user)) / 10_000)
    }

    /// Threads are counted by sysinfo on Linux only; Windows would need a
    /// ToolHelp snapshot, which costs more than the signal is worth here.
    pub fn thread_count() -> Option<u32> {
        None
    }
}

// ---------------------------------------------------------------------------
// Linux: the same counters out of /proc, so the framework is not Windows-only.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod native {
    fn status_field(key: &str) -> Option<u64> {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in s.lines() {
            let Some(rest) = line.strip_prefix(key) else {
                continue;
            };
            let rest = rest.trim_start_matches(':').trim();
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let v = num.parse::<u64>().ok()?;
            // Everything in /proc/self/status that we read is either a plain
            // count or a size in kB.
            return Some(if rest.ends_with("kB") { v * 1024 } else { v });
        }
        None
    }

    /// `(working set, private bytes, peak working set)`, zeros on failure.
    ///
    /// `VmRSS` is the resident set. There is no exact Windows-private-bytes
    /// equivalent; `VmRSS - RssFile` (anonymous + shared dirty) is the closest
    /// honest analogue, so that is what is reported.
    pub fn mem_counters() -> (u64, u64, u64) {
        let rss = status_field("VmRSS").unwrap_or(0);
        let peak = status_field("VmHWM").unwrap_or(0);
        let private = match status_field("RssFile") {
            Some(file) => rss.saturating_sub(file),
            None => rss,
        };
        (rss, private, peak)
    }

    pub fn page_faults() -> Option<u64> {
        // /proc/self/stat: field 10 is minflt, field 12 is majflt (1-based,
        // after the comm field which may itself contain spaces).
        let s = std::fs::read_to_string("/proc/self/stat").ok()?;
        let after_comm = s.rsplit_once(") ")?.1;
        let f: Vec<&str> = after_comm.split_whitespace().collect();
        let minflt = f.get(7)?.parse::<u64>().ok()?;
        let majflt = f.get(9)?.parse::<u64>().ok()?;
        Some(minflt + majflt)
    }

    /// File descriptors — the Linux analogue of a Windows handle count.
    pub fn handle_count() -> Option<u32> {
        Some(std::fs::read_dir("/proc/self/fd").ok()?.count() as u32)
    }

    pub fn io_counters() -> Option<(u64, u64, u64, u64)> {
        let s = std::fs::read_to_string("/proc/self/io").ok()?;
        let get = |k: &str| -> Option<u64> {
            s.lines().find_map(|l| {
                l.strip_prefix(k)?
                    .trim_start_matches(':')
                    .trim()
                    .parse()
                    .ok()
            })
        };
        // syscr/syscw are read/write syscall counts — the closest match to the
        // Windows operation counters.
        Some((
            get("read_bytes")?,
            get("write_bytes")?,
            get("syscr").unwrap_or(0),
            get("syscw").unwrap_or(0),
        ))
    }

    pub fn cpu_time_ms() -> Option<u64> {
        let s = std::fs::read_to_string("/proc/self/stat").ok()?;
        let after_comm = s.rsplit_once(") ")?.1;
        let f: Vec<&str> = after_comm.split_whitespace().collect();
        let utime = f.get(11)?.parse::<u64>().ok()?;
        let stime = f.get(12)?.parse::<u64>().ok()?;
        // Clock ticks; 100 Hz is the near-universal USER_HZ and is not worth a
        // libc dependency to confirm.
        Some((utime + stime) * 10)
    }

    pub fn thread_count() -> Option<u32> {
        status_field("Threads").map(|v| v as u32)
    }
}

// ---------------------------------------------------------------------------
// Everything else: measure nothing, claim nothing.
// ---------------------------------------------------------------------------

#[cfg(not(any(windows, target_os = "linux")))]
mod native {
    pub fn mem_counters() -> (u64, u64, u64) {
        (0, 0, 0)
    }
    pub fn page_faults() -> Option<u64> {
        None
    }
    pub fn handle_count() -> Option<u32> {
        None
    }
    pub fn io_counters() -> Option<(u64, u64, u64, u64)> {
        None
    }
    pub fn cpu_time_ms() -> Option<u64> {
        None
    }
    pub fn thread_count() -> Option<u32> {
        None
    }
}

/// `(working set, private bytes, peak working set)` for this process. Zeros
/// where unavailable — [`self_metrics`] is what maps those to `None`.
pub fn mem_counters() -> (u64, u64, u64) {
    native::mem_counters()
}

/// Whether this platform can attribute memory to a [`crate::Scope`] at all.
pub fn memory_probe_available() -> bool {
    native::mem_counters().0 != 0
}

// ---------------------------------------------------------------------------
// sysinfo-backed metrics
// ---------------------------------------------------------------------------

fn uptime_secs_of(p: &sysinfo::Process) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(p.start_time())
}

fn base_metrics(p: &sysinfo::Process) -> ProcessMetrics {
    let du = p.disk_usage();
    ProcessMetrics {
        pid: p.pid().as_u32(),
        name: p.name().to_string_lossy().into_owned(),
        rss: p.memory(),
        private_bytes: None,
        peak_rss: None,
        cpu_pct: p.cpu_usage(),
        cpu_time_ms: None,
        // sysinfo only knows tasks on Linux; elsewhere None, not a guess.
        threads: p.tasks().map(|t| t.len() as u32),
        handles: None,
        page_faults: None,
        uptime_secs: uptime_secs_of(p),
        disk_read: du.total_read_bytes,
        disk_written: du.total_written_bytes,
        disk_read_ops: None,
        disk_write_ops: None,
    }
}

/// This process, enriched with the native counters.
pub fn self_metrics(sys: &sysinfo::System) -> Option<ProcessMetrics> {
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let p = sys.process(pid)?;
    let mut m = base_metrics(p);

    let (rss, private, peak) = native::mem_counters();
    if rss != 0 {
        m.rss = rss;
        m.private_bytes = Some(private);
        m.peak_rss = Some(peak);
    }
    if let Some((rb, wb, rops, wops)) = native::io_counters() {
        m.disk_read = rb;
        m.disk_written = wb;
        m.disk_read_ops = Some(rops);
        m.disk_write_ops = Some(wops);
    }
    m.cpu_time_ms = native::cpu_time_ms();
    m.page_faults = native::page_faults();
    m.handles = native::handle_count();
    m.threads = m.threads.or_else(native::thread_count);
    Some(m)
}

/// Case-insensitive match. `*` matches any span (one wildcard is enough for
/// `server-*.exe`); without it the whole name must equal the pattern.
pub fn matches(name: &str, pattern: &str) -> bool {
    let (n, p) = (name.to_lowercase(), pattern.to_lowercase());
    match p.split_once('*') {
        Some((pre, suf)) => {
            n.starts_with(pre) && n.ends_with(suf) && n.len() >= pre.len() + suf.len()
        }
        None => n == p,
    }
}

/// Biggest-RAM process matching `pattern` — the heaviest instance is the one
/// that matters when several share a name.
pub fn find(sys: &sysinfo::System, pattern: &str) -> Option<ProcessMetrics> {
    sys.processes()
        .values()
        .filter(|p| matches(&p.name().to_string_lossy(), pattern))
        .max_by_key(|p| p.memory())
        .map(base_metrics)
}

/// The `n` heaviest processes on the machine by resident memory.
///
/// For the question a monitor is usually asked at 3am: not "is my process big"
/// but "what on this machine is eating the RAM".
pub fn top_by_rss(sys: &sysinfo::System, n: usize) -> Vec<ProcessMetrics> {
    if n == 0 {
        return Vec::new();
    }
    let mut all: Vec<&sysinfo::Process> = sys.processes().values().collect();
    all.sort_unstable_by_key(|p| std::cmp::Reverse(p.memory()));
    all.into_iter().take(n).map(base_metrics).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_matching() {
        assert!(matches("server-abc.exe", "server-*.exe"));
        assert!(matches("SERVER-ABC.EXE", "server-*.exe"));
        assert!(!matches("myserver-x.exe", "server-*.exe"));
        assert!(matches("explorer.exe", "explorer.exe"));
        assert!(!matches("explorer.exe", "other.exe"));
        assert!(matches("anything", "*"));
        assert!(!matches("server-.exe", "server-*x.exe"));
    }

    #[test]
    fn native_counters_are_self_consistent() {
        let (rss, private, peak) = native::mem_counters();
        if rss == 0 {
            // Unsupported platform: it must claim nothing rather than zero.
            assert_eq!(private, 0);
            assert_eq!(peak, 0);
            return;
        }
        assert!(peak >= rss, "peak {peak} below current {rss}");
        assert!(private > 0);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn leak_signals_are_available_on_supported_platforms() {
        assert!(native::page_faults().is_some(), "page faults");
        assert!(native::handle_count().is_some(), "handles");
        assert!(native::cpu_time_ms().is_some(), "cpu time");
    }

    #[test]
    fn top_by_rss_is_sorted_and_bounded() {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        assert!(top_by_rss(&sys, 0).is_empty(), "zero means off");
        let top = top_by_rss(&sys, 5);
        assert!(top.len() <= 5);
        for w in top.windows(2) {
            assert!(w[0].rss >= w[1].rss, "not sorted by rss");
        }
    }
}
