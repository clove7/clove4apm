//! Watch processes by name pattern (the watchdog's `server-*.exe` pattern).
//! `Gone` and `Switched` are events in their own right — a restart or a death
//! is exactly what a watcher exists to notice.

use std::collections::HashMap;

use crate::process;
use crate::report::{WatchEvent, WatchedProcess};

/// `prev` maps pattern -> last seen pid, kept between snapshots by `Apm`.
pub fn scan(
    patterns: &[String],
    sys: &sysinfo::System,
    prev: &mut HashMap<String, Option<u32>>,
) -> Vec<WatchedProcess> {
    patterns
        .iter()
        .map(|pat| {
            let metrics = process::find(sys, pat);
            let new_pid = metrics.as_ref().map(|m| m.pid);
            let event = match (prev.get(pat).copied().flatten(), new_pid) {
                (None, Some(_)) => Some(WatchEvent::Found),
                (Some(old), Some(new)) if old != new => Some(WatchEvent::Switched { old_pid: old }),
                (Some(_), None) => Some(WatchEvent::Gone),
                _ => None,
            };
            prev.insert(pat.clone(), new_pid);
            WatchedProcess {
                pattern: pat.clone(),
                metrics,
                event,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::matches;

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
    fn events_found_steady_gone() {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let exe = std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let mut prev = HashMap::new();
        let pats = vec![exe.clone()];

        let r = scan(&pats, &sys, &mut prev);
        assert_eq!(r[0].event, Some(WatchEvent::Found));

        let r = scan(&pats, &sys, &mut prev);
        assert_eq!(r[0].event, None, "steady state has no event");

        // A fresh pattern that matches nothing is not an event...
        let pats = vec!["definitely-not-running-xyz.exe".to_string()];
        let r = scan(&pats, &sys, &mut prev);
        assert_eq!(r[0].event, None);
        // ...but a pattern that was seen and now matches nothing is `Gone`.
        let mut prev2 = HashMap::from([(pats[0].clone(), Some(123u32))]);
        let r = scan(&pats, &sys, &mut prev2);
        assert_eq!(r[0].event, Some(WatchEvent::Gone));
    }
}
