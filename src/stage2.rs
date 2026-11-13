//! Stage 2: PID 1 on the real root. Sets up the console, supervises services,
//! starts getty on the configured terminals (or one root shell in the single
//! runlevel), reaps orphans, switches runlevels on request and performs an
//! orderly shutdown/reboot/soft-reboot.

use crate::config::Config;
use crate::runlevel::{Level, Target};
use crate::service::Supervisor;
use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::signal::{kill, sigprocmask, SigSet, SigmaskHow, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, setsid, ForkResult, Pid};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// How often the main loop wakes up even without a signal or service output,
/// so pending restarts and shutdown deadlines are processed on time.
const LOOP_TIMEOUT_MS: u16 = 100;

/// Where `jiminit --status` / `jiminit --runlevel` find the control socket.
pub const CONTROL_PATH: &str = "/run/jiminit.sock";

/// Delay before respawning a getty after the previous one exited.
const GETTY_RESPAWN_DELAY: Duration = Duration::from_millis(500);

struct Getty {
    pid: Pid,
    tty: String,
    serial: bool,
}

pub fn run() -> Result<()> {
    crate::log::init();
    crate::mount::mount_pseudo_fs();

    let cfg = crate::config::load(Path::new(crate::config::DEFAULT_PATH));
    crate::info!("stage2: starting as pid {}", nix::unistd::getpid());
    if let Ok(n) = std::env::var("JIMINIT_SOFT_REBOOTS") {
        crate::info!("soft reboot #{n}: running stage2 again");
    }

    let mut level = parse_default_level(&cfg);
    set_hostname(&cfg.hostname);
    setup_console(&cfg.console);
    let signals = block_signals()?;
    let signal_fd =
        SignalFd::with_flags(&signals.set, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC)
            .context("signalfd")?;
    let listener = control_listener();

    let mut supervisor = Supervisor::new(Path::new(&cfg.services_dir), level);
    supervisor.start_members();

    let mut getties: Vec<Getty> = Vec::new();
    let mut single_shell: Option<Pid> = None;
    if level == Level::Single {
        single_shell = Some(spawn_shell(&cfg)?);
    } else {
        start_getties(&cfg, &mut getties);
    }

    loop {
        // Wake on a signal, on service output, on a control request, or after
        // the timeout.
        {
            let mut pollfds = Vec::with_capacity(2 + supervisor.log_count());
            pollfds.push(PollFd::new(signal_fd.as_fd(), PollFlags::POLLIN));
            if let Some(listener) = &listener {
                pollfds.push(PollFd::new(listener.as_fd(), PollFlags::POLLIN));
            }
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
                    return shutdown(
                        &cfg,
                        &mut supervisor,
                        Action::Reboot,
                        &mut getties,
                        single_shell,
                    );
                }
                libc::SIGTERM => {
                    crate::info!("SIGTERM: powering off");
                    return shutdown(
                        &cfg,
                        &mut supervisor,
                        Action::PowerOff,
                        &mut getties,
                        single_shell,
                    );
                }
                libc::SIGUSR1 => {
                    crate::info!("SIGUSR1: soft reboot");
                    return shutdown(
                        &cfg,
                        &mut supervisor,
                        Action::SoftReboot,
                        &mut getties,
                        single_shell,
                    );
                }
                libc::SIGCHLD => {}
                libc::SIGQUIT | libc::SIGHUP => {
                    crate::info!("ignoring signal {}", info.ssi_signo)
                }
                other => crate::info!("unexpected signal {other}"),
            }
        }

        if let Some(listener) = &listener {
            if let Some(action) = handle_control(
                &cfg,
                listener,
                &mut level,
                &mut supervisor,
                &mut getties,
                &mut single_shell,
            ) {
                return shutdown(&cfg, &mut supervisor, action, &mut getties, single_shell);
            }
        }

        // Reap on every wakeup rather than only on SIGCHLD, so a coalesced
        // signal can never leave zombies behind.
        let (exited, _) = reap_all();
        for status in &exited {
            let Some(pid) = status.pid() else {
                crate::info!("reaped: {}", describe(status));
                continue;
            };

            if supervisor.handle_exit(pid, status) {
                continue;
            }

            if let Some(position) = getties.iter().position(|getty| getty.pid == pid) {
                let getty = getties.remove(position);
                crate::warn!(
                    "getty on {} (pid {pid}) exited: {}",
                    getty.tty,
                    describe(status)
                );
                if level != Level::Single {
                    std::thread::sleep(GETTY_RESPAWN_DELAY);
                    if level != Level::Single {
                        spawn_getty_for(&cfg, &getty.tty, getty.serial, &mut getties);
                    }
                }
                continue;
            }

            if Some(pid) == single_shell {
                crate::warn!("root shell {pid} exited");
                single_shell = None;
                if level != Level::Single {
                    // Killed by a runlevel switch; nothing to respawn.
                } else if cfg.respawn {
                    std::thread::sleep(Duration::from_millis(500));
                    match spawn_shell(&cfg) {
                        Ok(pid) => single_shell = Some(pid),
                        Err(e) => crate::err!("cannot respawn the root shell: {e:#}"),
                    }
                } else {
                    crate::info!("respawn is disabled; shutting down");
                    return shutdown(
                        &cfg,
                        &mut supervisor,
                        Action::PowerOff,
                        &mut getties,
                        single_shell,
                    );
                }
                continue;
            }

            crate::info!("reaped: {}", describe(status));
        }

        supervisor.tick();
    }
}

