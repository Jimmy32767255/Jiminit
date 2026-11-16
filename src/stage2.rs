//! Stage 2: PID 1 on the real root. Sets up the console, supervises services,
//! starts getty on the configured terminals (or one root shell in the single
//! runlevel), reaps orphans, switches runlevels on request and performs an
//! orderly shutdown/reboot/soft-reboot.

use crate::cmdline::Cmdline;
use crate::config::Config;
use crate::runlevel::Level;
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

/// Where the client sub-commands find the control socket.
pub const CONTROL_PATH: &str = "/run/jiminit.sock";

/// Delay before respawning a crashed getty; doubles up to the cap. Sessions
/// that stayed up longer than the stable window start over from the base.
const GETTY_RESPAWN_BASE: Duration = Duration::from_millis(500);
const GETTY_RESPAWN_MAX: Duration = Duration::from_secs(30);
const SESSION_STABLE_UPTIME: Duration = Duration::from_secs(10);

struct Getty {
    pid: Pid,
    tty: String,
    serial: bool,
    started: Instant,
    failures: u32,
}

/// A getty that exited and is waiting for its respawn deadline.
struct PendingGetty {
    tty: String,
    serial: bool,
    due: Instant,
    failures: u32,
}

/// The single-user root shell, waiting for its respawn deadline.
struct PendingShell {
    due: Instant,
    failures: u32,
}

fn respawn_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    (GETTY_RESPAWN_BASE * 2u32.pow(shift)).min(GETTY_RESPAWN_MAX)
}

