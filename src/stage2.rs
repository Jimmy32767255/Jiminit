//! Stage 2: PID 1 on the real root. Sets up the console, supervises services,
//! runs a root shell, reaps orphans and performs an orderly shutdown/reboot.

use crate::config::Config;
use crate::service::Supervisor;
use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::signal::{kill, SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, setsid, ForkResult, Pid};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// How often the main loop wakes up even without a signal or service output,
/// so pending restarts and shutdown deadlines are processed on time.
const LOOP_TIMEOUT_MS: u16 = 100;

pub fn run() -> Result<()> {
    crate::log::init();
    crate::mount::mount_pseudo_fs();

    let cfg = crate::config::load(Path::new(crate::config::DEFAULT_PATH));
    crate::info!("stage2: starting as pid {}", nix::unistd::getpid());
    if let Ok(n) = std::env::var("JIMINIT_SOFT_REBOOTS") {
        crate::info!("soft reboot #{n}: running stage2 again");
    }

    set_hostname(&cfg.hostname);
    setup_console(&cfg.console);
    let signals = block_signals()?;
    let signal_fd = SignalFd::with_flags(
        &signals.set,
        SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC,
    )
    .context("signalfd")?;

    let mut supervisor = Supervisor::new(Path::new(&cfg.services_dir));
    supervisor.start_all();

    let mut shell = spawn_shell(&cfg)?;

    loop {
        // Wake on a signal, on service output, or after the timeout.
        {
            let mut pollfds = Vec::with_capacity(1 + supervisor.log_count());
            pollfds.push(PollFd::new(signal_fd.as_fd(), PollFlags::POLLIN));
            for fd in supervisor.log_fds() {
                pollfds.push(PollFd::new(fd, PollFlags::POLLIN));
            }
            let _ = poll(&mut pollfds, LOOP_TIMEOUT_MS);
        }
        supervisor.drain_logs();

        while let Some(info) = signal_fd.read_signal().context("read signalfd")? {
            match info.ssi_signo as i32 {
                libc::SIGINT => {
                    crate::info!("SIGINT (ctrl-alt-del): rebooting");
                    return shutdown(&cfg, &mut supervisor, shell, Action::Reboot);
                }
                libc::SIGTERM => {
                    crate::info!("SIGTERM: powering off");
                    return shutdown(&cfg, &mut supervisor, shell, Action::PowerOff);
                }
                libc::SIGUSR1 => {
                    crate::info!("SIGUSR1: soft reboot");
                    return shutdown(&cfg, &mut supervisor, shell, Action::SoftReboot);
                }
                libc::SIGCHLD => {}
                libc::SIGQUIT | libc::SIGHUP => {
                    crate::info!("ignoring signal {}", info.ssi_signo)
                }
                other => crate::info!("unexpected signal {other}"),
            }
        }

        // Reap on every wakeup rather than only on SIGCHLD, so a coalesced
        // signal can never leave zombies behind.
        let (exited, _) = reap_all();
        let mut shell_exited = false;
        for status in &exited {
            match status.pid() {
                Some(pid) if pid == shell => shell_exited = true,
                Some(pid) => {
                    if !supervisor.handle_exit(pid, status) {
                        crate::info!("reaped: {}", describe(status));
                    }
                }
                None => crate::info!("reaped: {}", describe(status)),
            }
        }

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
                return shutdown(&cfg, &mut supervisor, shell, Action::PowerOff);
            }
        }

        supervisor.tick();
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
        Signal::SIGUSR1,
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
    clear_signal_mask();

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

