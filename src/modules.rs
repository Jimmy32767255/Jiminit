//! Load kernel modules listed in the initramfs' `/etc/modules` before the
//! root device is resolved.
//!
//! `/etc/modprobe.d/*.conf` rules that matter for an explicit load are
//! honoured: `options`, `blacklist`, `install` and `alias`. Other directives
//! (`softdep`, `weakdep`, `remove`, ...) are left to modprobe itself.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

pub const MODULES_FILE: &str = "/etc/modules";
pub const MODPROBE_DIR: &str = "/etc/modprobe.d";

/// Upper bound for a single modprobe/install helper invocation.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rules {
    pub options: BTreeMap<String, Vec<String>>,
    pub blacklist: BTreeSet<String>,
    pub install: BTreeMap<String, String>,
    pub aliases: BTreeMap<String, String>,
}

/// Parse `/etc/modules`: one `module [params...]` per line, `#` comments.
pub fn parse_modules(text: &str) -> Vec<(String, Vec<String>)> {
    let mut modules = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let params = fields.map(str::to_string).collect();
        modules.push((name.to_string(), params));
    }
    modules
}

/// Read every `*.conf` in `dir` (sorted by name) and merge their rules.
pub fn parse_rules(dir: &Path) -> Rules {
    let mut rules = Rules::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return rules,
        Err(e) => {
            crate::error!("modules: cannot read {}: {e}", dir.display());
            return rules;
        }
    };

    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
        .collect();
    files.sort();

    for path in files {
        match std::fs::read_to_string(&path) {
            Ok(text) => merge_config(&text, &mut rules),
            Err(e) => crate::error!("modules: cannot read {}: {e}", path.display()),
        }
    }
    rules
}

fn merge_config(text: &str, rules: &mut Rules) {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            ["options", module, rest @ ..] if !rest.is_empty() => {
                rules
                    .options
                    .entry(module.to_string())
                    .or_default()
                    .extend(rest.iter().map(|s| s.to_string()));
            }
            ["blacklist", module] => {
                rules.blacklist.insert(module.to_string());
            }
            ["install", module, rest @ ..] if !rest.is_empty() => {
                rules.install.insert(module.to_string(), rest.join(" "));
            }
            ["alias", name, target] => {
                rules.aliases.insert(name.to_string(), target.to_string());
            }
            _ => {}
        }
    }
}

/// Load every module from `modules_path`, applying the rules from `rules_dir`.
///
/// This stage fails fast: a missing modprobe or any failing modprobe/install
/// command aborts the boot, so a broken initramfs ends up in the rescue shell
/// instead of continuing without the storage driver the root device needs.
pub fn load(modules_path: &Path, rules_dir: &Path) -> Result<()> {
    let text = match std::fs::read_to_string(modules_path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            crate::info!(
                "modules: {} not present; nothing to load",
                modules_path.display()
            );
            return Ok(());
        }
        Err(e) => {
            return Err(e).with_context(|| format!("read {}", modules_path.display()));
        }
    };

    let modules = parse_modules(&text);
    if modules.is_empty() {
        crate::info!("modules: {} lists no modules", modules_path.display());
        return Ok(());
    }

    let rules = parse_rules(rules_dir);
    let Some(modprobe) = find_modprobe() else {
        bail!(
            "no modprobe program in the initramfs but {} module(s) are listed in {}",
            modules.len(),
            modules_path.display()
        );
    };

    for (name, params) in modules {
        load_one(modprobe, &rules, &name, &params)
            .with_context(|| format!("load kernel module {name}"))?;
    }
    Ok(())
}

