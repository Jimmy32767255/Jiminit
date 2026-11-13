//! Stage 2: PID 1 on the real root. Sets up the console, runs a root shell,
//! reaps orphans and performs an orderly shutdown or reboot.

use crate::config::Config;
use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::sys::signal::{kill, SigSet, Signal};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, setsid, ForkResult, Pid};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

pub fn run() -> Result<()> {
    crate::log::init();
    crate::mount::mount_pseudo_fs();

    let cfg = crate::config::load(Path::new(crate::config::DEFAULT_PATH));
    crate::info!("stage2: starting as pid {}", nix::unistd::getpid());

    set_hostname(&cfg.hostname);
    setup_console(&cfg.console);
    let signals = block_signals()?;

    let mut shell = spawn_shell(&cfg)?;

    loop {
        let signal = signals.set.wait().context("sigwaitinfo")?;
        match signal {
            Signal::SIGCHLD => {
                let exited = reap_all();
                for status in &exited {
                    crate::info!("reaped: {}", describe(status));
                }
                let shell_exited = exited.iter().any(|status| status.pid() == Some(shell));
                if shell_exited {
                    crate::warn!("root shell {shell} exited");
                    if cfg.respawn {
                        std::thread::sleep(Duration::from_millis(500));
                        match spawn_shell(&cfg) {
                            Ok(pid) => shell = pid,
                            Err(e) => crate::err!("cannot respawn the root shell: {e:#}"),
                        }
                    } else {
                        crate::info!("respawn is disabled; shutting down");
                        return shutdown(&cfg, Action::PowerOff);
                    }
                }
            }
            Signal::SIGINT => {
                crate::info!("SIGINT (ctrl-alt-del): rebooting");
                return shutdown(&cfg, Action::Reboot);
            }
            Signal::SIGTERM => {
                crate::info!("SIGTERM: powering off");
                return shutdown(&cfg, Action::PowerOff);
            }
            Signal::SIGQUIT | Signal::SIGHUP => {
                crate::info!("ignoring {signal}");
            }
            other => crate::info!("unexpected signal {other}"),
        }
    }
}

