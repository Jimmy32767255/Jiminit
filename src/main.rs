//! jiminit — a small experimental Rust init.
//!
//! Two modes:
//! * initramfs stage 1 (`root=` on the kernel command line): mount the real
//!   root and switch_root into it.
//! * root stage 2 (PID 1 on the final root): hostname, console, root shell,
//!   reaping and shutdown.
//!
//! PID 1 must never exit, so every failure ends up in a rescue shell loop.

mod cmdline;
mod config;
mod log;
mod mount;
mod rootdev;
mod stage1;
mod stage2;

use std::ffi::CString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Running from the initramfs, needs to mount the real root.
    Initramfs,
    /// Running on the real root as the long-lived PID 1.
    Root,
}

fn main() {
    log::init();
    mount::mount_pseudo_fs();

    let stage = detect_stage();
    crate::info!("jiminit starting ({stage:?})");

    let result = std::panic::catch_unwind(|| match stage {
        Stage::Initramfs => stage1::run(),
        Stage::Root => stage2::run(),
    });

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => crate::err!("{stage:?} failed: {e:#}"),
        Err(_) => crate::err!("{stage:?} panicked"),
    }

    stage2::rescue();
}

fn detect_stage() -> Stage {
    if let Some(stage) = forced_stage() {
        return stage;
    }

    let initramfs = is_initramfs_root();
    match std::fs::read_to_string("/proc/cmdline") {
        Ok(raw) => {
            let cmdline = cmdline::Cmdline::parse(&raw);
            if initramfs && cmdline.value("root").is_some() {
                Stage::Initramfs
            } else {
                Stage::Root
            }
        }
        Err(e) => {
            crate::warn!("cannot read /proc/cmdline ({e}); guessing stage from the root fs");
            if initramfs {
                Stage::Initramfs
            } else {
                Stage::Root
            }
        }
    }
}

/// `--stage1` / `--stage2` arguments or `JIMINIT_STAGE=1|2`.
fn forced_stage() -> Option<Stage> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--stage1") {
        return Some(Stage::Initramfs);
    }
    if args.iter().any(|arg| arg == "--stage2") {
        return Some(Stage::Root);
    }
    match std::env::var("JIMINIT_STAGE").as_deref() {
        Ok("1") => Some(Stage::Initramfs),
        Ok("2") => Some(Stage::Root),
        _ => None,
    }
}

/// Is `/` a ramfs/tmpfs, i.e. are we still inside the initramfs?
fn is_initramfs_root() -> bool {
    const RAMFS_MAGIC: libc::c_long = 0x8584_58f6;
    const TMPFS_MAGIC: libc::c_long = 0x0102_1994;

    let Ok(path) = CString::new("/") else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return false;
    }
    stat.f_type == RAMFS_MAGIC || stat.f_type == TMPFS_MAGIC
}
