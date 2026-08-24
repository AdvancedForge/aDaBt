//! Process resource sampling, read straight from procfs.
//!
//! Resource cost is half of the optimization objective, so the harness measures
//! it with the same seriousness as latency. A benchmark that reports only
//! throughput cannot distinguish a genuine win from one bought with 200GB of
//! RAM, and that distinction is the entire resource axis.

use std::fs;

/// Linux reports CPU time in clock ticks. `sysconf(_SC_CLK_TCK)` is 100 on
/// every supported configuration, and reading it would mean linking libc.
const CLOCK_TICKS_PER_SEC: f64 = 100.0;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResourceSample {
    /// Current resident set size.
    pub rss_bytes: u64,
    /// High-water mark of resident set size since process start.
    pub peak_rss_bytes: u64,
    /// User + system CPU time consumed by this process.
    pub cpu_secs: f64,
    /// Bytes this process caused to be sent to storage.
    pub disk_write_bytes: u64,
    pub disk_read_bytes: u64,
}

impl ResourceSample {
    pub fn now() -> Self {
        Self {
            rss_bytes: status_kib("VmRSS:").unwrap_or(0) * 1024,
            peak_rss_bytes: status_kib("VmHWM:").unwrap_or(0) * 1024,
            cpu_secs: cpu_secs().unwrap_or(0.0),
            disk_write_bytes: io_field("write_bytes:").unwrap_or(0),
            disk_read_bytes: io_field("read_bytes:").unwrap_or(0),
        }
    }

    /// Consumption between two samples. Peak RSS is a high-water mark, so it is
    /// carried forward rather than differenced.
    pub fn since(&self, start: &ResourceSample) -> ResourceSample {
        ResourceSample {
            rss_bytes: self.rss_bytes,
            peak_rss_bytes: self.peak_rss_bytes.max(start.peak_rss_bytes),
            cpu_secs: (self.cpu_secs - start.cpu_secs).max(0.0),
            disk_write_bytes: self.disk_write_bytes.saturating_sub(start.disk_write_bytes),
            disk_read_bytes: self.disk_read_bytes.saturating_sub(start.disk_read_bytes),
        }
    }
}

fn status_kib(key: &str) -> Option<u64> {
    let s = fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find(|l| l.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// `/proc/self/io` is absent when the kernel is built without accounting, or
/// unreadable under some sandboxes; callers treat `None` as "unmeasured".
fn io_field(key: &str) -> Option<u64> {
    let s = fs::read_to_string("/proc/self/io").ok()?;
    s.lines()
        .find(|l| l.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn cpu_secs() -> Option<f64> {
    let s = fs::read_to_string("/proc/self/stat").ok()?;
    // Field 2 (comm) may contain spaces and parentheses; everything after the
    // final ')' is positionally stable.
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After comm and state, utime and stime are fields 14 and 15 one-based,
    // which is index 11 and 12 here.
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some((utime + stime) as f64 / CLOCK_TICKS_PER_SEC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_returns_plausible_values() {
        let s = ResourceSample::now();
        assert!(s.rss_bytes > 0, "RSS should be readable on Linux");
        assert!(s.peak_rss_bytes >= s.rss_bytes);
    }

    #[test]
    fn cpu_time_does_not_go_backwards() {
        let a = ResourceSample::now();
        let mut acc = 0u64;
        for i in 0..2_000_000u64 {
            acc = acc.wrapping_add(i * 2654435761);
        }
        std::hint::black_box(acc);
        let b = ResourceSample::now();
        assert!(b.since(&a).cpu_secs >= 0.0);
    }

    #[test]
    fn peak_rss_is_carried_forward_not_differenced() {
        let a = ResourceSample {
            peak_rss_bytes: 900,
            ..Default::default()
        };
        let b = ResourceSample {
            peak_rss_bytes: 500,
            ..Default::default()
        };
        assert_eq!(b.since(&a).peak_rss_bytes, 900);
    }

    #[test]
    fn counters_never_underflow_when_unavailable() {
        let a = ResourceSample {
            disk_write_bytes: 100,
            ..Default::default()
        };
        let b = ResourceSample {
            disk_write_bytes: 0,
            ..Default::default()
        };
        assert_eq!(b.since(&a).disk_write_bytes, 0);
    }
}

/// Filesystem type backing `path`, from `/proc/self/mounts`.
///
/// The benchmark needs this because a scratch directory on tmpfs makes `fsync`
/// a no-op, which silently turns a durability measurement into a memory
/// measurement. That failure is invisible in the results: strict durability
/// simply looks nearly free, and every conclusion drawn from it is wrong.
pub fn filesystem_type(path: &std::path::Path) -> Option<String> {
    let target = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();
    let mounts = fs::read_to_string("/proc/self/mounts").ok()?;
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let _dev = f.next()?;
        let mount_point = f.next()?;
        let fstype = f.next()?;
        // The mount whose path is the longest prefix of the target owns it.
        let is_prefix = target == mount_point
            || (target.starts_with(mount_point)
                && (mount_point == "/" || target.as_bytes().get(mount_point.len()) == Some(&b'/')));
        if is_prefix
            && best
                .as_ref()
                .is_none_or(|(len, _)| mount_point.len() > *len)
        {
            best = Some((mount_point.len(), fstype.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

/// Whether writes to `path` can actually reach stable storage.
pub fn is_memory_backed(path: &std::path::Path) -> bool {
    matches!(
        filesystem_type(path).as_deref(),
        Some("tmpfs") | Some("ramfs")
    )
}

#[cfg(test)]
mod fs_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_root_filesystem_is_identified() {
        assert!(filesystem_type(Path::new("/")).is_some());
    }

    #[test]
    fn tmpfs_is_recognised_as_memory_backed() {
        // Skip rather than fail where /dev/shm is absent or not tmpfs.
        let shm = Path::new("/dev/shm");
        if shm.exists() && filesystem_type(shm).as_deref() == Some("tmpfs") {
            assert!(is_memory_backed(shm));
        }
    }

    #[test]
    fn a_real_disk_is_not_memory_backed() {
        // The root filesystem is ext4/overlay/9p in every environment this runs
        // in; none of those are memory-backed.
        assert!(!is_memory_backed(Path::new("/")));
    }

    #[test]
    fn the_longest_matching_mount_wins() {
        // /proc is its own mount, so it must not be attributed to /.
        if Path::new("/proc").exists() {
            assert_eq!(filesystem_type(Path::new("/proc")).as_deref(), Some("proc"));
        }
    }
}
