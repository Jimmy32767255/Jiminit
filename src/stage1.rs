//! Stage 1: runs from the initramfs, mounts the real root and switches to it.

use anyhow::{bail, Context, Result};
use nix::mount::MsFlags;
use nix::unistd::{chdir, chroot};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn run() -> Result<()> {
    crate::log::init();
    crate::info!("stage1: running from initramfs");

    crate::mount::mount_pseudo_fs();

    crate::modules::load(
        Path::new(crate::modules::MODULES_FILE),
        Path::new(crate::modules::MODPROBE_DIR),
    )?;

    let cmdline = crate::cmdline::Cmdline::read().context("read /proc/cmdline")?;
    let cfg = crate::config::load(Path::new("/etc/jiminit.toml"));

    let root_spec = cmdline
        .value("root")
        .context("kernel command line has no root=; nothing to mount")?;
    let fstype = cmdline.value("rootfstype");
    let data = cmdline.value("rootflags");
    let read_only = !cmdline.has("rw");

    crate::info!(
        "resolving root={root_spec} (fstype={}, read_only={read_only})",
        fstype.unwrap_or("auto")
    );
    let device =
        crate::rootdev::resolve_with_retry(root_spec, 20, std::time::Duration::from_millis(250))?;
    crate::info!("root device resolved to {}", device.display());

    fsck_root(&device, &cmdline)?;

    std::fs::create_dir_all("/newroot").context("create /newroot")?;

    let mut flags = MsFlags::empty();
    if read_only {
        flags |= MsFlags::MS_RDONLY;
    }
    mount_root(&device, "/newroot", fstype, flags, data)?;
    crate::info!("mounted {} on /newroot", device.display());

    mount_fstab("/newroot")?;

    let init_path = cfg.init_path.clone();
    let stage2_path = Path::new("/newroot").join(init_path.trim_start_matches('/'));
    if !stage2_path.exists() {
        bail!(
            "{} not found in the new root; install the jiminit binary there",
            stage2_path.display()
        );
    }

    // Move the pseudo filesystems into the new root so they survive the switch.
    for sub in ["/dev", "/proc", "/sys"] {
        let target = format!("/newroot{sub}");
        if let Err(e) = std::fs::create_dir_all(&target) {
            crate::warn!("create {target} failed: {e}");
        }
        match crate::mount::mount_move(sub, &target) {
            Ok(()) => crate::info!("moved {sub} to {target}"),
            Err(e) => crate::warn!("moving {sub} to {target} failed: {e}"),
        }
    }

    chdir("/newroot").context("chdir /newroot")?;
    match crate::mount::switch_root_move() {
        Ok(()) => {
            crate::info!("moved new root onto /");
            // Re-anchor the process root on the moved mount, like busybox
            // switch_root does; without this the old rootfs stays current.
            chroot(".").context("chroot . after switch_root")?;
        }
        Err(e) => {
            crate::warn!("switch_root move failed ({e}); falling back to chroot");
            chroot("/newroot").context("chroot /newroot")?;
        }
    }
    chdir("/").context("chdir / after switch_root")?;

    exec_stage2(&init_path)?;

    // exec only returns on failure.
    unreachable!()
}

/// Run `fsck -p` on the root device before mounting it.
///
/// The initramfs decides whether fsck is available: no program means the
/// check is skipped with a log line, while a failing check refuses to mount
/// the filesystem (the caller falls back to the rescue shell).
fn fsck_root(device: &Path, cmdline: &crate::cmdline::Cmdline) -> Result<()> {
    if matches!(cmdline.value("fsck"), Some("no") | Some("0")) {
        crate::info!("fsck: disabled by fsck=no");
        return Ok(());
    }
    let Some(program) = find_fsck() else {
        crate::info!("fsck: no fsck program in the initramfs; skipping");
        return Ok(());
    };

    let display = device.display();
    crate::info!("fsck: {program} -p {display}");
    let status = std::process::Command::new(program)
        .arg("-p")
        .arg(device)
        .status()
        .with_context(|| format!("run {program} -p {display}"))?;
    let code = status.code().unwrap_or(-1);
    crate::info!("fsck: exited with {code}");
    assess_fsck(code).with_context(|| format!("fsck {display}"))
}

fn find_fsck() -> Option<&'static str> {
    ["/sbin/fsck", "/bin/fsck", "/usr/sbin/fsck", "/usr/bin/fsck"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
}