fn parse_default_level(cfg: &Config) -> Level {
    match Level::parse(&cfg.default_runlevel) {
        Some(level) => level,
        None => {
            crate::warn!(
                "unknown default_runlevel `{}`; using {}",
                cfg.default_runlevel,
                Level::DEFAULT
            );
            Level::DEFAULT
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

fn open_console(path: &str) -> Option<std::os::fd::OwnedFd> {
    match open(path, OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty()) {
        Ok(fd) => Some(fd),
        Err(e) => {
            crate::warn!("cannot open console {path}: {e}");
            None
        }
    }
}

/// Put the console on fds 0..=2 for PID 1, without claiming it as controlling
/// terminal: getty/login sessions claim their own terminal.
fn setup_console(path: &str) {
    let Some(fd) = open_console(path) else {
        return;
    };
    let raw = fd.as_raw_fd();
    for target in [0, 1, 2] {
        let _ = unsafe { libc::dup2(raw, target) };
    }
    if raw > 2 {
        // Dropping `fd` closes the original descriptor; 0..=2 are copies.
        drop(fd);
    } else {
        std::mem::forget(fd);
    }
    crate::info!("console is {path}");
}

/// Open `path` and make it our controlling terminal on fds 0..=2.
fn attach_console(path: &str) -> bool {
    let Some(fd) = open_console(path) else {
        return false;
    };
    let raw = fd.as_raw_fd();
    let _ = unsafe { libc::ioctl(raw, libc::TIOCSCTTY as libc::c_ulong, 0) };
    for target in [0, 1, 2] {
        let _ = unsafe { libc::dup2(raw, target) };
    }
    if raw > 2 {
        drop(fd);
    } else {
        std::mem::forget(fd);
    }
    true
}

/// Redirect fds 0..=2 to /dev/null (used for getty, which opens its own tty).
fn detach_stdio() {
    let Ok(fd) = open("/dev/null", OFlag::O_RDWR, Mode::empty()) else {
        return;
    };
    let raw = fd.as_raw_fd();
    for target in [0, 1, 2] {
        let _ = unsafe { libc::dup2(raw, target) };
    }
    if raw > 2 {
        drop(fd);
    } else {
        std::mem::forget(fd);
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

/// A getty process: one per configured terminal.
fn start_getties(cfg: &Config, getties: &mut Vec<Getty>) {
    let mut targets: Vec<(String, bool)> =
        cfg.ttys.iter().map(|tty| (tty.clone(), false)).collect();
    targets.extend(cfg.serial_ttys.iter().map(|tty| (tty.clone(), true)));

    for (tty, serial) in targets {
        if getties.iter().any(|getty| getty.tty == tty) {
            continue;
        }
        if !Path::new(&tty).exists() {
            crate::info!("getty: {tty} does not exist; skipping");
            continue;
        }
        spawn_getty_for(cfg, &tty, serial, getties);
    }
}

fn spawn_getty_for(cfg: &Config, tty: &str, serial: bool, getties: &mut Vec<Getty>) {
    match spawn_getty(cfg, tty, serial) {
        Ok(pid) => {
            crate::info!("started getty on {tty} (pid {pid})");
            getties.push(Getty {
                pid,
                tty: tty.to_string(),
                serial,
            });
        }
        Err(e) => crate::err!("cannot start getty on {tty}: {e:#}"),
    }
}

fn spawn_getty(cfg: &Config, tty: &str, serial: bool) -> Result<Pid> {
    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => Ok(child),
        ForkResult::Child => child_getty(cfg, tty, serial),
    }
}

fn child_getty(cfg: &Config, tty: &str, serial: bool) -> ! {
    clear_signal_mask();
    let _ = setsid();
    detach_stdio();

    let baud = if serial {
        cfg.serial_getty_baud.as_str()
    } else {
        cfg.getty_baud.as_str()
    };

    let mut strings = Vec::with_capacity(4);
    for value in ["getty", "-L", baud, tty] {
        match CString::new(value) {
            Ok(value) => strings.push(value),
            Err(_) => {
                crate::err!("getty argument contains a NUL byte");
                unsafe { libc::_exit(127) }
            }
        }
    }
    let path = match CString::new(cfg.getty.as_str()) {
        Ok(path) => path,
        Err(_) => {
            crate::err!("getty path contains a NUL byte");
            unsafe { libc::_exit(127) }
        }
    };
    let env = inherited_env();

    let error = nix::unistd::execve(&path, &strings, &env);
    crate::err!("cannot exec {} on {tty}: {error:?}", cfg.getty);
    unsafe { libc::_exit(127) }
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
    crate::err!("cannot exec {shell}: {error:?}");
    unsafe { libc::_exit(127) }
}

/// Clear the inherited signal mask (`SIG_SETMASK` with an empty set).
fn clear_signal_mask() {
    let empty = SigSet::empty();
    let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&empty), None);
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

fn inherited_env() -> Vec<CString> {
    let mut env = Vec::new();
    for (key, value) in std::env::vars_os() {
        let mut entry = key.as_os_str().as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_os_str().as_bytes());
        if let Ok(entry) = CString::new(entry) {
            env.push(entry);
        }
    }
    env
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

// --- control socket ------------------------------------------------------

fn control_listener() -> Option<UnixListener> {
    if let Some(parent) = Path::new(CONTROL_PATH).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(CONTROL_PATH);

    match UnixListener::bind(CONTROL_PATH) {
        Ok(listener) => match listener.set_nonblocking(true) {
            Ok(()) => {
                crate::info!("control socket listening on {CONTROL_PATH}");
                Some(listener)
            }
            Err(e) => {
                crate::warn!("cannot make {CONTROL_PATH} nonblocking: {e}");
                None
            }
        },
        Err(e) => {
            crate::warn!("cannot create {CONTROL_PATH}: {e}");
            None
        }
    }
}

fn handle_control(
    cfg: &Config,
    listener: &UnixListener,
    level: &mut Level,
    supervisor: &mut Supervisor,
    getties: &mut Vec<Getty>,
    single_shell: &mut Option<Pid>,
) -> Option<Action> {
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return None,
            Err(e) => {
                crate::warn!("accept on {CONTROL_PATH}: {e}");
                return None;
            }
        };
        if let Some(action) = handle_client(cfg, stream, level, supervisor, getties, single_shell) {
            return Some(action);
        }
    }
}

fn handle_client(
    cfg: &Config,
    mut stream: UnixStream,
    level: &mut Level,
    supervisor: &mut Supervisor,
    getties: &mut Vec<Getty>,
    single_shell: &mut Option<Pid>,
) -> Option<Action> {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let mut buf = [0u8; 256];
    let read = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..read]);
    let command = request.trim();

    let (reply, action) = if command == "status" {
        (format!("status {}\n", level.name()), None)
    } else if let Some(name) = command.strip_prefix("runlevel ") {
        runlevel_request(cfg, name.trim(), level, supervisor, getties, single_shell)
    } else {
        (
            "error unknown request (try `status` or `runlevel <name>`)\n".to_string(),
            None,
        )
    };

    if let Err(e) = stream.write_all(reply.as_bytes()) {
        crate::warn!("cannot reply on {CONTROL_PATH}: {e}");
    }
    action
}

