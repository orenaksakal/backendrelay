//! Linux process and container memory facts.
//!
//! The result-cache budget takes a fixed share of the container limit, and the
//! private metrics endpoint exports the limit and resident set so an operator
//! alert can fire before the kernel OOM killer does.

use std::path::Path;

const CGROUP_MEMORY_MAX: &str = "/sys/fs/cgroup/memory.max";
const PROC_MEMINFO: &str = "/proc/meminfo";
#[cfg(target_os = "linux")]
const PROC_STATUS: &str = "/proc/self/status";
const RESULT_CACHE_MAX_BYTES_ENV: &str = "INSTANT_RESULT_CACHE_MAX_BYTES";
/// Result caches may use this share of the container memory limit.
const RESULT_CACHE_SHARE_DIVISOR: u64 = 4;
/// Even a large host limit must not turn the result caches into a multi-gigabyte
/// process-wide allocation.
const RESULT_CACHE_MAX_BYTES_CAP: u64 = 4 * 1024 * 1024 * 1024;
/// Used when neither the cgroup limit nor host memory is readable.
const RESULT_CACHE_FALLBACK_BYTES: u64 = 1024 * 1024 * 1024;

/// Process-wide byte cap for the per-app result caches.
///
/// `INSTANT_RESULT_CACHE_MAX_BYTES` wins when it is a positive integer. The
/// default is a quarter of the cgroup memory limit, a quarter of host memory
/// when the cgroup is unlimited or unreadable, and never more than 4 GiB.
pub(crate) fn result_cache_max_bytes() -> u64 {
    configured_result_cache_max_bytes(std::env::var(RESULT_CACHE_MAX_BYTES_ENV).ok().as_deref())
        .unwrap_or_else(|| {
            default_result_cache_max_bytes(Path::new(CGROUP_MEMORY_MAX), Path::new(PROC_MEMINFO))
        })
}

fn configured_result_cache_max_bytes(value: Option<&str>) -> Option<u64> {
    value?.trim().parse::<u64>().ok().filter(|bytes| *bytes > 0)
}

fn default_result_cache_max_bytes(cgroup_path: &Path, meminfo_path: &Path) -> u64 {
    let limit = std::fs::read_to_string(cgroup_path)
        .ok()
        .as_deref()
        .and_then(cgroup_memory_max)
        .or_else(|| {
            std::fs::read_to_string(meminfo_path)
                .ok()
                .as_deref()
                .and_then(meminfo_total)
        });
    limit.map_or(RESULT_CACHE_FALLBACK_BYTES, |bytes| {
        (bytes / RESULT_CACHE_SHARE_DIVISOR).min(RESULT_CACHE_MAX_BYTES_CAP)
    })
}

/// A cgroup v2 `memory.max` value. `max` and zero mean unlimited.
fn cgroup_memory_max(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok().filter(|bytes| *bytes > 0)
}

fn meminfo_total(raw: &str) -> Option<u64> {
    let kib = meminfo_kib(raw, "MemTotal")?;
    kib.checked_mul(1024)
}

/// Resident set size from `/proc/self/status`.
#[cfg(any(target_os = "linux", test))]
fn status_resident(raw: &str) -> Option<u64> {
    let kib = meminfo_kib(raw, "VmRSS")?;
    kib.checked_mul(1024)
}

fn meminfo_kib(raw: &str, field: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != field {
            return None;
        }
        value.split_ascii_whitespace().next()?.parse::<u64>().ok()
    })
}

/// Container memory limit, when the cgroup reports a finite one.
#[cfg(target_os = "linux")]
pub(crate) fn memory_limit_bytes() -> Option<u64> {
    std::fs::read_to_string(CGROUP_MEMORY_MAX)
        .ok()
        .as_deref()
        .and_then(cgroup_memory_max)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn memory_limit_bytes() -> Option<u64> {
    None
}

/// Resident bytes of this process.
#[cfg(target_os = "linux")]
pub(crate) fn resident_bytes() -> Option<u64> {
    std::fs::read_to_string(PROC_STATUS)
        .ok()
        .as_deref()
        .and_then(status_resident)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn resident_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/memory")
            .join(name)
    }

    #[test]
    fn result_cache_default_is_a_quarter_of_the_container_limit() {
        assert_eq!(
            default_result_cache_max_bytes(
                &memory_fixture("cgroup-memory.max"),
                &memory_fixture("proc-meminfo"),
            ),
            512 * 1024 * 1024
        );
    }

    #[test]
    fn unlimited_or_missing_cgroup_falls_back_to_host_memory_with_a_four_gib_cap() {
        for cgroup in ["cgroup-memory.max-unlimited", "cgroup-memory.max-missing"] {
            assert_eq!(
                default_result_cache_max_bytes(
                    &memory_fixture(cgroup),
                    &memory_fixture("proc-meminfo"),
                ),
                4 * 1024 * 1024 * 1024
            );
        }
        assert_eq!(
            default_result_cache_max_bytes(
                &memory_fixture("cgroup-memory.max-unlimited"),
                &memory_fixture("proc-meminfo-small"),
            ),
            2 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn an_unreadable_memory_limit_uses_the_conservative_fallback() {
        assert_eq!(
            default_result_cache_max_bytes(
                &memory_fixture("cgroup-memory.max-missing"),
                &memory_fixture("proc-meminfo-missing"),
            ),
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn explicit_and_malformed_overrides_are_parsed_before_any_probe() {
        assert_eq!(configured_result_cache_max_bytes(Some("4096")), Some(4096));
        assert_eq!(
            configured_result_cache_max_bytes(Some(" 1048576 \n")),
            Some(1_048_576)
        );
        for value in [None, Some(""), Some("0"), Some("-1"), Some("four")] {
            assert_eq!(configured_result_cache_max_bytes(value), None);
        }
    }

    #[test]
    fn memory_sources_parse_the_kernel_fixture_formats() {
        let cgroup = std::fs::read_to_string(memory_fixture("cgroup-memory.max")).unwrap();
        assert_eq!(cgroup_memory_max(&cgroup), Some(2 * 1024 * 1024 * 1024));
        let unlimited =
            std::fs::read_to_string(memory_fixture("cgroup-memory.max-unlimited")).unwrap();
        assert_eq!(cgroup_memory_max(&unlimited), None);

        let meminfo = std::fs::read_to_string(memory_fixture("proc-meminfo")).unwrap();
        assert_eq!(meminfo_total(&meminfo), Some(32 * 1024 * 1024 * 1024));
        let status = std::fs::read_to_string(memory_fixture("proc-status")).unwrap();
        assert_eq!(status_resident(&status), Some(64 * 1024 * 1024));
    }
}