/// Map fsck's documented exit status (fsck(8)) to success or failure.
fn assess_fsck(code: i32) -> Result<()> {
    match code {
        0 => Ok(()),
        1 => {
            crate::info!("fsck: filesystem errors corrected");
            Ok(())
        }
        2 => {
            crate::warn!("fsck: filesystem errors corrected, a reboot is recommended");
            Ok(())
        }
        4 => bail!("fsck: filesystem errors were left uncorrected"),
        8 => bail!("fsck: operational error"),
        16 => bail!("fsck: usage or syntax error"),
        32 => bail!("fsck: cancelled by user request"),
        other => bail!("fsck: unexpected exit status {other}"),
    }
}

/// Mount the non-root filesystems listed in the new root's `/etc/fstab`.
fn mount_fstab(root: &str) -> Result<()> {
    let path = Path::new(root).join("etc/fstab");
    let entries = crate::fstab::load(&path)?;
    if entries.is_empty() {
        crate::info!("fstab: no extra filesystems in {}", path.display());
        return Ok(());
    }

    for entry in entries {
        let target = format!("{root}{}", entry.mountpoint);
        if !Path::new(&target).exists() {
            crate::warn!(
                "fstab: mountpoint {target} does not exist; skipping {}",
                entry.device
            );
            continue;
        }

        let device = match resolve_source(&entry.device) {
            Ok(device) => device,
            Err(e) if entry.nofail() => {
                crate::warn!("fstab: {} (nofail): {e:#}", entry.device);
                continue;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("resolve fstab device {}", entry.device))
            }
        };

        let fstype = if entry.fstype == "auto" {
            None
        } else {
            Some(entry.fstype.as_str())
        };
        let (flags, data) = entry.mount_options();
        match mount_root(&device, &target, fstype, flags, data.as_deref()) {
            Ok(()) => crate::info!("fstab: mounted {} on {target}", device.display()),
            Err(e) if entry.nofail() => crate::warn!(
                "fstab: mounting {} on {target} failed ({e:#}); nofail",
                device.display()
            ),
            Err(e) => {
                return Err(e).with_context(|| format!("mount {} on {target}", device.display()))
            }
        }
    }

    Ok(())
}

/// Block devices are resolved like root=; other sources (tmpfs, none, ...)
/// are passed to mount(2) verbatim.
fn resolve_source(spec: &str) -> Result<PathBuf> {
    if spec.starts_with("UUID=") || spec.starts_with("PARTUUID=") || spec.starts_with('/') {
        crate::rootdev::resolve_with_retry(spec, 10, Duration::from_millis(100))
    } else {
        Ok(PathBuf::from(spec))
    }
}

fn mount_root(
    device: &Path,
    target: &str,
    fstype: Option<&str>,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<()> {
    let source = device
        .to_str()
        .with_context(|| format!("root device {} is not valid UTF-8", device.display()))?;

    match nix::mount::mount(Some(source), target, fstype, flags, data) {
        Ok(()) => Ok(()),
        Err(first_err) if fstype.is_none() => {
            crate::warn!("filesystem autodetection failed ({first_err}); probing known types");
            for candidate in ["ext4", "xfs", "btrfs", "vfat"] {
                if nix::mount::mount(Some(source), target, Some(candidate), flags, data).is_ok() {
                    crate::info!("mounted as {candidate}");
                    return Ok(());
                }
            }
            bail!("cannot mount {source} on {target}: {first_err}")
        }
        Err(e) => Err(e).with_context(|| format!("mount {source} on {target} as {fstype:?}")),
    }
}

fn exec_stage2(init_path: &str) -> Result<()> {
    let path = CString::new(init_path.as_bytes()).context("init path contains a NUL byte")?;
    let arg0 = path.clone();
    let stage2 = CString::new("--stage2").expect("literal has no NUL");
    let args = [arg0, stage2];

    let mut env: Vec<CString> = Vec::new();
    for (key, value) in std::env::vars_os() {
        let mut entry = key.as_os_str().as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_os_str().as_bytes());
        if let Ok(entry) = CString::new(entry) {
            env.push(entry);
        }
    }
    env.push(CString::new("JIMINIT_STAGE=2").expect("literal has no NUL"));

    crate::info!("exec {init_path} --stage2");
    let result = nix::unistd::execve(&path, &args, &env);
    match result {
        Ok(never) => match never {},
        Err(e) => Err(e).with_context(|| format!("exec {init_path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::assess_fsck;

    #[test]
    fn fsck_status_zero_one_two_are_successes() {
        assert!(assess_fsck(0).is_ok());
        assert!(assess_fsck(1).is_ok());
        assert!(assess_fsck(2).is_ok());
    }

    #[test]
    fn fsck_status_errors_refuse_the_mount() {
        for code in [4, 8, 16, 32, 64, -1] {
            assert!(assess_fsck(code).is_err(), "status {code} should fail");
        }
    }
}
