//! Stage 1: runs from the initramfs, mounts the real root and switches to it.

use anyhow::{bail, Context, Result};
use nix::mount::MsFlags;
use nix::unistd::{chdir, chroot};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub fn run() -> Result<()> {
    crate::log::init();
    crate::info!("stage1: running from initramfs");

    crate::mount::mount_pseudo_fs();

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
    let device = crate::rootdev::resolve_with_retry(
        root_spec,
        20,
        std::time::Duration::from_millis(250),
    )?;
    crate::info!("root device resolved to {}", device.display());

    std::fs::create_dir_all("/newroot").context("create /newroot")?;

    let mut flags = MsFlags::empty();
    if read_only {
        flags |= MsFlags::MS_RDONLY;
    }
    mount_root(&device, "/newroot", fstype, flags, data)?;
    crate::info!("mounted {} on /newroot", device.display());

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
