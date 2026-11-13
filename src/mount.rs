//! Mount helpers for pseudo filesystems and switch_root.

use nix::errno::Errno;
use nix::mount::{mount, MsFlags};

/// Mount (`EBUSY` is treated as success: the filesystem is already mounted).
fn mount_or_busy(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<(), Errno> {
    match mount(source, target, fstype, flags, data) {
        Ok(()) => Ok(()),
        Err(Errno::EBUSY) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Best-effort mounting of /dev, /proc and /sys.
///
/// Both stages call this; failures are logged but not fatal, because the
/// kernel may already have mounted them (devtmpfs) or the target may be a
/// restricted environment. The caller reports more precise errors later.
pub fn mount_pseudo_fs() {
    let _ = std::fs::create_dir_all("/dev");
    let _ = std::fs::create_dir_all("/proc");
    let _ = std::fs::create_dir_all("/sys");

    let entries: [(&str, &str, &str, MsFlags, Option<&str>); 3] = [
        ("devtmpfs", "/dev", "devtmpfs", MsFlags::MS_NOSUID, Some("mode=0755")),
        ("proc", "/proc", "proc", MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV, None),
        ("sysfs", "/sys", "sysfs", MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV, None),
    ];

    for (source, target, fstype, flags, data) in entries {
        if let Err(e) = mount_or_busy(Some(source), target, Some(fstype), flags, data) {
            crate::warn!("mount {fstype} on {target} failed: {e}");
        }
    }
}

/// Move an existing mount point to a new location (used before switch_root).
pub fn mount_move(source: &str, target: &str) -> Result<(), Errno> {
    mount(Some(source), target, None::<&str>, MsFlags::MS_MOVE, None::<&str>)
}

/// Atomically move the new root onto `/` (the classic switch_root trick).
///
/// Must be called after `chdir` into the new root.
pub fn switch_root_move() -> Result<(), Errno> {
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
}
