//! Dependency probes. Every check runs with a timeout and reports its own
//! latency — a check that hangs the app is worse than no check.

use std::time::{Duration, Instant};

use crate::report::{Check, CheckResult, Health};

impl Check {
    pub fn name(&self) -> &str {
        match self {
            Check::Tcp { name, .. } => name,
            Check::DiskFree { name, .. } => name,
            Check::ProcessAlive { name, .. } => name,
        }
    }
}

pub fn run(check: &Check, sys: &sysinfo::System, disks: &sysinfo::Disks) -> CheckResult {
    let t = Instant::now();
    let (status, message) = match check {
        Check::Tcp {
            addr, timeout_ms, ..
        } => tcp(addr, *timeout_ms),
        Check::DiskFree {
            mount, min_free, ..
        } => disk_free(disks, mount, *min_free),
        Check::ProcessAlive { pattern, .. } => process_alive(sys, pattern),
    };
    CheckResult {
        name: check.name().to_string(),
        status,
        latency_ms: t.elapsed().as_millis() as u64,
        message,
    }
}

fn tcp(addr: &str, timeout_ms: u64) -> (Health, Option<String>) {
    use std::net::ToSocketAddrs;

    let dur = Duration::from_millis(timeout_ms);
    // ponytail: DNS resolution itself is not bounded by the timeout, only the
    // connect is. Pass IPs if that matters.
    match addr.to_socket_addrs() {
        Ok(addrs) => {
            let mut last_err = None;
            for a in addrs {
                match std::net::TcpStream::connect_timeout(&a, dur) {
                    Ok(_) => return (Health::Healthy, None),
                    Err(e) => last_err = Some(e.to_string()),
                }
            }
            (Health::Unhealthy, last_err)
        }
        Err(e) => (Health::Unhealthy, Some(format!("resolve: {e}"))),
    }
}

fn disk_free(disks: &sysinfo::Disks, mount: &str, min_free: u64) -> (Health, Option<String>) {
    // Longest mount point that prefixes the path ("C:\" for "C:\data").
    let disk = disks
        .iter()
        .filter(|d| mount.starts_with(&d.mount_point().to_string_lossy().as_ref().to_string()))
        .max_by_key(|d| d.mount_point().as_os_str().len());
    match disk {
        Some(d) => {
            let free = d.available_space();
            if free >= min_free {
                (Health::Healthy, None)
            } else {
                (
                    Health::Unhealthy,
                    Some(format!(
                        "{} free on {}, need {}",
                        free,
                        d.mount_point().display(),
                        min_free
                    )),
                )
            }
        }
        None => (Health::Degraded, Some(format!("no mount matches {mount}"))),
    }
}

fn process_alive(sys: &sysinfo::System, pattern: &str) -> (Health, Option<String>) {
    match crate::process::find(sys, pattern) {
        Some(m) => (Health::Healthy, Some(format!("pid {}", m.pid))),
        None => (
            Health::Unhealthy,
            Some(format!("no process matches {pattern}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_free_zero_requirement_is_healthy() {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let mount = disks
            .iter()
            .map(|d| d.mount_point().to_string_lossy().into_owned())
            .next()
            .expect("at least one disk");
        let c = Check::DiskFree {
            name: "t".into(),
            mount,
            min_free: 0,
        };
        let sys = sysinfo::System::new();
        let r = run(&c, &sys, &disks);
        assert_eq!(r.status, Health::Healthy);
    }

    #[test]
    fn current_exe_is_alive() {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let exe = std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let c = Check::ProcessAlive {
            name: "t".into(),
            pattern: exe,
        };
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let r = run(&c, &sys, &disks);
        assert_eq!(r.status, Health::Healthy);
    }

    #[test]
    fn closed_port_is_unhealthy_not_a_hang() {
        let c = Check::Tcp {
            name: "t".into(),
            addr: "127.0.0.1:1".into(),
            timeout_ms: 200,
        };
        let sys = sysinfo::System::new();
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let r = run(&c, &sys, &disks);
        assert_eq!(r.status, Health::Unhealthy);
        assert!(r.latency_ms < 5_000, "took {}ms", r.latency_ms);
    }
}
