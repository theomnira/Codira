//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The heap limit a host gives a program when nobody chose one: half of
//!   the memory available to the process, which is the smaller of physical
//!   memory and the cgroup's memory limit (containers).
//! - Reading a heap limit as a user writes it (`512MiB`, `2GiB`, `25%`,
//!   `unlimited`) and a collector growth percentage (`100`, `off`), for the
//!   `--heap-limit`/`--gc-growth` flags, `CODIRA_HEAP_LIMIT`/
//!   `CODIRA_GC_GROWTH` and the `[runtime]` table of `codira.toml`.
//!
//! # Why half of the available memory
//!
//! The limit caps the garbage-collected heap only. The process also needs
//! memory the limit does not count: the compiler (`codira run` builds in
//! the same process first), loaded code, thread stacks, region arenas and
//! pages, file and database buffers. Pacing keeps the heap near twice the
//! live data, so the cap only matters for a program whose live data keeps
//! growing; at half the memory such a program stops with a message that
//! says what was live and how to raise the limit, instead of being killed by
//! the kernel's out-of-memory killer (or swapping) with nothing said. The
//! JVM's default is a quarter of physical memory, .NET's three quarters of a
//! container's limit; a half leaves room for the rest of the process and
//! still lets a program use most of a machine without configuration.
//!
//! In a container the cgroup's limit is what the process can actually get,
//! so it counts when it is lower than physical memory (cgroup v1 and v2 on
//! Linux, every level of the hierarchy). When the size of memory cannot be
//! found out, the default is [`FALLBACK_HEAP_LIMIT`].

use std::path::{Path, PathBuf};

use codira_memory::gc::format_bytes;

/// The default heap limit, as a percentage of the memory available to the
/// process.
pub const DEFAULT_HEAP_PERCENT: u32 = 50;

/// The default heap limit when the size of memory is unknown.
pub const FALLBACK_HEAP_LIMIT: usize = 4 << 30;

/// The default heap limit is never below this (a machine or container
/// with less than 128 MiB).
pub const MIN_DEFAULT_HEAP_LIMIT: usize = 64 << 20;

/// The smallest heap limit accepted from a user: one block of small
/// objects. Anything smaller fails on the first allocation.
pub const MIN_HEAP_LIMIT: usize = 64 * 1024;

/// A heap limit as a user writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapLimitSetting {
    /// A number of bytes.
    Bytes(usize),
    /// A percentage (1 to 100) of the memory available to the process.
    PercentOfMemory(u32),
    /// No limit.
    Unlimited,
}

/// Reads a heap limit: a number of bytes with an optional unit (`B`;
/// binary `K`/`KiB`, `M`/`MiB`, `G`/`GiB`, `T`/`TiB`; decimal `KB`, `MB`,
/// `GB`, `TB`; case-insensitive, fractions allowed: `1.5GiB`), a percentage
/// of the available memory (`25%`), or `unlimited`.
pub fn parse_heap_limit(text: &str) -> Result<HeapLimitSetting, String> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower == "unlimited" || lower == "none" {
        return Ok(HeapLimitSetting::Unlimited);
    }
    if let Some(percent) = lower.strip_suffix('%') {
        return match percent.trim().parse::<u32>() {
            Ok(p @ 1..=100) => Ok(HeapLimitSetting::PercentOfMemory(p)),
            _ => Err(format!(
                "`{trimmed}` is not a heap limit: a percentage of the available memory is a whole \
                 number from 1% to 100%"
            )),
        };
    }
    let split = lower
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(split);
    let unit = unit.trim();
    let multiplier: u64 = match unit {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        _ => {
            return Err(format!(
                "`{trimmed}` is not a heap limit: write a size such as `512MiB`, `2GiB` or \
                 `1500MB`, a percentage of the available memory such as `25%`, or `unlimited`"
            ))
        }
    };
    let number = number.replace('_', "");
    let value: f64 = number.parse().map_err(|_| {
        format!(
            "`{trimmed}` is not a heap limit: write a size such as `512MiB`, `2GiB` or \
             `1500MB`, a percentage of the available memory such as `25%`, or `unlimited`"
        )
    })?;
    let bytes = value * multiplier as f64;
    if !bytes.is_finite() || bytes >= usize::MAX as f64 {
        return Err(format!("`{trimmed}` is too large for a heap limit"));
    }
    let bytes = bytes as usize;
    if bytes < MIN_HEAP_LIMIT {
        return Err(format!(
            "`{trimmed}` is too small for a heap limit: the heap takes memory in blocks of \
             {}, so the least is `64KiB` (use `unlimited` for no limit)",
            format_bytes(MIN_HEAP_LIMIT as u64)
        ));
    }
    Ok(HeapLimitSetting::Bytes(bytes))
}

