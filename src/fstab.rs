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
    /// data string. Unknown options are passed through as data, which is how
    /// filesystem-specific options (`data=journal`, `compress=zstd`, ...)
    /// reach the kernel.
    pub fn mount_options(&self) -> (MsFlags, Option<String>) {
        let mut flags = MsFlags::empty();
        let mut data = Vec::new();

        for option in &self.options {
            match option.as_str() {
                "ro" => flags |= MsFlags::MS_RDONLY,
                "rw" | "defaults" | "auto" | "noauto" | "nofail" | "user" | "users" | "owner"
                | "group" | "quiet" => {}
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
                other => data.push(other.to_string()),
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
            crate::warn!(
                "fstab: line {} has fewer than 3 fields; skipping",
                number + 1
            );
            continue;
        }

        let device = unescape(fields[0]);
        let mountpoint = unescape(fields[1]);
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

        if mountpoint == "/" || mountpoint == "none" || device == "none" {
            continue;
        }
        if !mountpoint.starts_with('/') {
            crate::warn!("fstab: {mountpoint} is not an absolute mountpoint; skipping");
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
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &value[index + 1..index + 4];
            if let Ok(code) = u8::from_str_radix(digits, 8) {
                out.push(code as char);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