fn load_one(modprobe: &str, rules: &Rules, name: &str, params: &[String]) -> Result<()> {
    let alias = rules.aliases.get(name).map(String::as_str);
    let target = alias.unwrap_or(name);

    // Like modprobe(8), a blacklist only suppresses alias lookups. A module
    // named explicitly in /etc/modules is still loaded: otherwise a blacklisted
    // storage driver would silently make the root device unreachable.
    if alias.is_some() {
        if rules.blacklist.contains(target) {
            crate::info!("modules: alias {name} -> {target} is blacklisted; skipping");
            return Ok(());
        }
    } else if rules.blacklist.contains(name) {
        crate::warn!("modules: {name} is blacklisted but listed explicitly; loading it");
    }

    if let Some(command) = rules.install.get(target) {
        crate::info!("modules: install {target}: {command}");
        return run_install(command, target);
    }

    let args: Vec<String> = if params.is_empty() {
        rules.options.get(target).cloned().unwrap_or_default()
    } else {
        params.to_vec()
    };

    if args.is_empty() {
        crate::info!("modules: modprobe {target}");
    } else {
        crate::info!("modules: modprobe {target} {}", args.join(" "));
    }

    let mut command = std::process::Command::new(modprobe);
    command.arg(target).args(&args);
    let status = run_with_timeout(command).with_context(|| format!("run {modprobe} {target}"))?;
    check_status("modprobe", target, status)
}

/// `install` commands are shell command lines, like modprobe.d specifies.
fn run_install(command: &str, target: &str) -> Result<()> {
    let Some(shell) = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
    else {
        bail!("no shell available to run the install command for {target}");
    };

    let mut command_line = std::process::Command::new(shell);
    command_line.arg("-c").arg(command);
    let status = run_with_timeout(command_line)
        .with_context(|| format!("run install command for {target}: {command}"))?;
    check_status("install command for", target, status)
}

/// Run a helper with a hard timeout: a hung modprobe or install command must
/// not block the initramfs forever.
fn run_with_timeout(mut command: std::process::Command) -> Result<ExitStatus> {
    let label = format!("waiting for {}", command.get_program().to_string_lossy());
    let mut child = command.spawn().context("spawn helper")?;
    let mut wait = crate::log::Wait::start(label, Some(COMMAND_TIMEOUT));
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        wait.tick();
        if let Some(status) = child.try_wait().context("wait for helper")? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("helper timed out after {COMMAND_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn check_status(what: &str, target: &str, status: ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("{what} {target} exited with {status}")
    }
}

pub(crate) fn find_modprobe() -> Option<&'static str> {
    [
        "/sbin/modprobe",
        "/bin/modprobe",
        "/usr/sbin/modprobe",
        "/usr/bin/modprobe",
    ]
    .into_iter()
    .find(|candidate| Path::new(candidate).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modules_with_comments_and_params() {
        let text = "\
# storage and network
virtio_blk
e1000e InterruptThrottleRate=3000
";
        let modules = parse_modules(text);
        assert_eq!(
            modules,
            vec![
                ("virtio_blk".to_string(), vec![]),
                (
                    "e1000e".to_string(),
                    vec!["InterruptThrottleRate=3000".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn merges_modprobe_d_directives() {
        let mut rules = Rules::default();
        merge_config(
            "\
blacklist floppy
options e1000e InterruptThrottleRate=3000
install baz echo installed-baz
alias mynet e1000e
softdep foo pre: bar
",
            &mut rules,
        );
        assert!(rules.blacklist.contains("floppy"));
        assert_eq!(
            rules.options["e1000e"],
            vec!["InterruptThrottleRate=3000".to_string()]
        );
        assert_eq!(
            rules.install.get("baz").map(String::as_str),
            Some("echo installed-baz")
        );
        assert_eq!(
            rules.aliases.get("mynet").map(String::as_str),
            Some("e1000e")
        );
    }

    #[test]
    fn a_failing_modprobe_is_an_error() {
        use std::os::unix::process::ExitStatusExt;

        assert!(check_status("modprobe", "foo", ExitStatus::from_raw(0)).is_ok());
        assert!(check_status("modprobe", "foo", ExitStatus::from_raw(1 << 8)).is_err());
    }

    #[test]
    fn later_rules_override_install_commands() {
        let mut rules = Rules::default();
        merge_config("install baz echo one\n", &mut rules);
        merge_config("install baz echo two\n", &mut rules);
        assert_eq!(
            rules.install.get("baz").map(String::as_str),
            Some("echo two")
        );
    }
}
