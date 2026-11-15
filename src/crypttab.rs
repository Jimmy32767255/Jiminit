//! `/etc/crypttab` parsing for the initramfs.
//!
//! Only the fields needed before switch_root are kept; mount-time options
//! handled by systemd (`x-systemd.*`, `nofail`, ...) are ignored later by the
//! unlock code with a warning.

use anyhow::{Context, Result};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Device-mapper name (`/dev/mapper/<name>`).
    pub name: String,
    /// Backing device: a path, `UUID=...` or `PARTUUID=...`.
    pub device: String,
    /// `None` means "ask for a passphrase" (`none`/`-` in the file).
    pub keyfile: Option<String>,
    pub options: Vec<String>,
}

/// Parse crypttab text: `name device keyfile options`.
pub fn parse(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();

    for (number, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            crate::error!(
                "crypttab: line {} has fewer than 2 fields; skipping",
                number + 1
            );
            continue;
        }

        let keyfile = match fields.get(2).copied() {
            None | Some("none") | Some("-") => None,
            Some(path) => Some(path.to_string()),
        };
        let options = fields
            .get(3)
            .map(|options| {
                options
                    .split(',')
                    .filter(|option| !option.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        entries.push(Entry {
            name: fields[0].to_string(),
            device: fields[1].to_string(),
            keyfile,
            options,
        });
    }

    entries
}

/// Read a crypttab file; a missing file simply yields no entries.
pub fn load(path: &Path) -> Result<Vec<Entry>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_keyfiles_and_options() {
        let text = "\
# root and swap
cryptroot UUID=1111 none luks,discard
cryptswap /dev/vda3 /etc/keys/swap.key luks,keyslot=1
";
        let entries = parse(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "cryptroot");
        assert_eq!(entries[0].device, "UUID=1111");
        assert_eq!(entries[0].keyfile, None);
        assert!(entries[0].options.iter().any(|o| o == "discard"));
        assert_eq!(entries[1].keyfile.as_deref(), Some("/etc/keys/swap.key"));
        assert!(entries[1].options.iter().any(|o| o == "keyslot=1"));
    }

    #[test]
    fn treats_dash_as_interactive_keyfile() {
        let entries = parse("root /dev/vda2 - luks\n");
        assert_eq!(entries[0].keyfile, None);
    }

    #[test]
    fn skips_short_lines() {
        assert!(parse("cryptroot\n\n").is_empty());
    }
}