/// Clear the inherited signal mask (`SIG_SETMASK` with an empty set).
fn clear_signal_mask() {
    let empty = SigSet::empty();
    let _ = nix::sys::signal::sigprocmask(
        nix::sys::signal::SigmaskHow::SIG_SETMASK,
        Some(&empty),
        None,
    );
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
    set(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
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

/// Reap all exited children. The boolean is true when there are no children
/// left at all (ECHILD).
fn reap_all() -> (Vec<WaitStatus>, bool) {
    let mut reaped = Vec::new();
    loop {
        match waitpid(None::<Pid>, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return (reaped, false),
            Ok(status) => reaped.push(status),
            Err(Errno::ECHILD) => return (reaped, true),
            Err(e) => {
                crate::warn!("waitpid failed: {e}");
                return (reaped, false);
            }
        }
    }
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
    /// Stop everything, then exec a fresh stage 2 in this same PID instead of
    /// asking the kernel to reboot.
    SoftReboot,
}

fn shutdown(
    cfg: &Config,
    supervisor: &mut Supervisor,
    shell: Pid,
    mode: Action,
) -> Result<()> {
    crate::info!(
        "shutting down ({mode:?}): stopping shell and {} service(s)",
        supervisor.running_count()
    );
    // Interactive shells ignore SIGTERM, so hang up the session instead; the
    // rest of the system gets a graceful SIGTERM.
    let _ = kill(shell, Signal::SIGHUP);
    supervisor.stop();

    // Phase 1: give everything shutdown_timeout_ms to exit after SIGTERM.
    // Phase 2: SIGKILL whatever is left, then wait a little longer.
    let deadline = Instant::now() + Duration::from_millis(cfg.shutdown_timeout_ms);
    let mut hard_deadline: Option<Instant> = None;
    loop {
        let (exited, all_gone) = reap_all();
        for status in &exited {
            match status.pid() {
                Some(pid) if !supervisor.handle_exit(pid, status) => {
                    crate::info!("reaped: {}", describe(status))
                }
                None => crate::info!("reaped: {}", describe(status)),
                Some(_) => {}
            }
        }
        supervisor.drain_logs();

        if all_gone {
            break;
        }

        let now = Instant::now();
        match hard_deadline {
            None if now >= deadline => {
                crate::warn!("timed out waiting for processes; sending SIGKILL");
                supervisor.kill_all();
                let _ = kill(Pid::from_raw(-1), Signal::SIGKILL);
                hard_deadline = Some(now + Duration::from_secs(2));
            }
            Some(hard) if now >= hard => {
                crate::warn!("processes survived SIGKILL; continuing anyway");
                break;
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    crate::info!("flushing filesystems");
    nix::unistd::sync();

    if let Action::SoftReboot = mode {
        return soft_reboot_exec(cfg);
    }

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
        Action::SoftReboot => unreachable!("handled before the reboot syscall"),
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

/// Clear the transient directories and exec a fresh stage 2 in this same PID,
/// so the whole boot flow (config, services, shell) runs again without asking
/// the kernel to reboot.
fn soft_reboot_exec(cfg: &Config) -> Result<()> {
    for dir in ["/run", "/tmp"] {
        reset_tmpfs(dir);
    }

    let count = next_soft_reboot_count();
    let exe = std::fs::read_link("/proc/self/exe")
        .unwrap_or_else(|_| std::path::PathBuf::from(&cfg.init_path));
    crate::info!("soft reboot #{count}: exec {} --stage2", exe.display());

    match exec_self(&exe, count) {
        Ok(never) => match never {},
        Err(e) => {
            crate::err!("soft reboot failed: {e:#}; falling back to the rescue shell");
            rescue();
        }
    }
}

fn exec_self(exe: &Path, count: u32) -> Result<std::convert::Infallible> {
    let path = CString::new(exe.as_os_str().as_bytes()).context("binary path contains a NUL byte")?;
    let argv = [
        CString::new("jiminit").expect("no NUL"),
        CString::new("--stage2").expect("no NUL"),
    ];
    let env = soft_reboot_env(count);
    match nix::unistd::execve(&path, &argv, &env) {
        Ok(never) => Ok(never),
        Err(e) => Err(anyhow::anyhow!("exec {}: {e}", exe.display())),
    }
}

fn next_soft_reboot_count() -> u32 {
    parse_soft_reboot_count(std::env::var("JIMINIT_SOFT_REBOOTS").ok().as_deref())
}

fn parse_soft_reboot_count(value: Option<&str>) -> u32 {
    value.and_then(|value| value.parse().ok()).unwrap_or(0) + 1
}

fn soft_reboot_env(count: u32) -> Vec<CString> {
    let mut vars: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        vars.insert(
            key.as_os_str().as_bytes().to_vec(),
            value.as_os_str().as_bytes().to_vec(),
        );
    }
    vars.insert(b"JIMINIT_STAGE".to_vec(), b"2".to_vec());
    vars.insert(
        b"JIMINIT_SOFT_REBOOTS".to_vec(),
        count.to_string().into_bytes(),
    );

    vars.into_iter()
        .map(|(mut key, value)| {
            key.push(b'=');
            key.extend_from_slice(&value);
            CString::new(key).expect("environment has no NUL")
        })
        .collect()
}

/// Replace `dir` with a fresh tmpfs, detaching whatever was mounted there.
fn reset_tmpfs(dir: &str) {
    use nix::mount::{mount, umount2, MntFlags, MsFlags};

    let _ = umount2(dir, MntFlags::MNT_DETACH);
    if !Path::new(dir).exists() {
        let _ = std::fs::create_dir_all(dir);
    }
    match mount(
        Some("tmpfs"),
        dir,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=1777"),
    ) {
        Ok(()) => crate::info!("soft reboot: fresh tmpfs on {dir}"),
        Err(e) => crate::warn!("soft reboot: cannot mount tmpfs on {dir}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_soft_reboot_count;

    #[test]
    fn soft_reboot_count_defaults_to_one() {
        assert_eq!(parse_soft_reboot_count(None), 1);
        assert_eq!(parse_soft_reboot_count(Some("garbage")), 1);
        assert_eq!(parse_soft_reboot_count(Some("2")), 3);
    }
}
