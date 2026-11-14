//! cgroup v2 (unified hierarchy) helpers: one cgroup per service, used for
//! isolation and for reliably killing whole service trees at shutdown.

use anyhow::{Context, Result};
use nix::mount::{mount, MsFlags};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
pub const SUBTREE: &str = "jiminit";

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Mount the unified hierarchy if necessary and create our subtree. Returns
/// whether cgroup isolation is usable; failures are not fatal.
pub fn init(enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    *ENABLED.get_or_init(|| match setup() {
        Ok(()) => true,
        Err(e) => {
            crate::warn!("cgroup: disabled: {e:#}");
            false
        }
    })
}

pub fn is_enabled() -> bool {
    *ENABLED.get().unwrap_or(&false)
}

fn setup() -> Result<()> {
    let root = Path::new(CGROUP_ROOT);
    if !root.join("cgroup.controllers").exists() {
        std::fs::create_dir_all(root)
            .with_context(|| format!("cannot create {}", root.display()))?;
        mount::<str, str, str, str>(
            Some("none"),
            CGROUP_ROOT,
            Some("cgroup2"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            None,
        )
        .with_context(|| format!("cannot mount cgroup2 on {}", root.display()))?;
    }
    let subtree = root.join(SUBTREE);
    std::fs::create_dir_all(&subtree)
        .with_context(|| format!("cannot create {}", subtree.display()))?;
    // Remove per-service directories left behind by an earlier boot (or a
    // soft reboot). They are empty because every process is gone.
    if let Ok(entries) = std::fs::read_dir(&subtree) {
        for entry in entries.flatten() {
            let procs = entry.path().join("cgroup.procs");
            if let Ok(text) = std::fs::read_to_string(&procs) {
                if text.trim().is_empty() {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }
    Ok(())
}

/// Directory for one service, creating it if needed.
pub fn service_dir(name: &str) -> Option<PathBuf> {
    if !is_enabled() {
        return None;
    }
    let dir = Path::new(CGROUP_ROOT).join(SUBTREE).join(sanitize(name));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Move `pid` into the cgroup of `name`. Called by the child before exec, so
/// it happens while the process is still privileged.
pub fn attach(name: &str, pid: u32) {
    let Some(dir) = service_dir(name) else {
        return;
    };
    let path = dir.join("cgroup.procs");
    if let Err(e) = std::fs::write(&path, format!("{pid}\n")) {
        crate::warn!("cgroup: cannot attach pid {pid} to {}: {e}", dir.display());
    }
}

/// Kill every process in a service cgroup (kernel-level, includes
/// grandchildren that escaped the process group).
pub fn kill(name: &str) {
    let Some(dir) = service_dir(name) else {
        return;
    };
    let path = dir.join("cgroup.kill");
    let _ = std::fs::write(&path, "1\n");
}

/// Enable the cgroup v2 controllers we need, both at the root and in our
/// subtree (a controller must be enabled in the parent before children can
/// use it). Only controllers requested by service limits are touched.
pub fn enable_controllers(controllers: &[&str]) -> Result<()> {
    if !is_enabled() || controllers.is_empty() {
        return Ok(());
    }
    let root = Path::new(CGROUP_ROOT);
    let available = std::fs::read_to_string(root.join("cgroup.controllers"))
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    for controller in controllers {
        if !available.iter().any(|c| c == controller) {
            crate::warn!("cgroup: controller `{controller}` is not available");
            continue;
        }
        enable_one(root, controller)?;
        enable_one(&root.join(SUBTREE), controller)?;
        crate::info!("cgroup: enabled controller {controller}");
    }
    Ok(())
}

fn enable_one(dir: &Path, controller: &str) -> Result<()> {
    let path = dir.join("cgroup.subtree_control");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.split_whitespace().any(|c| c == controller) {
        return Ok(());
    }
    std::fs::write(&path, format!("+{controller}\n"))
        .with_context(|| format!("cannot enable {controller} in {}", path.display()))?;
    Ok(())
}

/// Write the configured limits into the service cgroup. Invalid or
/// unsupported values are logged and skipped rather than killing the service.
pub fn configure(
    name: &str,
    memory_max: Option<&str>,
    pids_max: Option<u64>,
    cpu_max: Option<&str>,
) {
    let Some(dir) = service_dir(name) else {
        return;
    };
    if let Some(value) = memory_max {
        write_limit(&dir, "memory.max", value);
    }
    if let Some(value) = pids_max {
        write_limit(&dir, "pids.max", &value.to_string());
    }
    if let Some(value) = cpu_max {
        write_limit(&dir, "cpu.max", value);
    }
}

fn write_limit(dir: &Path, file: &str, value: &str) {
    let path = dir.join(file);
    if let Err(e) = std::fs::write(&path, format!("{value}\n")) {
        crate::warn!("cgroup: cannot write {}: {e}", path.display());
    }
}

/// Accounting values for one service cgroup. `None` means the file (and thus
/// the controller behind it) is not available; statistics never create the
/// cgroup directory, so asking about a stopped service is harmless.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub cpu_usec: Option<u64>,
    pub memory: Option<u64>,
    pub memory_peak: Option<u64>,
    pub pids: Option<u64>,
    pub io_read: Option<u64>,
    pub io_write: Option<u64>,
}

pub fn stats(name: &str) -> Stats {
    let Some(dir) = existing_dir(name) else {
        return Stats::default();
    };
    let io = read_file(&dir, "io.stat").and_then(|text| parse_io_stat(&text));
    Stats {
        cpu_usec: read_file(&dir, "cpu.stat").and_then(|text| parse_cpu_stat(&text)),
        memory: read_file(&dir, "memory.current").and_then(|text| parse_u64(&text)),
        memory_peak: read_file(&dir, "memory.peak").and_then(|text| parse_u64(&text)),
        pids: read_file(&dir, "pids.current").and_then(|text| parse_u64(&text)),
        io_read: io.map(|(read, _)| read),
        io_write: io.map(|(_, write)| write),
    }
}

/// Existing cgroup directory, without creating it.
fn existing_dir(name: &str) -> Option<PathBuf> {
    if !is_enabled() {
        return None;
    }
    let dir = Path::new(CGROUP_ROOT).join(SUBTREE).join(sanitize(name));
    dir.is_dir().then_some(dir)
}

fn read_file(dir: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(file)).ok()
}

fn parse_u64(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

fn parse_cpu_stat(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("usage_usec"), Some(value)) => value.parse().ok(),
            _ => None,
        }
    })
}

