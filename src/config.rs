//! Configuration file (`/etc/jiminit.toml`), with defaults so a missing or
//! partial file still boots.

use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_PATH: &str = "/etc/jiminit.toml";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
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
    /// Directory with `*.toml` service definitions.
    pub services_dir: String,
    /// Runlevel entered at boot: `single`, `multi-user` or `graphical`
    /// (numeric aliases `1`/`3`/`5` are accepted too).
    pub default_runlevel: String,
    /// Program that opens a terminal and prompts for login.
    pub getty: String,
    /// Baud rate passed to getty for virtual terminals.
    pub getty_baud: String,
    /// Baud rate passed to getty for serial terminals.
    pub serial_getty_baud: String,
    /// Virtual terminals that get a getty.
    pub ttys: Vec<String>,
    /// Serial terminals that get a getty when the device exists.
    pub serial_ttys: Vec<String>,
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
            services_dir: crate::service::DEFAULT_DIR.to_string(),
            default_runlevel: "multi-user".to_string(),
            getty: "/sbin/getty".to_string(),
            getty_baud: "38400".to_string(),
            serial_getty_baud: "115200".to_string(),
            ttys: (1..=6).map(|n| format!("/dev/tty{n}")).collect(),
            serial_ttys: vec!["/dev/ttyS0".to_string()],
        }
    }
}

/// Load the configuration, falling back to defaults with a log message on any
/// problem. A missing file is normal.
pub fn load(path: &Path) -> Config {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                crate::warn!("{}: invalid config ({e}); using defaults", path.display());
                Config::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::info!("{}: not found; using defaults", path.display());
            Config::default()
        }
        Err(e) => {
            crate::warn!("{}: cannot read ({e}); using defaults", path.display());
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
