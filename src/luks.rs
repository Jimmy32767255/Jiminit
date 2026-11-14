//! Unlock the dm-crypt volumes listed in the initramfs' `/etc/crypttab`.
//!
//! The actual work is done by `cryptsetup`; this module only parses the
//! configuration, resolves devices, retries failed passphrase attempts and
//! checks that the mapper device really appeared. Failures are fatal: the
//! caller (stage 1) falls back to the rescue shell.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::crypttab::Entry;

pub const CRYPTTAB: &str = "/etc/crypttab";

/// Number of passphrase attempts per volume before giving up.
const ATTEMPTS: usize = 3;

pub fn unlock_all(path: &Path) -> Result<()> {
    let entries = crate::crypttab::load(path)?;
    if entries.is_empty() {
        crate::info!("crypttab: no encrypted volumes to unlock");
        return Ok(());
    }

    let Some(program) = find_cryptsetup() else {
        bail!(
            "{} lists {} encrypted volume(s) but no cryptsetup program is available",
            path.display(),
            entries.len()
        );
    };

    for entry in &entries {
        unlock(program, entry)?;
    }
    Ok(())
}

fn unlock(program: &str, entry: &Entry) -> Result<()> {
    let mapper = format!("/dev/mapper/{}", entry.name);
    if Path::new(&mapper).exists() {
        crate::info!(
            "crypttab: {} is already mapped at {mapper}; skipping",
            entry.device
        );
        return Ok(());
    }

    let device = crate::rootdev::resolve_with_retry(&entry.device, 10, Duration::from_millis(100))
        .with_context(|| format!("resolve crypttab device {}", entry.device))?;

    let mut command = Command::new(program);
    command.arg("luksOpen").args(["--tries", "1"]);
    if let Some(keyfile) = &entry.keyfile {
        command.arg("--key-file").arg(keyfile);
    }
    add_options(&mut command, entry);
    command.arg(&device).arg(&entry.name);

    let mut last_error = String::new();
    for attempt in 1..=ATTEMPTS {
        crate::info!(
            "crypttab: unlocking {} as {} (attempt {attempt}/{ATTEMPTS})",
            device.display(),
            entry.name
        );
        match command.status() {
            Ok(status) if status.success() => {
                if !Path::new(&mapper).exists() {
                    bail!("cryptsetup reported success but {mapper} does not exist");
                }
                crate::info!("crypttab: {} is ready", entry.name);
                return Ok(());
            }
            Ok(status) => last_error = format!("cryptsetup exited with {status}"),
            Err(e) => last_error = format!("cannot run {program}: {e}"),
        }
        crate::warn!("crypttab: {} not unlocked ({last_error})", entry.name);
    }

    bail!(
        "crypttab: {} could not be unlocked after {ATTEMPTS} attempts: {last_error}",
        entry.name
    )
}

/// Translate crypttab options into cryptsetup flags. Unknown mount-time
/// options are ignored with a warning instead of failing the boot.
fn add_options(command: &mut Command, entry: &Entry) {
    for option in &entry.options {
        match option.as_str() {
            "luks" | "none" | "noearly" | "nofail" | "noauto" => {}
            "discard" | "allow-discards" => {
                command.arg("--allow-discards");
            }
            "readonly" | "read-only" => {
                command.arg("--readonly");
            }
            other if other.starts_with("keyslot=") => {
                command.arg("--key-slot").arg(&other["keyslot=".len()..]);
            }
            other if other.starts_with("header=") => {
                command.arg("--header").arg(&other["header=".len()..]);
            }
            other => crate::warn!(
                "crypttab: ignoring unsupported option {other} for {}",
                entry.name
            ),
        }
    }
}

fn find_cryptsetup() -> Option<&'static str> {
    [
        "/sbin/cryptsetup",
        "/bin/cryptsetup",
        "/usr/sbin/cryptsetup",
        "/usr/bin/cryptsetup",
    ]
    .into_iter()
    .find(|candidate| Path::new(candidate).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_no_cryptsetup_in_a_hosted_test_environment() {
        // Not an assertion about the host: only that probing never panics and
        // returns either a path that exists or None.
        if let Some(program) = find_cryptsetup() {
            assert!(Path::new(program).is_file());
        }
    }
}