/// Sum `rbytes`/`wbytes` over all devices in an `io.stat` file.
fn parse_io_stat(text: &str) -> Option<(u64, u64)> {
    let mut read = 0;
    let mut write = 0;
    let mut any = false;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next().is_none() {
            continue;
        }
        for field in parts {
            if let Some(value) = field.strip_prefix("rbytes=") {
                read += value.parse::<u64>().unwrap_or(0);
                any = true;
            } else if let Some(value) = field.strip_prefix("wbytes=") {
                write += value.parse::<u64>().unwrap_or(0);
                any = true;
            }
        }
    }
    any.then_some((read, write))
}

/// Number of OOM kills recorded for this service cgroup.
pub fn oom_kill_count(name: &str) -> u64 {
    let Some(dir) = service_dir(name) else {
        return 0;
    };
    let text = std::fs::read_to_string(dir.join("memory.events")).unwrap_or_default();
    text.lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some("oom_kill"), Some(value)) => value.parse().ok(),
                _ => None,
            }
        })
        .unwrap_or(0)
}

pub fn remove(name: &str) {
    if !is_enabled() {
        return;
    }
    let dir = Path::new(CGROUP_ROOT).join(SUBTREE).join(sanitize(name));
    let _ = std::fs::remove_dir(dir);
}

/// cgroup directory names may not contain '/', so map everything else to '_'.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_service_names() {
        assert_eq!(sanitize("ticker"), "ticker");
        assert_eq!(sanitize("a/b:c d"), "a_b_c_d");
        assert_eq!(sanitize("db-1.2"), "db-1.2");
    }

    #[test]
    fn parses_cpu_stat() {
        let text = "usage_usec 12345\nuser_usec 10000\nsystem_usec 2345\n";
        assert_eq!(parse_cpu_stat(text), Some(12345));
        assert_eq!(parse_cpu_stat("user_usec 1\n"), None);
    }

    #[test]
    fn parses_io_stat_across_devices() {
        let text = "8:0 rbytes=100 wbytes=200 rios=1 wios=2 dbytes=0 dios=0\n\
                    8:16 rbytes=300 wbytes=400 rios=3 wios=4 dbytes=0 dios=0\n";
        assert_eq!(parse_io_stat(text), Some((400, 600)));
        assert_eq!(parse_io_stat(""), None);
    }
}