/// Reads a collector growth percentage: a whole number of percent (the heap
/// may grow by that much over what the previous collection left live; `0`
/// collects whenever the heap takes more memory), or `off` (collect only at
/// the heap limit and on request).
pub fn parse_gc_growth(text: &str) -> Result<Option<u32>, String> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let digits = trimmed.strip_suffix('%').unwrap_or(trimmed).trim();
    match digits.parse::<u32>() {
        Ok(percent) if percent <= 100_000 => Ok(Some(percent)),
        _ => Err(format!(
            "`{trimmed}` is not a collector growth: write a percentage such as `100` (collect \
             once the heap has doubled since the last collection's live data) or `off`"
        )),
    }
}

/// Where a limit on the process's memory came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemorySource {
    /// The machine's physical memory.
    Physical,
    /// A cgroup's memory limit (`memory.max` or `memory.limit_in_bytes`) in
    /// this directory.
    Cgroup(PathBuf),
}

/// How much memory the process can have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableMemory {
    /// Bytes.
    pub bytes: u64,
    /// Where the number came from.
    pub source: MemorySource,
}

impl AvailableMemory {
    /// Finds out how much memory the process can have: the smaller of
    /// physical memory and the memory limit of the cgroup it runs in. `None`
    /// when neither is known.
    pub fn detect() -> Option<Self> {
        let physical = physical_memory().map(|bytes| AvailableMemory {
            bytes,
            source: MemorySource::Physical,
        });
        let cgroup = cgroup_memory_limit().map(|(bytes, dir)| AvailableMemory {
            bytes,
            source: MemorySource::Cgroup(dir),
        });
        match (physical, cgroup) {
            (Some(p), Some(c)) => Some(if c.bytes < p.bytes { c } else { p }),
            (p, c) => p.or(c),
        }
    }

    fn describe(&self) -> String {
        match &self.source {
            MemorySource::Physical => {
                format!("{} of physical memory", format_bytes(self.bytes))
            }
            MemorySource::Cgroup(dir) => format!(
                "the {} the process's cgroup allows ({})",
                format_bytes(self.bytes),
                dir.display()
            ),
        }
    }
}

/// A heap limit and, in words, where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHeapLimit {
    /// The limit; `None` for no limit.
    pub bytes: Option<usize>,
    /// How it was computed, e.g. `50% of 15.6 GiB of physical memory`.
    pub explanation: String,
}

/// Turns a setting into a number of bytes; a percentage is of `memory`.
pub fn resolve_heap_limit(
    setting: HeapLimitSetting,
    memory: Option<&AvailableMemory>,
) -> ResolvedHeapLimit {
    match setting {
        HeapLimitSetting::Bytes(bytes) => ResolvedHeapLimit {
            bytes: Some(bytes),
            explanation: format_bytes(bytes as u64),
        },
        HeapLimitSetting::Unlimited => ResolvedHeapLimit {
            bytes: None,
            explanation: "unlimited".to_string(),
        },
        HeapLimitSetting::PercentOfMemory(percent) => match memory {
            Some(memory) => {
                let bytes = (u128::from(memory.bytes) * u128::from(percent) / 100) as u64;
                let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
                ResolvedHeapLimit {
                    bytes: Some(bytes),
                    explanation: format!(
                        "{} ({percent}% of {})",
                        format_bytes(bytes as u64),
                        memory.describe()
                    ),
                }
            }
            None => ResolvedHeapLimit {
                bytes: Some(FALLBACK_HEAP_LIMIT),
                explanation: format!(
                    "{} (the size of memory is unknown, so {percent}% of it could not be \
                     computed)",
                    format_bytes(FALLBACK_HEAP_LIMIT as u64)
                ),
            },
        },
    }
}

/// The heap limit a host uses when nobody chose one:
/// [`DEFAULT_HEAP_PERCENT`] of `memory` (at least
/// [`MIN_DEFAULT_HEAP_LIMIT`]), or [`FALLBACK_HEAP_LIMIT`] when the size of
/// memory is unknown. See the module documentation for why.
pub fn default_heap_limit(memory: Option<&AvailableMemory>) -> ResolvedHeapLimit {
    let mut resolved = resolve_heap_limit(
        HeapLimitSetting::PercentOfMemory(DEFAULT_HEAP_PERCENT),
        memory,
    );
    if let Some(bytes) = resolved.bytes {
        if bytes < MIN_DEFAULT_HEAP_LIMIT {
            resolved = ResolvedHeapLimit {
                bytes: Some(MIN_DEFAULT_HEAP_LIMIT),
                explanation: format!(
                    "{} (the least default; {DEFAULT_HEAP_PERCENT}% of the available memory \
                     would be {})",
                    format_bytes(MIN_DEFAULT_HEAP_LIMIT as u64),
                    format_bytes(bytes as u64)
                ),
            };
        }
    }
    resolved
}

