//! Suspend and hibernate through the kernel's `/sys/power/state`.
//!
//! The control socket only accepts string states, so these helpers check that
//! the running kernel supports the requested state before writing it, and
//! flush the page cache first. `mem` is suspend-to-RAM, `disk` is
//! hibernate-to-disk (which needs a `resume=`/swap setup the kernel already
//! knows about).

use anyhow::{bail, Context, Result};
use std::path::Path;

pub const STATE_PATH: &str = "/sys/power/state";

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
    fn writes_the_requested_state_after_checking_support() {
        let path = std::env::temp_dir().join(format!("jiminit-power-write-{}", std::process::id()));
        std::fs::write(&path, "freeze mem disk\n").unwrap();
        enter_at(&path, "mem").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mem\n");
        let _ = std::fs::remove_file(&path);
    }
}