/// Fallback loop used when a stage fails: keep a shell alive on the console so
/// the machine is still usable and PID 1 never exits.
pub fn rescue() -> ! {
    let cfg = crate::config::load(Path::new(crate::config::DEFAULT_PATH));
    let shell = if Path::new(&cfg.shell).exists() {
        cfg.shell.clone()
    } else {
        "/bin/sh".to_string()
    };

    crate::err!("starting rescue shell ({shell})");

    setup_console(&cfg.console);
    loop {
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                let _ = waitpid(child, None);
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(ForkResult::Child) => child_shell(&cfg.console, &shell),
            Err(e) => {
                crate::warn!("fork failed: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn set_hostname(hostname: &str) {
    match nix::unistd::sethostname(hostname) {
        Ok(()) => crate::info!("hostname set to {hostname}"),
        Err(e) => crate::warn!("cannot set hostname to {hostname}: {e}"),
    }
}

/// Become a session leader and make the console our controlling terminal.
fn setup_console(path: &str) {
    // PID 1 may already be a session leader; EPERM is fine.
    let _ = setsid();
    attach_console(path);
}

/// Open `path` and make it our controlling terminal on fds 0..=2.
fn attach_console(path: &str) -> bool {
    match open(path, OFlag::O_RDWR, Mode::empty()) {
        Ok(fd) => {
            let raw = fd.as_raw_fd();
            let _ = unsafe { libc::ioctl(raw, libc::TIOCSCTTY as libc::c_ulong, 0) };
            for target in [0, 1, 2] {
                let _ = unsafe { libc::dup2(raw, target) };
            }
            if raw > 2 {
                // Dropping `fd` closes the original descriptor; 0..=2 are copies.
            } else {
                std::mem::forget(fd);
            }
            crate::info!("console is {path}");
            true
        }
        Err(e) => {
            crate::warn!("cannot open console {path}: {e}");
            false
        }
    }
}

struct Signals {
    set: SigSet,
}

fn block_signals() -> Result<Signals> {
    let mut set = SigSet::empty();
    for signal in [
        Signal::SIGCHLD,
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGQUIT,
        Signal::SIGHUP,
    ] {
        set.add(signal);
    }
    set.thread_block().context("block signals")?;
    Ok(Signals { set })
}

fn spawn_shell(cfg: &Config) -> Result<Pid> {
    let console = cfg.console.clone();
    let shell = cfg.shell.clone();
    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => {
            crate::info!("started {shell} (pid {child}) on {console}");
            Ok(child)
        }
        ForkResult::Child => child_shell(&console, &shell),
    }
}

/// Child side: give the shell a fresh session and the console as controlling
/// terminal, then exec it. Never returns.
fn child_shell(console: &str, shell: &str) -> ! {
    // The shell must not inherit the signals blocked for PID 1.
    let _ = SigSet::empty().thread_unblock();

    let _ = setsid();
    attach_console(console);

    let name = shell.rsplit('/').next().unwrap_or("sh");
    let argv0 = CString::new(format!("-{name}")).expect("argv0 has no NUL");
    let path = CString::new(shell).expect("shell path has no NUL");
    let env = shell_env(shell);
    let args = [argv0];

    let error = nix::unistd::execve(&path, &args, &env);
    eprintln!("jiminit: cannot exec {shell}: {error:?}");
    unsafe { libc::_exit(127) }
}

fn shell_env(shell: &str) -> Vec<CString> {
    let mut vars: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        vars.insert(
            key.as_os_str().as_bytes().to_vec(),
            value.as_os_str().as_bytes().to_vec(),
        );
    }

    let mut set = |key: &str, value: &str| {
        vars.insert(key.as_bytes().to_vec(), value.as_bytes().to_vec());
    };
    set("HOME", "/root");
    set("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    set("SHELL", shell);
    set("TERM", "linux");
    set("PS1", "jiminit:\\w# ");

    vars.into_iter()
        .map(|(mut key, value)| {
            key.push(b'=');
            key.extend_from_slice(&value);
            CString::new(key).expect("environment has no NUL")
        })
        .collect()
}

fn reap_all() -> Vec<WaitStatus> {
    let mut reaped = Vec::new();
    loop {
        match waitpid(None::<Pid>, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => break,
            Ok(status) => reaped.push(status),
            Err(Errno::ECHILD) => break,
            Err(e) => {
                crate::warn!("waitpid failed: {e}");
                break;
            }
        }
    }
    reaped
}

fn describe(status: &WaitStatus) -> String {
    match status {
        WaitStatus::Exited(pid, code) => format!("pid {pid} exited with status {code}"),
        WaitStatus::Signaled(pid, signal, core) => format!(
            "pid {pid} killed by {signal:?}{}",
            if *core { " (core dumped)" } else { "" }
        ),
        other => format!("{other:?}"),
    }
}

#[derive(Debug, Clone, Copy)]
enum Action {
    PowerOff,
    Reboot,
}

fn shutdown(cfg: &Config, mode: Action) -> Result<()> {
    crate::info!("shutting down ({mode:?}): sending SIGTERM to all processes");
    let _ = kill(Pid::from_raw(-1), Signal::SIGTERM);

    let deadline = Instant::now() + Duration::from_millis(cfg.shutdown_timeout_ms);
    loop {
        match waitpid(None::<Pid>, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {
                if Instant::now() >= deadline {
                    crate::warn!("timed out waiting for processes; sending SIGKILL");
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(status) => crate::info!("reaped: {}", describe(&status)),
            Err(Errno::ECHILD) => break,
            Err(e) => {
                crate::warn!("waitpid failed: {e}");
                break;
            }
        }
    }

    let _ = kill(Pid::from_raw(-1), Signal::SIGKILL);
    std::thread::sleep(Duration::from_millis(100));
    let _ = reap_all();

    crate::info!("flushing filesystems");
    nix::unistd::sync();
    let _ = nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        nix::mount::MsFlags::MS_REMOUNT | nix::mount::MsFlags::MS_RDONLY,
        None::<&str>,
    );

    let how = match mode {
        Action::PowerOff => nix::sys::reboot::RebootMode::RB_POWER_OFF,
        Action::Reboot => nix::sys::reboot::RebootMode::RB_AUTOBOOT,
    };
    crate::info!("requesting {how:?}");
    match nix::sys::reboot::reboot(how) {
        Ok(never) => match never {},
        Err(e) => crate::warn!("reboot syscall failed: {e}"),
    }

    crate::warn!("system did not power off; idling");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
