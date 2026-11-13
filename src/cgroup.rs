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
}
