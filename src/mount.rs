//! Mount helpers for pseudo filesystems and switch_root.

use nix::errno::Errno;
use nix::mount::{mount, MsFlags};

/// Is `target` currently a mount point? This tells "already mounted" apart
/// from "something else is in the way" when mount(2) returns `EBUSY`.
pub fn is_mountpoint(target: &str) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|info| {
            info.lines()
                .any(|line| line.split_whitespace().nth(4) == Some(target))
        })
        .unwrap_or(false)
}

/// Best-effort mounting of /dev, /proc and /sys.
///
/// Both stages call this. A real failure is logged but not fatal here, because
/// callers report the consequences with more context. `EBUSY` is only ignored
/// when the target really is a mount point, so a foreign filesystem sitting on
/// `/dev` is reported instead of being mistaken for devtmpfs.
pub fn mount_pseudo_fs() {
    let _ = std::fs::create_dir_all("/dev");
    let _ = std::fs::create_dir_all("/proc");
    let _ = std::fs::create_dir_all("/sys");

    let entries: [(&str, &str, &str, MsFlags, Option<&str>); 3] = [
        (
            "devtmpfs",
            "/dev",
            "devtmpfs",
            MsFlags::MS_NOSUID,
            Some("mode=0755"),
        ),
        (
            "proc",
            "/proc",
            "proc",
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
            None,
        ),
        (
            "sysfs",
            "/sys",
            "sysfs",
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
            None,
        ),
    ];

    for (source, target, fstype, flags, data) in entries {
        mount_or_report(source, target, fstype, flags, data);
    }

    // Userspace needs these to exist as real filesystems: PTYs and shared
    // memory for desktop stacks, /run for sockets and state, /tmp writable
    // even when the root filesystem is read-only.
    let _ = std::fs::create_dir_all("/dev/pts");
    let _ = std::fs::create_dir_all("/dev/shm");
    let _ = std::fs::create_dir_all("/run");
    let _ = std::fs::create_dir_all("/tmp");

    mount_or_report(
        "devpts",
        "/dev/pts",
        "devpts",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("gid=5,mode=620"),
    );
    for target in ["/dev/shm", "/tmp"] {
        mount_or_report(
            "tmpfs",
            target,
            "tmpfs",
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some("mode=1777"),
        );
    }
    mount_or_report(
        "tmpfs",
        "/run",
        "tmpfs",
        MsFlags::MS_NOSUID,
        Some("mode=0755"),
    );
}

fn mount_or_report(source: &str, target: &str, fstype: &str, flags: MsFlags, data: Option<&str>) {
    match mount(Some(source), target, Some(fstype), flags, data) {
        Ok(()) => {}
        Err(Errno::EBUSY) if is_mountpoint(target) => {
            crate::info!("{target} is already a mount point; leaving it alone");
        }
        Err(Errno::EBUSY) => {
            crate::error!("mount {fstype} on {target}: target is busy but is not a mount point");
        }
        Err(e) => crate::error!("mount {fstype} on {target} failed: {e}"),
    }
}

/// Move an existing mount point to a new location (used before switch_root).
pub fn mount_move(source: &str, target: &str) -> Result<(), Errno> {
    mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_MOVE,
        None::<&str>,
    )
}

/// Atomically move the new root onto `/` (the classic switch_root trick).
///
/// Must be called after `chdir` into the new root.
pub fn switch_root_move() -> Result<(), Errno> {
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
}
