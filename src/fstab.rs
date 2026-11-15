//! `/etc/fstab` parsing for the initramfs stage.
//!
//! Only the entries that make sense before `switch_root` are kept: the root
//! entry (it is already mounted from `root=`), swap, pseudo filesystems and
//! `noauto` entries are filtered out. Duplicate mountpoints keep the first
//! entry, like mount(8) would.

use anyhow::{Context, Result};
use nix::mount::MsFlags;
use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::Path;

const PSEUDO_FILESYSTEMS: &[&str] = &[
    "binfmt_misc",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "proc",
    "pstore",
    "securityfs",
    "sysfs",
    "tracefs",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    pub options: Vec<String>,
}

impl Entry {
    pub fn has_option(&self, name: &str) -> bool {
        self.options.iter().any(|option| option == name)
    }

    /// `nofail` means a failure is logged but does not stop the boot.
    pub fn nofail(&self) -> bool {
        self.has_option("nofail")
    }

    /// Split fstab options into generic mount flags and the fstype-specific
    /// data string. Options that mount(8) consumes itself (`_netdev`, `x-*`,
    /// `comment=`, `user`, ...) are dropped, `bind`/`rbind` become mount
    /// flags, and everything else is passed as filesystem data, which is how
    /// options such as `data=journal` or `compress=zstd` reach the kernel.
    pub fn mount_options(&self) -> (MsFlags, Option<String>) {
        let mut flags = MsFlags::empty();
        let mut data = Vec::new();

        for option in &self.options {
            match option.as_str() {
                "ro" => flags |= MsFlags::MS_RDONLY,
                "rw" => flags.remove(MsFlags::MS_RDONLY),
                "bind" => flags |= MsFlags::MS_BIND,
                "rbind" => flags |= MsFlags::MS_BIND | MsFlags::MS_REC,
                "suid" => flags.remove(MsFlags::MS_NOSUID),
                "nosuid" => flags |= MsFlags::MS_NOSUID,
                "dev" => flags.remove(MsFlags::MS_NODEV),
                "nodev" => flags |= MsFlags::MS_NODEV,
                "exec" => flags.remove(MsFlags::MS_NOEXEC),
                "noexec" => flags |= MsFlags::MS_NOEXEC,
                "sync" => flags |= MsFlags::MS_SYNCHRONOUS,
                "async" => flags.remove(MsFlags::MS_SYNCHRONOUS),
                "dirsync" => flags |= MsFlags::MS_DIRSYNC,
                "noatime" => flags |= MsFlags::MS_NOATIME,
                "atime" => flags.remove(MsFlags::MS_NOATIME),
                "nodiratime" => flags |= MsFlags::MS_NODIRATIME,
                "relatime" => flags |= MsFlags::MS_RELATIME,
                "norelatime" => flags.remove(MsFlags::MS_RELATIME),
                "strictatime" => flags |= MsFlags::MS_STRICTATIME,
                "lazytime" => flags |= MsFlags::MS_LAZYTIME,
                // Filesystem-specific options go to mount(2) as data.
                other if !is_userspace_option(other) => data.push(other.to_string()),
                // Options that only mount(8)/systemd consume; never pass them
                // to mount(2), where ext4/xfs would reject the whole mount.
                _ => {}
            }
        }

        let data = if data.is_empty() {
            None
        } else {
            Some(data.join(","))
        };
        (flags, data)
    }
}

/// Options that only exist in the mount(8)/systemd fstab dialect.
fn is_userspace_option(option: &str) -> bool {
    matches!(
        option,
        "defaults"
            | "auto"
            | "noauto"
            | "nofail"
            | "user"
            | "users"
            | "nouser"
            | "owner"
            | "group"
            | "quiet"
            | "silent"
            | "loud"
            | "bg"
            | "fg"
            | "fail"
    ) || option == "_netdev"
        || option.starts_with("x-")
        || option.starts_with("comment=")
}

/// Parse fstab text into the entries worth mounting in the initramfs.
pub fn parse(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for (number, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            crate::error!(
                "fstab: line {} has fewer than 3 fields; skipping",
                number + 1
            );
            continue;
        }

        let device = unescape(fields[0]);
        let raw_mountpoint = unescape(fields[1]);
        let fstype = fields[2].to_ascii_lowercase();
        let options: Vec<String> = fields
            .get(3)
            .map(|options| {
                options
                    .split(',')
                    .filter(|option| !option.is_empty())
                    .map(unescape)
                    .collect()
            })
            .unwrap_or_default();

        if raw_mountpoint == "none" || device == "none" {
            continue;
        }
        let Some(mountpoint) = normalize_mountpoint(&raw_mountpoint) else {
            crate::error!(
                "fstab: {} is not a usable absolute mountpoint; skipping",
                fields[1]
            );
            continue;
        };
        if mountpoint == "/" {
            continue;
        }
        if fstype == "swap" || PSEUDO_FILESYSTEMS.contains(&fstype.as_str()) {
            continue;
        }
        if options.iter().any(|option| option == "noauto") {
            continue;
        }
        if !seen.insert(mountpoint.clone()) {
            crate::warn!("fstab: duplicate mountpoint {mountpoint}; keeping the first entry");
            continue;
        }

        entries.push(Entry {
            device,
            mountpoint,
            fstype,
            options,
        });
    }

    entries
}

