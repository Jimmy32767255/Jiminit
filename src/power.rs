//! Power helpers that talk to the kernel directly: suspend and hibernate
//! through `/sys/power/state`, plus the kexec-loaded flag.
//!
//! The control socket only accepts string states, so these helpers check that
//! the running kernel supports the requested state before writing it, and
//! flush the page cache first. `mem` is suspend-to-RAM, `disk` is
//! hibernate-to-disk (which needs a `resume=`/swap setup the kernel already
//! knows about).

use anyhow::{bail, Context, Result};
use std::path::Path;

pub const STATE_PATH: &str = "/sys/power/state";
pub const KEXEC_LOADED_PATH: &str = "/sys/kernel/kexec_loaded";

pub fn suspend() -> Result<()> {
    enter("mem")
}

pub fn hibernate() -> Result<()> {
    enter("disk")
}

/// Write `state` to `/sys/power/state`. Returns after the machine wakes up.
pub fn enter(state: &str) -> Result<()> {
    enter_at(Path::new(STATE_PATH), state)
}

fn enter_at(path: &Path, state: &str) -> Result<()> {
    let states = available_at(path)?;
    if !states.iter().any(|supported| supported == state) {
        bail!(
            "this kernel does not support `{state}` (available: {})",
            if states.is_empty() {
                "none".to_string()
            } else {
                states.join(" ")
            }
        );
    }
    // Flush the page cache before the machine freezes; filesystems and the
    // hibernate image must not depend on dirty pages that only reach the swap
    // file after the image was written.
    nix::unistd::sync();
    std::fs::write(path, format!("{state}\n"))
        .with_context(|| format!("write `{state}` to {}", path.display()))?;
    Ok(())
}

fn available_at(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(parse_states(&text))
}

fn parse_states(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Has a kernel image been loaded with `kexec -l`/`kexec_file_load`?
///
/// `reboot(RB_KEXEC)` without a loaded image would fail and leave the machine
/// stopped with a read-only root, so the shutdown path refuses to start it.
pub fn kexec_loaded() -> Result<bool> {
    kexec_loaded_at(Path::new(KEXEC_LOADED_PATH))
}

fn kexec_loaded_at(path: &Path) -> Result<bool> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {} (is CONFIG_KEXEC enabled?)", path.display()))?;
    Ok(parse_kexec_loaded(&text))
}

fn parse_kexec_loaded(text: &str) -> bool {
    text.trim() == "1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state_list() {
        assert_eq!(parse_states("freeze mem disk\n"), ["freeze", "mem", "disk"]);
        assert_eq!(parse_states("  mem  "), ["mem"]);
        assert!(parse_states("\n").is_empty());
    }

    #[test]
    fn rejects_states_the_kernel_does_not_list() {
        let path = std::env::temp_dir().join(format!("jiminit-power-{}", std::process::id()));
        std::fs::write(&path, "freeze mem\n").unwrap();
        let error = enter_at(&path, "disk").unwrap_err().to_string();
        assert!(error.contains("does not support `disk`"), "{error}");
        assert!(error.contains("freeze mem"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_the_kexec_loaded_flag() {
        assert!(parse_kexec_loaded("1\n"));
        assert!(!parse_kexec_loaded("0\n"));
        assert!(!parse_kexec_loaded(""));
    }

    #[test]
    fn reports_when_no_kernel_is_loaded() {
        let path = std::env::temp_dir().join(format!("jiminit-kexec-{}", std::process::id()));
        std::fs::write(&path, "0\n").unwrap();
        assert!(!kexec_loaded_at(&path).unwrap());
        std::fs::write(&path, "1\n").unwrap();
        assert!(kexec_loaded_at(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(kexec_loaded_at(&path).is_err());
    }

    #[test]
    fn writes_the_requested_state_after_checking_support() {
        let path = std::env::temp_dir().join(format!("jiminit-power-write-{}", std::process::id()));
        std::fs::write(&path, "freeze mem disk\n").unwrap();
        enter_at(&path, "mem").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mem\n");
        let _ = std::fs::remove_file(&path);
    }
}