/// The machine's physical memory in bytes.
#[cfg(unix)]
fn physical_memory() -> Option<u64> {
    // Safety: `sysconf` only reads system configuration.
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    if pages <= 0 || page_size <= 0 {
        return None;
    }
    (pages as u64).checked_mul(page_size as u64)
}

/// The machine's physical memory in bytes.
#[cfg(windows)]
fn physical_memory() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    // Safety: `status` is a correctly sized and initialised MEMORYSTATUSEX.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0 && status.total_phys > 0).then_some(status.total_phys)
}

#[cfg(not(any(unix, windows)))]
fn physical_memory() -> Option<u64> {
    None
}

/// The memory limit of the cgroup this process runs in (the smallest on
/// the path from its own cgroup to the root of the hierarchy), and the
/// directory that sets it. `None` when there is no limit or no cgroup.
#[cfg(target_os = "linux")]
fn cgroup_memory_limit() -> Option<(u64, PathBuf)> {
    let cgroups = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    cgroup_limit_from(&cgroups, &mountinfo, &|path| {
        std::fs::read_to_string(path).ok()
    })
}

#[cfg(not(target_os = "linux"))]
fn cgroup_memory_limit() -> Option<(u64, PathBuf)> {
    None
}

/// The work of `cgroup_memory_limit`, over the contents of
/// `/proc/self/cgroup` and `/proc/self/mountinfo` and a way to read the
/// cgroup files, so it can be tested without a cgroup.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_limit_from(
    cgroups: &str,
    mountinfo: &str,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Option<(u64, PathBuf)> {
    // `hierarchy-id:controllers:path`; v2 is `0::path`.
    let mut v1 = None;
    let mut v2 = None;
    for line in cgroups.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(id), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if controllers.split(',').any(|c| c == "memory") {
            v1 = Some(path);
        } else if id == "0" && controllers.is_empty() {
            v2 = Some(path);
        }
    }
    // `id parent major:minor root mount-point options [optional...] - fstype
    // source super-options`
    let mut v1_mount = None;
    let mut v2_mount = None;
    for line in mountinfo.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<&str> = before.split(' ').collect();
        let tail: Vec<&str> = after.split(' ').collect();
        if fields.len() < 5 || tail.is_empty() {
            continue;
        }
        let (root, mount_point) = (fields[3], fields[4]);
        match tail[0] {
            "cgroup2" => v2_mount = Some((root, mount_point)),
            "cgroup"
                if tail
                    .get(2)
                    .is_some_and(|o| o.split(',').any(|o| o == "memory")) =>
            {
                v1_mount = Some((root, mount_point));
            }
            _ => {}
        }
    }
    let (path, (root, mount_point), file) = match (v1, v1_mount, v2, v2_mount) {
        (Some(path), Some(mount), _, _) => (path, mount, "memory.limit_in_bytes"),
        (_, _, Some(path), Some(mount)) => (path, mount, "memory.max"),
        _ => return None,
    };
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .trim_start_matches('/');
    let mount_point = Path::new(mount_point);
    let mut dir = mount_point.join(relative);
    let mut best: Option<(u64, PathBuf)> = None;
    loop {
        if let Some(text) = read(&dir.join(file)) {
            let text = text.trim();
            // v1 reports "no limit" as a huge number near `i64::MAX`.
            if let Ok(limit) = text.parse::<u64>() {
                if limit < (1 << 62) && best.as_ref().is_none_or(|(b, _)| limit < *b) {
                    best = Some((limit, dir.clone()));
                }
            }
        }
        if dir == mount_point || !dir.pop() || !dir.starts_with(mount_point) {
            break;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn sizes() {
        use HeapLimitSetting::*;
        assert_eq!(parse_heap_limit("512MiB"), Ok(Bytes(512 << 20)));
        assert_eq!(parse_heap_limit("512 MiB"), Ok(Bytes(512 << 20)));
        assert_eq!(parse_heap_limit("2g"), Ok(Bytes(2 << 30)));
        assert_eq!(parse_heap_limit("1.5GiB"), Ok(Bytes(3 << 29)));
        assert_eq!(parse_heap_limit("1500MB"), Ok(Bytes(1_500_000_000)));
        assert_eq!(parse_heap_limit("1_048_576"), Ok(Bytes(1 << 20)));
        assert_eq!(parse_heap_limit("64KiB"), Ok(Bytes(64 << 10)));
        assert_eq!(parse_heap_limit("25%"), Ok(PercentOfMemory(25)));
        assert_eq!(parse_heap_limit("Unlimited"), Ok(Unlimited));
        for bad in [
            "",
            "0",
            "1KiB",
            "lots",
            "12 parsecs",
            "0%",
            "101%",
            "-5MiB",
            "1e999GiB",
        ] {
            assert!(parse_heap_limit(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(parse_heap_limit("1KiB").unwrap_err().contains("64KiB"));
    }

    #[test]
    fn growth() {
        assert_eq!(parse_gc_growth("100"), Ok(Some(100)));
        assert_eq!(parse_gc_growth("50%"), Ok(Some(50)));
        assert_eq!(parse_gc_growth("OFF"), Ok(None));
        assert!(parse_gc_growth("fast").is_err());
        assert!(parse_gc_growth("-1").is_err());
    }

    #[test]
    fn the_default_is_half_of_the_available_memory() {
        let memory = AvailableMemory {
            bytes: 16 << 30,
            source: MemorySource::Physical,
        };
        let limit = default_heap_limit(Some(&memory));
        assert_eq!(limit.bytes, Some(8 << 30));
        assert_eq!(
            limit.explanation,
            "8.00 GiB (50% of 16.0 GiB of physical memory)"
        );

        let small = AvailableMemory {
            bytes: 100 << 20,
            source: MemorySource::Physical,
        };
        assert_eq!(
            default_heap_limit(Some(&small)).bytes,
            Some(MIN_DEFAULT_HEAP_LIMIT)
        );
        assert_eq!(default_heap_limit(None).bytes, Some(FALLBACK_HEAP_LIMIT));
    }

    #[test]
    fn detection_finds_some_memory_here() {
        let memory = AvailableMemory::detect().expect("the test machine's memory");
        assert!(memory.bytes >= 64 << 20, "{memory:?}");
    }

    fn files(entries: &[(&str, &str)]) -> impl Fn(&Path) -> Option<String> {
        let map: HashMap<PathBuf, String> = entries
            .iter()
            .map(|(path, text)| (PathBuf::from(path), text.to_string()))
            .collect();
        move |path| map.get(path).cloned()
    }

    #[test]
    fn cgroup_v1_takes_the_smallest_limit_on_the_path() {
        let cgroups = "4:memory:/process_api/abc/bash\n1:cpu:/\n";
        let mountinfo = "41 37 0:33 / /sys/fs/cgroup/memory rw,relatime - cgroup cgroup \
                         rw,memory\n";
        let read = files(&[
            (
                "/sys/fs/cgroup/memory/process_api/abc/bash/memory.limit_in_bytes",
                "14337302528\n",
            ),
            (
                "/sys/fs/cgroup/memory/process_api/abc/memory.limit_in_bytes",
                "9223372036854771712\n",
            ),
            (
                "/sys/fs/cgroup/memory/memory.limit_in_bytes",
                "9223372036854771712\n",
            ),
        ]);
        let (limit, dir) = cgroup_limit_from(cgroups, mountinfo, &read).unwrap();
        assert_eq!(limit, 14_337_302_528);
        assert_eq!(dir, Path::new("/sys/fs/cgroup/memory/process_api/abc/bash"));
    }

    #[test]
    fn cgroup_v2_reads_memory_max_up_the_tree() {
        let cgroups = "0::/user.slice/app.scope\n";
        let mountinfo = "30 24 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n";
        let read = files(&[
            ("/sys/fs/cgroup/user.slice/app.scope/memory.max", "max\n"),
            ("/sys/fs/cgroup/user.slice/memory.max", "2147483648\n"),
        ]);
        let (limit, dir) = cgroup_limit_from(cgroups, mountinfo, &read).unwrap();
        assert_eq!(limit, 2 << 30);
        assert_eq!(dir, Path::new("/sys/fs/cgroup/user.slice"));

        // In a container the cgroup is often mounted at its own root.
        let in_container = "0::/\n";
        let read = files(&[("/sys/fs/cgroup/memory.max", "536870912\n")]);
        assert_eq!(
            cgroup_limit_from(in_container, mountinfo, &read).map(|(l, _)| l),
            Some(512 << 20)
        );
        let unlimited = files(&[("/sys/fs/cgroup/memory.max", "max\n")]);
        assert_eq!(cgroup_limit_from(in_container, mountinfo, &unlimited), None);
    }
}