fn runlevel_request(
    cfg: &Config,
    name: &str,
    level: &mut Level,
    supervisor: &mut Supervisor,
    getties: &mut Vec<Getty>,
    single_shell: &mut Option<Pid>,
) -> (String, Option<Action>) {
    match Target::parse(name) {
        Some(Target::Level(target)) => {
            if target == *level {
                return (format!("ok runlevel {target}\n"), None);
            }
            switch_level(cfg, target, level, supervisor, getties, single_shell);
            (format!("ok runlevel {target}\n"), None)
        }
        Some(Target::PowerOff) => ("ok poweroff\n".to_string(), Some(Action::PowerOff)),
        Some(Target::Reboot) => ("ok reboot\n".to_string(), Some(Action::Reboot)),
        None => (
            format!("error unknown runlevel `{name}`; use single, multi-user or graphical\n"),
            None,
        ),
    }
}

/// Move between runlevels: stop the sessions and services that do not belong
/// to the target, wait for them to leave, then start the target's sessions and
/// services.
fn switch_level(
    cfg: &Config,
    target: Level,
    level: &mut Level,
    supervisor: &mut Supervisor,
    getties: &mut Vec<Getty>,
    single_shell: &mut Option<Pid>,
) {
    if target == *level {
        return;
    }
    crate::info!("switching runlevel {} -> {target}", *level);

    // Stop the current login session first: the new shell or getty must be able
    // to claim the console once the old controlling terminal is gone.
    let mut session_pids: Vec<Pid> = if target == Level::Single {
        stop_getties(getties)
    } else {
        stop_single_shell(single_shell).into_iter().collect()
    };

    supervisor.set_level(target);
    session_pids.extend(supervisor.stopping_pids());
    wait_for(supervisor, &session_pids, Duration::from_secs(3));

    supervisor.start_members();

    if target == Level::Single {
        if single_shell.is_none() {
            match spawn_shell(cfg) {
                Ok(pid) => *single_shell = Some(pid),
                Err(e) => crate::err!("cannot start the single-user shell: {e:#}"),
            }
        }
    } else {
        start_getties(cfg, getties);
    }

    *level = target;
    crate::info!("now in runlevel {target}");
}

