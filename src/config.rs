//! Configuration file (`/etc/jiminit.toml`). Missing keys fall back to
//! defaults; unknown keys and malformed values are hard errors, so a typo
//! cannot silently change the boot into something unexpected.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_PATH: &str = "/etc/jiminit.toml";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Host name to set in stage 2.
    pub hostname: String,
    /// Console (tty) device used for the init log and the root shell.
    pub console: String,
    /// Shell started on the console.
    pub shell: String,
    /// Restart the shell after it exits.
    pub respawn: bool,
    /// How long to wait for processes to exit during shutdown.
    pub shutdown_timeout_ms: u64,
    /// Path of the stage 2 binary inside the real root filesystem.
    pub init_path: String,
    /// Append init logs here (empty disables the file sink).
    pub log_file: String,
    /// Directory with `*.toml` service definitions.
    pub services_dir: String,
    /// Put each service into its own cgroup v2 subtree.
    pub cgroup: bool,
    /// How long a `notify = true` service may take to send READY=1.
    pub ready_timeout_ms: u64,
    /// Runlevel entered at boot: `single`, `multi-user` or `graphical`
    /// (numeric aliases `1`/`3`/`5` are accepted too).
    pub default_runlevel: String,
    /// Program that opens a terminal and prompts for login.
    pub getty: String,
    /// Baud rate passed to getty for virtual terminals.
    pub getty_baud: String,
    /// Baud rate passed to getty for serial terminals.
    pub serial_getty_baud: String,
    /// Pass `-L <baud> <tty>` (busybox getty) instead of `-L <tty> <baud>`
    /// (util-linux agetty).
    pub getty_baud_first: bool,
    /// Strip the leading /dev/ when passing the tty name. util-linux agetty
    /// prepends /dev/ itself, so it needs "tty1" instead of "/dev/tty1".
    pub getty_tty_basename: bool,
    /// Virtual terminals that get a getty.
    pub ttys: Vec<String>,
    /// Serial terminals that get a getty when the device exists.
    pub serial_ttys: Vec<String>,
    /// Terminal colors: `auto`, `always` or `never`.
    pub color: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hostname: "jiminit".to_string(),
            console: "/dev/console".to_string(),
            shell: "/bin/sh".to_string(),
            respawn: true,
            shutdown_timeout_ms: 3000,
            init_path: "/sbin/jiminit".to_string(),
            log_file: "/var/log/jiminit.log".to_string(),
            services_dir: crate::service::DEFAULT_DIR.to_string(),
            cgroup: true,
            ready_timeout_ms: 5_000,
            default_runlevel: "multi-user".to_string(),
            getty: "/sbin/getty".to_string(),
            getty_baud: "38400".to_string(),
            serial_getty_baud: "115200".to_string(),
            getty_baud_first: true,
            getty_tty_basename: false,
            ttys: (1..=6).map(|n| format!("/dev/tty{n}")).collect(),
            serial_ttys: vec!["/dev/ttyS0".to_string()],
            color: "auto".to_string(),
        }
    }
}

impl Config {
    /// Parsed [`crate::log::Color`]; an invalid value falls back to `auto`.
    pub fn color_choice(&self) -> crate::log::Color {
        match crate::log::Color::parse(&self.color) {
            Some(choice) => choice,
            None => {
                crate::warn!(
                    "color = {:?} is not auto/always/never; using auto",
                    self.color
                );
                crate::log::Color::Auto
            }
        }
    }
}

/// Load the configuration. A missing file is normal and yields defaults; a
/// read error or a malformed file (unknown keys, wrong types) is returned to
/// the caller so that stage 1/stage 2 can fall back to the rescue shell.
pub fn load(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let cfg = toml::from_str::<Config>(&text)
                .with_context(|| format!("parse {}", path.display()))?;
            Ok(cfg)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::info!("{}: not found; using defaults", path.display());
            Ok(Config::default())
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Like [`load`], but logs the error and falls back to defaults. Only the
/// rescue shell uses this: it must keep working even with a broken config.
pub fn load_or_default(path: &Path) -> Config {
    match load(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            crate::warn!("{e:#}; using defaults");
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.hostname, "jiminit");
        assert_eq!(cfg.console, "/dev/console");
        assert_eq!(cfg.shell, "/bin/sh");
        assert!(cfg.respawn);
        assert_eq!(cfg.shutdown_timeout_ms, 3000);
        assert_eq!(cfg.init_path, "/sbin/jiminit");
        assert_eq!(cfg.services_dir, "/etc/jiminit/services.d");
        assert_eq!(cfg.default_runlevel, "multi-user");
        assert_eq!(cfg.getty, "/sbin/getty");
        assert_eq!(cfg.ttys.len(), 6);
        assert_eq!(cfg.serial_ttys, ["/dev/ttyS0"]);
        assert_eq!(cfg.color, "auto");
    }

    #[test]
    fn partial_override_keeps_other_defaults() {
        let cfg: Config = toml::from_str("hostname = \"qemu\"\nrespawn = false\n").unwrap();
        assert_eq!(cfg.hostname, "qemu");
        assert!(!cfg.respawn);
        assert_eq!(cfg.console, "/dev/console");
        assert_eq!(cfg.init_path, "/sbin/jiminit");
    }

    #[test]
    fn load_rejects_unknown_keys_and_bad_types() {
        let path = std::env::temp_dir().join(format!(
            "jiminit-config-{}-{}.toml",
            std::process::id(),
            line!()
        ));

        std::fs::write(&path, "hostnam = \"typo\"\n").unwrap();
        assert!(load(&path).is_err(), "unknown key was accepted");

        std::fs::write(&path, "hostname = \"x\"\nrespawn = \"yes\"\n").unwrap();
        assert!(load(&path).is_err(), "wrong value type was accepted");

        std::fs::write(&path, "hostname = \"ok\"\n").unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.hostname, "ok");
        assert_eq!(cfg.console, "/dev/console");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn color_choice_falls_back_to_auto() {
        let mut cfg = Config::default();
        assert_eq!(cfg.color_choice(), crate::log::Color::Auto);
        cfg.color = "always".into();
        assert_eq!(cfg.color_choice(), crate::log::Color::Always);
        cfg.color = "sometimes".into();
        assert_eq!(cfg.color_choice(), crate::log::Color::Auto);
    }

    #[test]
    fn load_missing_file_uses_defaults() {
        let path = std::env::temp_dir().join("jiminit-config-does-not-exist.toml");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load(&path).unwrap(), Config::default());
    }

    #[test]
    fn full_config() {
        let text = r#"
hostname = "jiminit-qemu"
console = "/dev/ttyS0"
shell = "/bin/sh"
respawn = true
shutdown_timeout_ms = 1500
init_path = "/sbin/jiminit"
default_runlevel = "graphical"
ttys = ["/dev/ttyS0"]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.console, "/dev/ttyS0");
        assert_eq!(cfg.shutdown_timeout_ms, 1500);
        assert_eq!(cfg.default_runlevel, "graphical");
        assert_eq!(
            cfg,
            Config {
                hostname: "jiminit-qemu".into(),
                console: "/dev/ttyS0".into(),
                shutdown_timeout_ms: 1500,
                default_runlevel: "graphical".into(),
                ttys: vec!["/dev/ttyS0".into()],
                ..Config::default()
            }
        );
    }
}