/// Normalize a mount point without touching the filesystem: collapse repeated
/// slashes and `.` components, resolve `..`, and reject paths that would climb
/// above `/`. `fstab` normally has clean paths, but `/data/` and `/data` must
/// not both be mounted, and `/data/../etc` must mean `/etc`.
fn normalize_mountpoint(value: &str) -> Option<String> {
    if !value.starts_with('/') {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }

    let mut normalized = String::from("/");
    normalized.push_str(&parts.join("/"));
    Some(normalized)
}

/// Read and parse an fstab file; a missing file simply yields no entries.
pub fn load(path: &Path) -> Result<Vec<Entry>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Decode the octal escapes fstab uses for whitespace (`\040`, `\011`, ...).
fn unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &bytes[index + 1..index + 4];
            if digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                let code = (digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0');
                out.push(code);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    // fstab is UTF-8; decode after escaping so non-ASCII paths survive and
    // no &str is sliced at a non-char boundary (which would panic).
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_escapes_without_corrupting_utf8() {
        assert_eq!(unescape("/mnt/my\\040data"), "/mnt/my data");
        assert_eq!(unescape("/mnt/数据"), "/mnt/数据");
        assert_eq!(unescape("/mnt/\\a数"), "/mnt/\\a数");
    }

    #[test]
    fn drops_userspace_options_and_maps_bind_and_rw() {
        let entry = |options: &str| Entry {
            device: "/dev/vdb".into(),
            mountpoint: "/data".into(),
            fstype: "ext4".into(),
            options: options.split(',').map(str::to_string).collect(),
        };

        let (flags, data) = entry("ro,rw,defaults,compress=zstd").mount_options();
        assert!(!flags.contains(MsFlags::MS_RDONLY));
        assert_eq!(data.as_deref(), Some("compress=zstd"));

        let (flags, data) =
            entry("_netdev,x-systemd.device-timeout=10,comment=home").mount_options();
        assert!(flags.is_empty());
        assert_eq!(data, None);

        let (flags, _) = entry("bind").mount_options();
        assert!(flags.contains(MsFlags::MS_BIND));
        let (flags, _) = entry("rbind").mount_options();
        assert!(flags.contains(MsFlags::MS_BIND) && flags.contains(MsFlags::MS_REC));
    }

    #[test]
    fn skips_root_swap_pseudo_and_noauto_entries() {
        let text = "\
# comment
/dev/vda1 / ext4 defaults 0 1
proc /proc proc defaults 0 0
/dev/vdb /data ext4 defaults 0 2
/dev/vdc /backup ext4 defaults,noauto 0 2
/dev/vdd none swap sw 0 0
";
        let entries = parse(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mountpoint, "/data");
        assert_eq!(entries[0].device, "/dev/vdb");
        assert_eq!(entries[0].fstype, "ext4");
    }

    #[test]
    fn unescapes_spaces_and_tabs() {
        let text = "/dev/vdb /mnt/my\\040disk ext4 defaults 0 2\n";
        let entries = parse(text);
        assert_eq!(entries[0].mountpoint, "/mnt/my disk");
    }

    #[test]
    fn keeps_first_of_duplicate_mountpoints() {
        let text = "\
/dev/vdb /data ext4 defaults 0 2
/dev/vdc /data ext4 defaults 0 2
";
        let entries = parse(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].device, "/dev/vdb");
    }

    #[test]
    fn normalizes_mountpoints_before_deduplicating() {
        let text = "\
/dev/vdb /data/ ext4 defaults 0 2
/dev/vdc /data ext4 defaults 0 2
/dev/vdd /data/../etc ext4 defaults 0 2
/dev/vde /../escape ext4 defaults 0 2
";
        let entries = parse(text);
        assert_eq!(entries.len(), 2, "entries: {entries:?}");
        assert_eq!(entries[0].mountpoint, "/data");
        assert_eq!(entries[1].mountpoint, "/etc");
    }

    #[test]
    fn splits_generic_flags_from_fs_specific_data() {
        let entry = Entry {
            device: "/dev/vdb".into(),
            mountpoint: "/data".into(),
            fstype: "ext4".into(),
            options: ["ro", "nosuid", "nodev", "noatime", "nofail", "data=journal"]
                .map(String::from)
                .to_vec(),
        };
        let (flags, data) = entry.mount_options();
        assert!(flags.contains(MsFlags::MS_RDONLY));
        assert!(flags.contains(MsFlags::MS_NOSUID));
        assert!(flags.contains(MsFlags::MS_NODEV));
        assert!(flags.contains(MsFlags::MS_NOATIME));
        assert_eq!(data.as_deref(), Some("data=journal"));
        assert!(entry.nofail());
    }
}