fn stop_getties(getties: &mut Vec<Getty>) -> Vec<Pid> {
    let mut pids = Vec::new();
    for getty in getties.drain(..) {
        crate::info!("getty on {}: stopping (SIGHUP)", getty.tty);
        let _ = kill(getty.pid, Signal::SIGHUP);
        pids.push(getty.pid);
    }
    pids
}

fn stop_single_shell(single_shell: &mut Option<Pid>) -> Option<Pid> {
    let pid = single_shell.take()?;
    crate::info!("single-user shell {pid}: stopping (SIGHUP)");
    let _ = kill(pid, Signal::SIGHUP);
    Some(pid)
}

fn pid_alive(pid: Pid) -> bool {
    let rc = unsafe { libc::kill(pid.as_raw(), 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Wait until every process in `pids` is gone (services are reaped through the
/// supervisor, sessions through the kernel's liveness check), killing the
/// stragglers after `timeout`.
fn wait_for(supervisor: &mut Supervisor, pids: &[Pid], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        reap_for(supervisor);

        let services_left = supervisor.any_running(pids);
        let sessions_left = pids
            .iter()
            .any(|pid| !supervisor.any_running(&[*pid]) && pid_alive(*pid));
        if !services_left && !sessions_left {
            return;
        }

        if Instant::now() >= deadline {
            crate::warn!("processes did not stop for the runlevel change; sending SIGKILL");
            for pid in pids {
                // Login sessions are process groups led by getty/shell.
                let _ = kill(Pid::from_raw(-pid.as_raw()), Signal::SIGKILL);
            }
            let hard = Instant::now() + Duration::from_secs(1);
            while Instant::now() < hard {
                reap_for(supervisor);
                let services_left = supervisor.any_running(pids);
                let sessions_left = pids
                    .iter()
                    .any(|pid| !supervisor.any_running(&[*pid]) && pid_alive(*pid));
                if !services_left && !sessions_left {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        }

        std::thread::sleep(Duration::from_millis(30));
    }
}

fn reap_for(supervisor: &mut Supervisor) {
    let (exited, _) = reap_all();
    for status in &exited {
        match status.pid() {
            Some(pid) if supervisor.handle_exit(pid, status) => {}
            _ => crate::info!("reaped: {}", describe(status)),
        }
    }
    supervisor.drain_logs();
}

// --- shutdown ------------------------------------------------------------

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
    mode: Action,
    getties: &mut Vec<Getty>,
    single_shell: Option<Pid>,
) -> Result<()> {
    crate::info!(
        "shutting down ({mode:?}): stopping {} session(s) and {} service(s)",
        getties.len() + usize::from(single_shell.is_some()),
        supervisor.running_count()
    );
    // Interactive shells ignore SIGTERM, so hang up the sessions instead; the
    // services get a graceful SIGTERM.
    stop_getties(getties);
    if let Some(pid) = single_shell {
        let _ = kill(pid, Signal::SIGHUP);
    }
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

fn reset_tmpfs(dir: &str) {
    let _ = nix::mount::umount2(dir, nix::mount::MntFlags::MNT_DETACH);
    match nix::mount::mount(
        Some("tmpfs"),
        dir,
        Some("tmpfs"),
        nix::mount::MsFlags::MS_NOSUID | nix::mount::MsFlags::MS_NODEV,
        Some("mode=1777"),
    ) {
        Ok(()) => crate::info!("soft reboot: fresh tmpfs on {dir}"),
        Err(e) => crate::warn!("cannot mount a fresh tmpfs on {dir}: {e}"),
    }
}

fn exec_self(exe: &Path, count: u32) -> Result<std::convert::Infallible> {
    let path =
        CString::new(exe.as_os_str().as_bytes()).context("binary path contains a NUL byte")?;
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

#[cfg(test)]
mod tests {
    use super::parse_soft_reboot_count;

    #[test]
    fn soft_reboot_count_defaults_to_one() {
        assert_eq!(parse_soft_reboot_count(None), 1);
        assert_eq!(parse_soft_reboot_count(Some("not-a-number")), 1);
    }

    #[test]
    fn soft_reboot_count_increments() {
        assert_eq!(parse_soft_reboot_count(Some("2")), 3);
    }
}
