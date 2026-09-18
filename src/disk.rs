//! Mounted filesystems: total and free space.

use crate::report::DiskMetrics;

pub fn collect(disks: &sysinfo::Disks) -> Vec<DiskMetrics> {
    disks
        .iter()
        .map(|d| DiskMetrics {
            name: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_string_lossy().into_owned(),
            total: d.total_space(),
            free: d.available_space(),
        })
        .collect()
}
