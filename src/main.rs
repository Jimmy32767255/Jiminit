//! jiminit — a small experimental Rust init.
//!
//! Two modes:
//! * initramfs stage 1 (`root=` on the kernel command line): mount the real
//!   root and switch_root into it.
//! * root stage 2 (PID 1 on the final root): hostname, console, getty/login,
//!   service supervision, runlevel switching, reaping and shutdown.
//! * a small client (`jiminit --status` / `--runlevel NAME`) that talks to the
//!   running init over /run/jiminit.sock.
//!
//! PID 1 must never exit, so every failure ends up in a rescue shell loop.

mod cmdline;
mod coldplug;
mod config;
mod crypttab;
mod fstab;
mod log;
mod luks;
mod modules;
mod mount;
mod rootdev;
mod runlevel;
mod service;
mod stage1;
mod stage2;

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Running from the initramfs, needs to mount the real root.
    Initramfs,
    /// Running on the real root as the long-lived PID 1.
    Root,
}

fn main() {
    if let Some(code) = client() {
        std::process::exit(code);
    }

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

/// Handle the one-shot client commands. Returns `None` for the init itself.
fn client() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let request = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--status"] => "status".to_string(),
        ["--runlevel", level] => format!("runlevel {level}"),
        _ => return None,
    };
    Some(run_client(&request))
}

fn run_client(request: &str) -> i32 {
    let path = stage2::CONTROL_PATH;
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("jiminit: cannot connect to {path}: {e} (is jiminit running as pid 1?)");
            return 1;
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));

    if let Err(e) = stream.write_all(request.as_bytes()) {
        eprintln!("jiminit: cannot send to {path}: {e}");
        return 1;
    }

    let mut reply = String::new();
    if let Err(e) = stream.read_to_string(&mut reply) {
        eprintln!("jiminit: no reply from init: {e}");
        return 1;
    }
    // The session may be hung up while the reply is on the way (a runlevel
    // switch stops the session that asked for it), so ignore write errors.
    let _ = std::io::stdout().write_all(reply.as_bytes());
    if reply.starts_with("error") {
        1
    } else {
        0
    }
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