pub fn run() -> Result<()> {
    crate::log::init();
    crate::mount::mount_pseudo_fs();

    let cfg = crate::config::load(Path::new(crate::config::DEFAULT_PATH))
        .context("load configuration")?;
    crate::log::init_file(&cfg.log_file);
    crate::log::set_level(cfg.level_choice());
    crate::info!("stage2: starting as pid {}", nix::unistd::getpid());
    if !cfg.log_file.is_empty() {
        crate::info!("logging to {}", cfg.log_file);
    }
    if let Ok(n) = std::env::var("JIMINIT_SOFT_REBOOTS") {
        crate::info!("soft reboot #{n}: running stage2 again");
    }

    let mut level = initial_level(&cfg);
    set_hostname(&cfg.hostname);
    setup_console(&cfg.console);
    let signals = block_signals()?;
    let signal_fd =
        SignalFd::with_flags(&signals.set, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC)
            .context("signalfd")?;
    let listener = control_listener();

    crate::cgroup::init(cfg.cgroup);
    let mut supervisor = Supervisor::new(Path::new(&cfg.services_dir), level);
    supervisor.set_ready_timeout(Duration::from_millis(cfg.ready_timeout_ms));
    supervisor.start_members();

    let mut getties: Vec<Getty> = Vec::new();
    let mut pending_getties: Vec<PendingGetty> = Vec::new();
    let mut single_shell: Option<Pid> = None;
    let mut pending_shell: Option<PendingShell> = None;
    let mut single_failures: u32 = 0;
    let mut shell_started = Instant::now();
    let mut shell_pid = single_shell;
    if level == Level::Single {
        single_shell = Some(spawn_shell(&cfg)?);
        shell_pid = single_shell;
        shell_started = Instant::now();
    } else {
        start_getties(&cfg, &mut getties);
    }
    let mut previous_level = level;

    loop {
        // Wake on a signal, on service output, on a control request, or after
        // the timeout.
        {
            let mut pollfds = Vec::with_capacity(3 + supervisor.log_count());
            pollfds.push(PollFd::new(signal_fd.as_fd(), PollFlags::POLLIN));
            if let Some(listener) = &listener {
                pollfds.push(PollFd::new(listener.as_fd(), PollFlags::POLLIN));
            }
            for fd in supervisor.log_fds() {
                pollfds.push(PollFd::new(fd, PollFlags::POLLIN));
            }
            for fd in supervisor.notify_fds() {
                pollfds.push(PollFd::new(fd, PollFlags::POLLIN));
            }
            let _ = poll(&mut pollfds, LOOP_TIMEOUT_MS);
        }
        supervisor.drain_logs();
        supervisor.drain_notify();

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
                    crate::debug!("ignoring signal {}", info.ssi_signo)
                }
                other => crate::warn!("unexpected signal {other}"),
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

        if level != previous_level {
            previous_level = level;
            // Restarts scheduled for the old sessions no longer apply.
            pending_getties.clear();
            pending_shell = None;
        }
        if single_shell != shell_pid {
            shell_pid = single_shell;
            shell_started = Instant::now();
        }

        // Reap on every wakeup rather than only on SIGCHLD, so a coalesced
        // signal can never leave zombies behind.
        let (exited, _) = reap_all();
        for status in &exited {
            let Some(pid) = status.pid() else {
                crate::trace!("reaped: {}", describe(status));
                continue;
            };

            if supervisor.handle_exit(pid, status) {
                continue;
            }

            if let Some(position) = getties.iter().position(|getty| getty.pid == pid) {
                let getty = getties.remove(position);
                let failures = if getty.started.elapsed() >= SESSION_STABLE_UPTIME {
                    1
                } else {
                    getty.failures.saturating_add(1)
                };
                if level != Level::Single {
                    let delay = respawn_delay(failures);
                    crate::warn!(
                        "getty on {} (pid {pid}) exited: {}; respawning in {}ms",
                        getty.tty,
                        describe(status),
                        delay.as_millis()
                    );
                    pending_getties.push(PendingGetty {
                        tty: getty.tty,
                        serial: getty.serial,
                        due: Instant::now() + delay,
                        failures,
                    });
                } else {
                    crate::warn!(
                        "getty on {} (pid {pid}) exited: {}",
                        getty.tty,
                        describe(status)
                    );
                }
                continue;
            }

            if Some(pid) == single_shell {
                single_shell = None;
                if level != Level::Single {
                    // Killed by a runlevel switch; nothing to respawn.
                    crate::debug!("root shell {pid} exited");
                } else if cfg.respawn {
                    let failures = if shell_started.elapsed() >= SESSION_STABLE_UPTIME {
                        1
                    } else {
                        single_failures.saturating_add(1)
                    };
                    single_failures = failures;
                    let delay = respawn_delay(failures);
                    crate::warn!(
                        "root shell {pid} exited: {}; respawning in {}ms",
                        describe(status),
                        delay.as_millis()
                    );
                    pending_shell = Some(PendingShell {
                        due: Instant::now() + delay,
                        failures,
                    });
                } else {
                    crate::warn!("root shell {pid} exited: {}", describe(status));
                    crate::mesg!("respawn is disabled; shutting down");
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

            crate::trace!("reaped: {}", describe(status));
        }

        supervisor.tick();

        // Respawn crashed sessions once their deadline passed. This runs from
        // the main loop so PID 1 never sleeps and keeps handling signals,
        // service output and control requests meanwhile.
        let now = Instant::now();
        let mut index = 0;
        while index < pending_getties.len() {
            if pending_getties[index].due > now {
                index += 1;
                continue;
            }
            let pending = pending_getties.remove(index);
            if level != Level::Single && !getties.iter().any(|getty| getty.tty == pending.tty) {
                spawn_getty_for(
                    &cfg,
                    &pending.tty,
                    pending.serial,
                    pending.failures,
                    &mut getties,
                );
            }
        }
        if pending_shell
            .as_ref()
            .is_some_and(|pending| pending.due <= now)
        {
            let pending = pending_shell.take().expect("checked above");
            match spawn_shell(&cfg) {
                Ok(pid) => {
                    single_shell = Some(pid);
                    shell_pid = single_shell;
                    shell_started = Instant::now();
                }
                Err(e) => {
                    crate::error!("cannot respawn the root shell: {e:#}");
                    let failures = pending.failures.saturating_add(1);
                    pending_shell = Some(PendingShell {
                        due: Instant::now() + respawn_delay(failures),
                        failures,
                    });
                }
            }
        }
    }
}

/// The runlevel to start in. The `jiminit.runlevel=` kernel parameter wins
/// over `default_runlevel` from the configuration, so a rescue boot needs no
/// edit of `/etc/jiminit.toml`.
fn initial_level(cfg: &Config) -> Level {
    let requested = std::fs::read_to_string("/proc/cmdline")
        .ok()
        .and_then(|raw| {
            Cmdline::parse(&raw)
                .value("jiminit.runlevel")
                .map(str::to_string)
        });
    choose_level(requested.as_deref(), &cfg.default_runlevel)
}

fn choose_level(requested: Option<&str>, configured: &str) -> Level {
    if let Some(value) = requested {
        match Level::parse(value) {
            Some(level) => {
                crate::info!(
                    "kernel parameter jiminit.runlevel={value}: starting in {level} \
                     (overrides default_runlevel)"
                );
                return level;
            }
            None => crate::warn!(
                "unknown jiminit.runlevel `{value}` on the kernel command line; \
                 falling back to default_runlevel"
            ),
        }
    }
    match Level::parse(configured) {
        Some(level) => level,
        None => {
            crate::error!(
                "unknown default_runlevel `{configured}`; using {}",
                Level::DEFAULT
            );
            Level::DEFAULT
        }
    }
}

/// Fallback loop used when a stage fails: keep a shell alive on the console so
/// the machine is still usable and PID 1 never exits.
pub fn rescue() -> ! {
    let cfg = crate::config::load_or_default(Path::new(crate::config::DEFAULT_PATH));
    crate::log::init_file(&cfg.log_file);
    crate::log::set_level(cfg.level_choice());
    let shell = if Path::new(&cfg.shell).exists() {
        cfg.shell.clone()
    } else {
        "/bin/sh".to_string()
    };

    crate::crit!("starting rescue shell ({shell})");

    setup_console(&cfg.console);
    loop {
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                let _ = waitpid(child, None);
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(ForkResult::Child) => child_shell(&cfg.console, &shell),
            Err(e) => {
                crate::error!("fork failed: {e}");
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
            crate::error!("cannot open console {path}: {e}");
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

/// Give getty a clean stdin but keep its diagnostics on the console, so a
/// login program that refuses to start is visible in the boot log.
fn detach_getty_stdio() {
    let Ok(null) = open("/dev/null", OFlag::O_RDONLY, Mode::empty()) else {
        return;
    };
    let _ = unsafe { libc::dup2(null.as_raw_fd(), 0) };

    match open("/dev/console", OFlag::O_WRONLY, Mode::empty()) {
        Ok(console) => {
            let raw = console.as_raw_fd();
            let _ = unsafe { libc::dup2(raw, 1) };
            let _ = unsafe { libc::dup2(raw, 2) };
        }
        Err(_) => {
            let _ = unsafe { libc::dup2(null.as_raw_fd(), 1) };
            let _ = unsafe { libc::dup2(null.as_raw_fd(), 2) };
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
        spawn_getty_for(cfg, &tty, serial, 0, getties);
    }
}

fn spawn_getty_for(cfg: &Config, tty: &str, serial: bool, failures: u32, getties: &mut Vec<Getty>) {
    match spawn_getty(cfg, tty, serial) {
        Ok(pid) => {
            crate::info!("started getty on {tty} (pid {pid})");
            getties.push(Getty {
                pid,
                tty: tty.to_string(),
                serial,
                started: Instant::now(),
                failures,
            });
        }
        Err(e) => crate::fail!("cannot start getty on {tty}: {e:#}"),
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
    detach_getty_stdio();

    let baud = if serial {
        cfg.serial_getty_baud.as_str()
    } else {
        cfg.getty_baud.as_str()
    };

    let tty = if cfg.getty_tty_basename {
        tty.strip_prefix("/dev/").unwrap_or(tty)
    } else {
        tty
    };
    let order: [&str; 4] = if cfg.getty_baud_first {
        ["getty", "-L", baud, tty]
    } else {
        ["getty", "-L", tty, baud]
    };
    let mut strings = Vec::with_capacity(4);
    for value in order {
        match CString::new(value) {
            Ok(value) => strings.push(value),
            Err(_) => {
                crate::fail!("getty argument contains a NUL byte");
                unsafe { libc::_exit(127) }
            }
        }
    }
    let path = match CString::new(cfg.getty.as_str()) {
        Ok(path) => path,
        Err(_) => {
            crate::fail!("getty path contains a NUL byte");
            unsafe { libc::_exit(127) }
        }
    };
    let env = inherited_env();

    let error = nix::unistd::execve(&path, &strings, &env);
    crate::fail!("cannot exec {} on {tty}: {error:?}", cfg.getty);
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
    let argv0 = match CString::new(format!("-{name}")) {
        Ok(argv0) => argv0,
        Err(_) => {
            crate::crit!("shell name contains a NUL byte");
            unsafe { libc::_exit(127) }
        }
    };
    let path = match CString::new(shell) {
        Ok(path) => path,
        Err(_) => {
            crate::crit!("shell path contains a NUL byte");
            unsafe { libc::_exit(127) }
        }
    };
    let env = shell_env(shell);
    let args = [argv0];

    let error = nix::unistd::execve(&path, &args, &env);
    crate::crit!("cannot exec {shell}: {error:?}");
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
                crate::error!("waitpid failed: {e}");
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
        Ok(listener) => {
            // The socket can power the machine off, so keep it root-only
            // regardless of the umask PID 1 inherited.
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(CONTROL_PATH, std::fs::Permissions::from_mode(0o600))
            {
                crate::error!("cannot restrict {CONTROL_PATH} to root: {e}");
            }
            match listener.set_nonblocking(true) {
                Ok(()) => {
                    crate::info!("control socket listening on {CONTROL_PATH}");
                    Some(listener)
                }
                Err(e) => {
                    crate::error!("cannot make {CONTROL_PATH} nonblocking: {e}");
                    None
                }
            }
        }
        Err(e) => {
            crate::error!("cannot create {CONTROL_PATH}: {e}");
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
                crate::error!("accept on {CONTROL_PATH}: {e}");
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

    // Suspend and hibernate freeze the machine, so they answer only after the
    // wake-up and never turn into a shutdown action.
    if power_state_request(command) {
        let result = if command == "suspend" {
            crate::power::suspend()
        } else {
            crate::power::hibernate()
        };
        let reply = match result {
            Ok(()) => format!("ok resumed from {command}\n"),
            Err(e) => format!("error {e:#}\n"),
        };
        if let Err(e) = stream.write_all(reply.as_bytes()) {
            crate::warn!("cannot reply on {CONTROL_PATH}: {e}");
        }
        return None;
    }

    let (reply, action) = if command == "status" {
        let mut reply = format!("status {}\n", level.name());
        for line in supervisor.status_lines() {
            reply.push_str(&line);
            reply.push('\n');
        }
        (reply, None)
    } else if let Some(name) = command.strip_prefix("status ") {
        match supervisor.status_line_for(name.trim()) {
            Ok(line) => (format!("status {}\n{line}\n", level.name()), None),
            Err(e) => (format!("error {e:#}\n"), None),
        }
    } else if command == "list" {
        let mut reply = String::from("list\n");
        for name in supervisor.list_lines() {
            reply.push_str(&name);
            reply.push('\n');
        }
        (reply, None)
    } else if command == "metrics" {
        (supervisor.metrics(level.name()), None)
    } else if command == "runlevel" {
        (format!("runlevel {}\n", level.name()), None)
    } else if let Some(name) = command.strip_prefix("runlevel ") {
        runlevel_request(cfg, name.trim(), level, supervisor, getties, single_shell)
    } else if let Some(rest) = command.strip_prefix("service ") {
        let mut parts = rest.splitn(2, ' ');
        match (parts.next(), parts.next()) {
            (Some(action), Some(name)) => match supervisor.control(name.trim(), action.trim()) {
                Ok(message) => (format!("{message}\n"), None),
                Err(e) => (format!("error {e:#}\n"), None),
            },
            _ => (
                "error usage: service <start|stop|restart|reload> <name>\n".to_string(),
                None,
            ),
        }
    } else if command == "kexec" {
        // Refuse before stopping anything: a failed RB_KEXEC would leave the
        // machine half-shut-down with a read-only root.
        match crate::power::kexec_loaded() {
            Ok(true) => ("ok kexec\n".to_string(), Some(Action::Kexec)),
            Ok(false) => (
                "error kexec: no kernel image is loaded; run `kexec --load <kernel>` first\n"
                    .to_string(),
                None,
            ),
            Err(e) => (format!("error kexec: {e:#}\n"), None),
        }
    } else if let Some(action) = power_action(command) {
        (format!("ok {command}\n"), Some(action))
    } else {
        (
            "error unknown request (try `status [service]`, `list`, `metrics`, `runlevel [name]`, `service <action> <name>`, `poweroff`, `reboot`, `halt`, `soft-reboot`, `kexec`, `suspend` or `hibernate`)\n"
                .to_string(),
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
    match Level::parse(name) {
        Some(target) => {
            if target == *level {
                return (format!("ok runlevel {target}\n"), None);
            }
            switch_level(cfg, target, level, supervisor, getties, single_shell);
            (format!("ok runlevel {target}\n"), None)
        }
        None => (
            format!(
                "error unknown runlevel `{name}`; use single, multi-user or graphical \
                 (power commands are `jiminit poweroff`, `jiminit reboot`, ...)\n"
            ),
            None,
        ),
    }
}

/// Shutdown verbs accepted on the control socket.
fn power_action(command: &str) -> Option<Action> {
    match command {
        "poweroff" => Some(Action::PowerOff),
        "reboot" => Some(Action::Reboot),
        "halt" => Some(Action::Halt),
        "soft-reboot" => Some(Action::SoftReboot),
        _ => None,
    }
}

/// Suspend and hibernate are handled inline instead of as a shutdown action.
fn power_state_request(command: &str) -> bool {
    matches!(command, "suspend" | "hibernate")
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
    // Leaving graphical: whatever held the display (the greeter's X server, a
    // compositor) has to be gone before the console can repaint again.
    let left_graphical = *level == Level::Graphical && target != Level::Graphical;

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

    if left_graphical {
        // Whatever still owns the display has to go first, otherwise the
        // console cannot take the framebuffer back.
        stop_display_holders(supervisor);
        // Give the text console back before a getty or shell writes to it, and
        // make it the visible VT: the session ran on its own tty (7/8).
        restore_text_console(cfg.ttys.first().and_then(|tty| tty_number(tty)));
    }

    supervisor.start_members();

    if target == Level::Single {
        if single_shell.is_none() {
            match spawn_shell(cfg) {
                Ok(pid) => *single_shell = Some(pid),
                Err(e) => crate::error!("cannot start the single-user shell: {e:#}"),
            }
        }
    } else {
        start_getties(cfg, getties);
    }

    *level = target;
    crate::succ!("now in runlevel {target}");
}

fn stop_getties(getties: &mut Vec<Getty>) -> Vec<Pid> {
    let mut pids = Vec::new();
    for getty in getties.drain(..) {
        crate::info!("getty on {}: stopping (SIGHUP)", getty.tty);
        signal_tree(getty.pid, Signal::SIGHUP);
        pids.push(getty.pid);
    }
    pids
}

fn stop_single_shell(single_shell: &mut Option<Pid>) -> Option<Pid> {
    let pid = single_shell.take()?;
    crate::info!("single-user shell {pid}: stopping (SIGHUP)");
    signal_tree(pid, Signal::SIGHUP);
    Some(pid)
}

/// Signal a process and its process group. The group catches children that
/// were forked but have not called setsid yet (their pid is not a group id
/// then), and the plain pid still works after the leader is gone.
fn signal_tree(pid: Pid, signal: Signal) {
    let _ = kill(pid, signal);
    if pid.as_raw() > 1 {
        let _ = kill(Pid::from_raw(-pid.as_raw()), signal);
    }
}

fn pid_alive(pid: Pid) -> bool {
    let rc = unsafe { libc::kill(pid.as_raw(), 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Wait until every process in `pids` is gone (services are reaped through the
/// supervisor, sessions through the kernel's liveness check), killing the
/// stragglers after `timeout`.
fn wait_for(supervisor: &mut Supervisor, pids: &[Pid], timeout: Duration) {
    let mut wait = crate::log::Wait::start(
        format!("stopping {} process(es)", pids.len()),
        Some(timeout),
    );
    let deadline = Instant::now() + timeout;
    loop {
        wait.tick();
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
            // Daemonizing services (a display manager, its X server, the
            // session it spawned) leave the service's process group, so the
            // group signal below cannot reach them; the cgroup covers the
            // whole tree.
            supervisor.kill_foreign();
            for pid in pids {
                // Login sessions are process groups led by getty/shell.
                signal_tree(*pid, Signal::SIGKILL);
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
            _ => crate::trace!("reaped: {}", describe(status)),
        }
    }
    supervisor.drain_logs();
}

/// PIDs that have a DRM device open. A compositor or X server keeps the
/// display until it closes these, and only the process holding them can leave
/// the console in a state where nothing repaints.
fn display_holders() -> Vec<Pid> {
    let mut holders = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return holders;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let uses_drm = fds.flatten().any(|fd| {
            std::fs::read_link(fd.path())
                .map(|target| target.as_os_str().as_bytes().starts_with(b"/dev/dri/"))
                .unwrap_or(false)
        });
        if uses_drm {
            holders.push(Pid::from_raw(pid));
        }
    }
    holders
}

/// Terminate whatever still holds the display after the graphical services
/// were stopped. A Wayland session is usually moved into its own login
/// session scope, outside the service cgroup, so neither the process group
/// nor `cgroup.kill` reaches its compositor.
fn stop_display_holders(supervisor: &mut Supervisor) {
    let holders = display_holders();
    if holders.is_empty() {
        return;
    }
    crate::info!(
        "stopping {} process(es) still holding the display",
        holders.len()
    );
    for pid in &holders {
        signal_tree(*pid, Signal::SIGTERM);
    }
    let mut wait = crate::log::Wait::start(
        "waiting for the display to be released",
        Some(Duration::from_secs(2)),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        wait.tick();
        reap_for(supervisor);
        if !display_holders().iter().any(|pid| pid_alive(*pid)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let leftover = display_holders();
    if !leftover.is_empty() {
        crate::warn!(
            "{} process(es) still hold the display; sending SIGKILL",
            leftover.len()
        );
        for pid in leftover {
            signal_tree(pid, Signal::SIGKILL);
        }
        let hard = Instant::now() + Duration::from_secs(1);
        while Instant::now() < hard {
            reap_for(supervisor);
            if !display_holders().iter().any(|pid| pid_alive(*pid)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

// --- shutdown ------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Action {
    PowerOff,
    /// Stop everything and halt, leaving the machine powered on.
    Halt,
    Reboot,
    /// Stop everything, then exec a fresh stage 2 in this same PID instead of
    /// asking the kernel to reboot.
    SoftReboot,
    /// Reboot into a kernel image that is already loaded in memory.
    Kexec,
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
        signal_tree(pid, Signal::SIGHUP);
    }
    supervisor.stop();

    // Sessions and services have been asked to stop; this also reaches their
    // children (login, shells, shell jobs) which are not PID 1's own children
    // and would otherwise still write to the filesystems when we remount them
    // read-only.
    let _ = kill(Pid::from_raw(-1), Signal::SIGTERM);

    // Phase 1: give everything shutdown_timeout_ms to exit after SIGTERM.
    // Phase 2: SIGKILL whatever is left, then wait a little longer.
    let mut wait = crate::log::Wait::start(
        "stopping sessions and services",
        Some(Duration::from_millis(cfg.shutdown_timeout_ms)),
    );
    let deadline = Instant::now() + Duration::from_millis(cfg.shutdown_timeout_ms);
    let mut hard_deadline: Option<Instant> = None;
    loop {
        wait.tick();
        let (exited, all_gone) = reap_all();
        for status in &exited {
            match status.pid() {
                Some(pid) if !supervisor.handle_exit(pid, status) => {
                    crate::trace!("reaped: {}", describe(status))
                }
                None => crate::trace!("reaped: {}", describe(status)),
                Some(_) => {}
            }
        }
        supervisor.drain_logs();

        let others = live_user_processes();
        if all_gone && others == 0 {
            break;
        }

        let now = Instant::now();
        match hard_deadline {
            None if now >= deadline => {
                crate::warn!("timed out waiting for {others} process(es); sending SIGKILL");
                supervisor.kill_all();
                let _ = kill(Pid::from_raw(-1), Signal::SIGKILL);
                hard_deadline = Some(now + Duration::from_secs(2));
            }
            Some(hard) if now >= hard => {
                crate::error!("{others} process(es) survived SIGKILL; continuing anyway");
                break;
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(wait);
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
        Action::Halt => nix::sys::reboot::RebootMode::RB_HALT_SYSTEM,
        Action::Reboot => nix::sys::reboot::RebootMode::RB_AUTOBOOT,
        Action::Kexec => nix::sys::reboot::RebootMode::RB_KEXEC,
        Action::SoftReboot => unreachable!("handled before the reboot syscall"),
    };
    crate::info!("requesting {how:?}");
    match nix::sys::reboot::reboot(how) {
        Ok(never) => match never {},
        Err(e) => crate::crit!("reboot syscall failed: {e}"),
    }

    crate::crit!("system did not power off; idling");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Number of live user processes other than PID 1. Kernel threads have no
/// `/proc/<pid>/exe` and dead processes (zombies) are unreadable too, so this
/// only counts processes that can still touch the filesystems.
fn live_user_processes() -> usize {
    let self_pid = nix::unistd::getpid().as_raw();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|name| name.parse::<i32>().ok()) else {
                return false;
            };
            pid > 1 && pid != self_pid && std::fs::read_link(entry.path().join("exe")).is_ok()
        })
        .count()
}

/// Clear the transient directories and exec a fresh stage 2 in this same PID,
/// so the whole boot flow (config, services, shell) runs again without asking
/// the kernel to reboot.
/// Put the console back into text mode, optionally making `vt` the foreground
/// virtual terminal first.
///
/// A graphical session leaves its VT in `KD_GRAPHICS`, and a compositor that
/// dies without cleaning up (or is killed) leaves the DRM cursor plane
/// programmed. The kernel then never repaints text: the screen stays black
/// with a stale pointer, and switching VTs does nothing because the console
/// driver never took the display back. `KDSETMODE`/`KD_TEXT` releases fbcon
/// again, so it has to run after the graphical processes are gone.
fn restore_text_console(vt: Option<u32>) {
    const KDSETMODE: libc::c_ulong = 0x4B3A;
    const KD_TEXT: libc::c_int = 0x00;
    const KDSKBMODE: libc::c_ulong = 0x4B45;
    const K_UNICODE: libc::c_int = 0x06;
    const VT_ACTIVATE: libc::c_ulong = 0x5606;

    if let Some(vt) = vt {
        // Activate before the mode change: both ioctls apply to the VT the
        // descriptor refers to, and /dev/console follows the active VT.
        if let Ok(fd) = open("/dev/console", OFlag::O_RDWR, Mode::empty()) {
            let raw = fd.as_raw_fd();
            unsafe { libc::ioctl(raw, VT_ACTIVATE, vt as libc::c_int) };
        }
    }

    let Ok(fd) = open("/dev/console", OFlag::O_RDWR, Mode::empty()) else {
        return;
    };
    let raw = fd.as_raw_fd();
    let text = unsafe { libc::ioctl(raw, KDSETMODE, KD_TEXT) };
    if text == 0 {
        unsafe { libc::ioctl(raw, KDSKBMODE, K_UNICODE) };
        crate::info!("console restored to text mode");
    }
}

/// Virtual terminal number of a `ttys` entry (`/dev/tty3` -> `3`).
fn tty_number(path: &str) -> Option<u32> {
    path.strip_prefix("/dev/tty")?.parse().ok()
}

fn soft_reboot_exec(cfg: &Config) -> Result<()> {
    restore_text_console(None);
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
            crate::crit!("soft reboot failed: {e:#}; falling back to the rescue shell");
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
        Err(e) => crate::error!("cannot mount a fresh tmpfs on {dir}: {e}"),
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
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or(0u32)
        .saturating_add(1)
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
    use super::{choose_level, parse_soft_reboot_count, respawn_delay, tty_number};
    use std::time::Duration;

    #[test]
    fn kernel_parameter_overrides_the_configured_runlevel() {
        use crate::runlevel::Level;
        assert_eq!(choose_level(Some("single"), "graphical"), Level::Single);
        assert_eq!(choose_level(Some("3"), "single"), Level::MultiUser);
        assert_eq!(
            choose_level(Some("graphical"), "multi-user"),
            Level::Graphical
        );
    }

    #[test]
    fn invalid_or_missing_kernel_parameter_falls_back_to_the_config() {
        use crate::runlevel::Level;
        assert_eq!(choose_level(None, "graphical"), Level::Graphical);
        assert_eq!(choose_level(Some(""), "single"), Level::Single);
        assert_eq!(choose_level(Some("rescue"), "multi-user"), Level::MultiUser);
        assert_eq!(choose_level(None, "nonsense"), Level::DEFAULT);
    }

    #[test]
    fn vt_number_is_parsed_from_the_console_device() {
        assert_eq!(tty_number("/dev/tty1"), Some(1));
        assert_eq!(tty_number("/dev/tty12"), Some(12));
        assert_eq!(tty_number("/dev/ttyS0"), None);
        assert_eq!(tty_number("console"), None);
    }

    #[test]
    fn session_respawn_backoff_grows_and_caps() {
        assert_eq!(respawn_delay(1), Duration::from_millis(500));
        assert_eq!(respawn_delay(2), Duration::from_millis(1_000));
        assert_eq!(respawn_delay(3), Duration::from_millis(2_000));
        assert_eq!(respawn_delay(7), Duration::from_secs(30));
        assert_eq!(respawn_delay(40), Duration::from_secs(30));
    }

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
