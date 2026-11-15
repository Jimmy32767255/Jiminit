//! Best-effort modalias coldplug for real hardware.
//!
//! There is no udev in this initramfs: every device that the kernel has
//! already discovered gets its `modalias` passed to `modprobe`, which matches
//! it against the module alias table. Explicitly listed modules in
//! `/etc/modules` are loaded before this, so they always win.

use std::collections::BTreeSet;
use std::path::Path;

pub const SYSFS_BUS: &str = "/sys/bus";
/// Bound the work; real machines are far below this.
const MAX_ALIASES: usize = 512;

pub fn load() -> usize {
    load_from(Path::new(SYSFS_BUS))
}

pub fn load_from(bus_dir: &Path) -> usize {
    let aliases = collect_aliases(bus_dir);
    if aliases.is_empty() {
        crate::info!("coldplug: no modalias entries found");
        return 0;
    }
    crate::info!("coldplug: {} distinct modalias entries", aliases.len());

    let Some(program) = crate::modules::find_modprobe() else {
        crate::warn!("coldplug: modprobe not found; skipping module autoload");
        return 0;
    };

    let mut matched = 0;
    for alias in aliases.iter().take(MAX_ALIASES) {
        match std::process::Command::new(&program)
            .arg("-q")
            .arg(alias)
            .status()
        {
            Ok(status) if status.success() => matched += 1,
            Ok(status) => crate::debug!("coldplug: modprobe {alias}: {status}"),
            Err(e) => crate::error!("coldplug: run {program} {alias}: {e}"),
        }
    }
    crate::info!(
        "coldplug: processed {} of {} aliases ({matched} reported success)",
        aliases.len().min(MAX_ALIASES),
        aliases.len()
    );
    matched
}

fn collect_aliases(bus_dir: &Path) -> BTreeSet<String> {
    let mut aliases = BTreeSet::new();
    let Ok(buses) = std::fs::read_dir(bus_dir) else {
        return aliases;
    };
    for bus in buses.flatten() {
        let devices = bus.path().join("devices");
        let Ok(entries) = std::fs::read_dir(devices) else {
            continue;
        };
        for entry in entries.flatten() {
            let modalias = entry.path().join("modalias");
            if let Ok(text) = std::fs::read_to_string(&modalias) {
                let alias = text.trim();
                if !alias.is_empty() {
                    aliases.insert(alias.to_string());
                }
            }
        }
    }
    aliases
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("jiminit-coldplug-{}-{name}", std::process::id()))
    }

    #[test]
    fn collects_and_deduplicates_aliases() {
        let root = scratch("collect");
        let pci = root.join("pci/devices/0000:00:01.0");
        let virtio = root.join("virtio/devices/virtio0");
        std::fs::create_dir_all(&pci).unwrap();
        std::fs::create_dir_all(&virtio).unwrap();
        std::fs::write(pci.join("modalias"), "pci:v00001AF4d00000001\n").unwrap();
        std::fs::write(virtio.join("modalias"), "virtio:d00000002\n").unwrap();
        std::fs::write(pci.join("modalias"), "pci:v00001AF4d00000001\n").unwrap();

        let aliases = collect_aliases(&root);
        assert_eq!(
            aliases.into_iter().collect::<Vec<_>>(),
            vec!["pci:v00001AF4d00000001", "virtio:d00000002"]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_bus_directory_is_empty() {
        assert!(collect_aliases(Path::new("/nonexistent/bus")).is_empty());
    }
}
