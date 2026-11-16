//! Per-session user instances and the login watcher that starts them.
//!
//! `jiminit userspawn` runs as root (supervised by PID 1) and watches
//! `/run/user/<uid>`, which elogind creates for every login session. When a
//! session appears it forks `jiminit --user` as that user; when the directory
//! disappears the instance gets SIGTERM.
//!
//! `jiminit --user` supervises `~/.config/jiminit/services.d/*.toml` in the
//! caller's session: dependencies, restarts, sd_notify and a control socket at
//! `$XDG_RUNTIME_DIR/jiminit.sock`. It never touches the console, getty,
//! hostname, cgroups or the power state; it exits when the session ends.

use crate::config::Config;
use crate::runlevel::Level;
use crate::service::Supervisor;
use anyhow::{bail, Context, Result};
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::signal::{kill, SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often `userspawn` rescans `/run/user` (it also wakes on signals).
const SCAN_INTERVAL: Duration = Duration::from_secs(1);

/// Delay before retrying a user instance that exited on its own.
const RETRY_DELAY: Duration = Duration::from_secs(3);

/// How long a session instance gets to stop before it is SIGKILLed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// The session runtime directory (`$XDG_RUNTIME_DIR`, else `/run/user/<uid>`).
pub fn runtime_dir(uid: u32) -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(format!("/run/user/{uid}")),
    }
}

/// Where the user instance keeps its sockets.
pub fn run_dir(uid: u32) -> PathBuf {
    runtime_dir(uid).join("jiminit")
}

/// The control socket of the current session's user instance.
pub fn control_path() -> PathBuf {
    run_dir(unsafe { libc::getuid() }).join("jiminit.sock")
}

/// `$XDG_CONFIG_HOME/jiminit` (default `~/.config/jiminit`).
fn config_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("jiminit"),
        _ => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"));
            home.join(".config/jiminit")
        }
    }
}

fn user_config() -> Config {
    let path = config_dir().join("jiminit.toml");
    let mut cfg = if path.exists() {
        match crate::config::load(&path) {
            Ok(cfg) => cfg,
            Err(e) => {
                crate::warn!("{e:#}; using defaults for the user instance");
                Config::default()
            }
        }
    } else {
        Config::default()
    };
    if cfg.services_dir == crate::service::DEFAULT_DIR {
        cfg.services_dir = config_dir().join("services.d").display().to_string();
    }
    cfg
}

/// `jiminit --user`: supervise this session's services until it ends.
pub fn run() -> Result<()> {
    crate::log::init();
    let uid = unsafe { libc::getuid() };
    let cfg = user_config();
    crate::log::set_level(cfg.level_choice());

    let run_dir = run_dir(uid);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("cannot create {}", run_dir.display()))?;

    // No console, no hostname, no cgroups: a session instance only supervises.
    crate::cgroup::init(false);
    // A login session wants every user service, including graphical ones.
    let mut supervisor = Supervisor::new(Path::new(&cfg.services_dir), Level::Graphical);
    supervisor.set_ready_timeout(Duration::from_millis(cfg.ready_timeout_ms));
    supervisor.set_run_dir(run_dir.clone());
    supervisor.start_members();

    let socket_path = run_dir.join("jiminit.sock");
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("cannot bind {}", socket_path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600));
    }
    listener
        .set_nonblocking(true)
        .context("nonblocking listener")?;

    // Adopt orphaned service children (a daemonizing service leaves them to
    // the next subreaper) so they are reaped and restarted correctly.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }

    let signals = block_signals()?;
    let signal_fd = SignalFd::with_flags(&signals, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC)
        .context("signalfd")?;

    crate::info!(
        "user instance for uid {uid}: {} service(s) from {}",
        supervisor.list_lines().len(),
        cfg.services_dir
    );
    crate::succ!("user instance listening on {}", socket_path.display());

    let mut stopping = false;
    loop {
        {
            let mut pollfds = Vec::with_capacity(2 + supervisor.log_count());
            pollfds.push(PollFd::new(signal_fd.as_fd(), PollFlags::POLLIN));
            pollfds.push(PollFd::new(listener.as_fd(), PollFlags::POLLIN));
            for fd in supervisor.log_fds() {
                pollfds.push(PollFd::new(fd, PollFlags::POLLIN));
            }
            for fd in supervisor.notify_fds() {
                pollfds.push(PollFd::new(fd, PollFlags::POLLIN));
            }
            let _ = poll(&mut pollfds, 100u16);
        }
        supervisor.drain_logs();
        supervisor.drain_notify();
        reap(&mut supervisor);

        while let Some(info) = signal_fd.read_signal().context("read signalfd")? {
            match info.ssi_signo as i32 {
                libc::SIGTERM | libc::SIGINT => {
                    crate::info!("session ended: stopping the user instance");
                    stopping = true;
                }
                libc::SIGCHLD => {}
                libc::SIGHUP | libc::SIGQUIT => {
                    crate::trace!("ignoring signal {}", info.ssi_signo)
                }
                other => crate::warn!("unexpected signal {other}"),
            }
        }
        if stopping {
            break;
        }

        if let Some(action) = handle_control(&listener, &mut supervisor)? {
            if action == ControlAction::Stop {
                stopping = true;
                continue;
            }
        }

        supervisor.tick();
    }

    supervisor.stop();
    let deadline = Instant::now() + STOP_TIMEOUT;
    while supervisor.running_count() > 0 && Instant::now() < deadline {
        reap(&mut supervisor);
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(&socket_path);
    crate::info!("user instance stopped");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlAction {
    Continue,
    Stop,
}

fn handle_control(
    listener: &UnixListener,
    supervisor: &mut Supervisor,
) -> Result<Option<ControlAction>> {
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e).context("accept on the session socket"),
        };
        if let Some(action) = handle_client(stream, supervisor) {
            return Ok(Some(action));
        }
    }
}

fn handle_client(mut stream: UnixStream, supervisor: &mut Supervisor) -> Option<ControlAction> {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 256];
    let read = stream.read(&mut buf).unwrap_or(0);
    let command = String::from_utf8_lossy(&buf[..read]).trim().to_string();

    let (reply, action) = match command.as_str() {
        "status" => {
            let mut reply = String::from("status graphical\n");
            for line in supervisor.status_lines() {
                reply.push_str(&line);
                reply.push('\n');
            }
            (reply, ControlAction::Continue)
        }
        "list" => {
            let mut reply = String::from("list\n");
            for name in supervisor.list_lines() {
                reply.push_str(&name);
                reply.push('\n');
            }
            (reply, ControlAction::Continue)
        }
        "metrics" => (supervisor.metrics("graphical"), ControlAction::Continue),
        "runlevel" => ("runlevel graphical\n".to_string(), ControlAction::Continue),
        "stop" => ("ok stopping the user instance\n".to_string(), ControlAction::Stop),
        "reboot" | "poweroff" | "halt" | "soft-reboot" | "kexec" | "suspend" | "hibernate" => (
            format!("error `{command}` is not available in a user instance\n"),
            ControlAction::Continue,
        ),
        other if other.starts_with("status ") => {
            match supervisor.status_line_for(other["status ".len()..].trim()) {
                Ok(line) => (
                    format!("status multi-user\n{line}\n"),
                    ControlAction::Continue,
                ),
                Err(e) => (format!("error {e:#}\n"), ControlAction::Continue),
            }
        }
        other if other.starts_with("service ") => {
            let mut parts = other["service ".len()..].splitn(2, ' ');
            match (parts.next(), parts.next()) {
                (Some(action), Some(name)) => match supervisor.control(name.trim(), action.trim()) {
                    Ok(message) => (format!("{message}\n"), ControlAction::Continue),
                    Err(e) => (format!("error {e:#}\n"), ControlAction::Continue),
                },
                _ => (
                    "error usage: service <start|stop|restart|reload> <name>\n".to_string(),
                    ControlAction::Continue,
                ),
            }
        }
        _ => (
            "error unknown request (try `status [service]`, `list`, `metrics`, `service <action> <name>` or `stop`)\n"
                .to_string(),
            ControlAction::Continue,
        ),
    };

    let _ = stream.write_all(reply.as_bytes());
    Some(action)
}

fn reap(supervisor: &mut Supervisor) {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return,
            Ok(status) => {
                if let Some(pid) = status.pid() {
                    supervisor.handle_exit(pid, &status);
                }
            }
            Err(nix::errno::Errno::ECHILD) => return,
            Err(e) => {
                crate::warn!("waitpid failed: {e}");
                return;
            }
        }
    }
}

/// Block the signals both the session loop and userspawn handle via signalfd.
fn block_signals() -> Result<SigSet> {
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
    Ok(set)
}

// --- userspawn -----------------------------------------------------------

struct SessionChild {
    pid: Pid,
    user: String,
    stopping_since: Option<Instant>,
}

/// `jiminit userspawn`: start one `jiminit --user` per `/run/user/<uid>`.
pub fn userspawn() -> Result<()> {
    crate::log::init();
    if unsafe { libc::geteuid() } != 0 {
        bail!("`jiminit userspawn` must run as root (it switches to each session's user)");
    }
    let exe = std::fs::read_link("/proc/self/exe").context("resolve the jiminit binary path")?;
    let users = crate::accounts::load_passwd(Path::new(crate::accounts::PASSWD))
        .context("read /etc/passwd")?;
    let signals = block_signals()?;
    let signal_fd = SignalFd::with_flags(&signals, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC)
        .context("signalfd")?;

    let mut children: BTreeMap<u32, SessionChild> = BTreeMap::new();
    let mut retry_at: BTreeMap<u32, Instant> = BTreeMap::new();
    crate::info!("userspawn: watching /run/user for login sessions");

    loop {
        reap_sessions(&mut children, &mut retry_at);
        let present: Vec<u32> = session_uids_in(Path::new("/run/user"));

        for uid in &present {
            if *uid == 0 || children.contains_key(uid) {
                continue;
            }
            if retry_at.get(uid).is_some_and(|at| *at > Instant::now()) {
                continue;
            }
            let Some(user) = users.iter().find(|user| user.uid == *uid) else {
                crate::warn!("userspawn: no passwd entry for uid {uid}; skipping its session");
                retry_at.insert(*uid, Instant::now() + RETRY_DELAY);
                continue;
            };
            match spawn_user(&exe, user) {
                Ok(pid) => {
                    crate::succ!(
                        "userspawn: session for {} (uid {uid}) started as pid {pid}",
                        user.name
                    );
                    children.insert(
                        *uid,
                        SessionChild {
                            pid,
                            user: user.name.clone(),
                            stopping_since: None,
                        },
                    );
                }
                Err(e) => {
                    crate::fail!("userspawn: cannot start a session for {}: {e:#}", user.name);
                    retry_at.insert(*uid, Instant::now() + RETRY_DELAY);
                }
            }
        }

        for (uid, child) in children.iter_mut() {
            if present.contains(uid) || child.stopping_since.is_some() {
                continue;
            }
            crate::info!(
                "userspawn: session for {} (uid {uid}) ended; stopping its instance",
                child.user
            );
            let _ = kill(child.pid, Signal::SIGTERM);
            child.stopping_since = Some(Instant::now());
        }
        let expired: Vec<u32> = children
            .iter()
            .filter(|(_, child)| {
                child
                    .stopping_since
                    .is_some_and(|since| since.elapsed() > STOP_TIMEOUT)
            })
            .map(|(uid, _)| *uid)
            .collect();
        for uid in expired {
            if let Some(child) = children.remove(&uid) {
                crate::warn!(
                    "userspawn: session for {} (uid {uid}) did not stop; sending SIGKILL",
                    child.user
                );
                let _ = kill(child.pid, Signal::SIGKILL);
                let _ = waitpid(child.pid, None);
                retry_at.insert(uid, Instant::now() + RETRY_DELAY);
            }
        }

        // Wake on SIGCHLD/SIGTERM, otherwise rescan once per second.
        let mut pollfds = [PollFd::new(signal_fd.as_fd(), PollFlags::POLLIN)];
        let _ = poll(&mut pollfds, SCAN_INTERVAL.as_millis() as u16);
        while let Some(info) = signal_fd.read_signal().context("read signalfd")? {
            match info.ssi_signo as i32 {
                libc::SIGTERM | libc::SIGINT => {
                    crate::info!("userspawn: stopping all session instances");
                    for child in children.values() {
                        let _ = kill(child.pid, Signal::SIGTERM);
                    }
                    let deadline = Instant::now() + STOP_TIMEOUT;
                    while !children.is_empty() && Instant::now() < deadline {
                        reap_sessions(&mut children, &mut retry_at);
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    for child in children.values() {
                        let _ = kill(child.pid, Signal::SIGKILL);
                    }
                    return Ok(());
                }
                libc::SIGCHLD => {}
                libc::SIGHUP | libc::SIGQUIT => {}
                other => crate::warn!("userspawn: unexpected signal {other}"),
            }
        }
    }
}

/// Reap finished session children; schedule a retry when one dies while its
/// `/run/user` directory still exists.
fn reap_sessions(
    children: &mut BTreeMap<u32, SessionChild>,
    retry_at: &mut BTreeMap<u32, Instant>,
) {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return,
            Ok(status) => {
                let Some(pid) = status.pid() else { continue };
                let Some((uid, child)) = children.iter().find(|(_, c)| c.pid == pid) else {
                    continue;
                };
                let uid = *uid;
                let stopping = child.stopping_since.is_some();
                let user = child.user.clone();
                crate::info!("userspawn: session for {user} (uid {uid}) exited: {status:?}");
                children.remove(&uid);
                if !stopping {
                    crate::warn!(
                        "userspawn: session for {user} exited on its own; retrying in {}s",
                        RETRY_DELAY.as_secs()
                    );
                    retry_at.insert(uid, Instant::now() + RETRY_DELAY);
                }
            }
            Err(nix::errno::Errno::ECHILD) => return,
            Err(e) => {
                crate::warn!("userspawn: waitpid failed: {e}");
                return;
            }
        }
    }
}

fn session_uids_in(dir: &Path) -> Vec<u32> {
    let mut uids = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return uids;
    };
    for entry in entries.flatten() {
        if let Some(uid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        {
            uids.push(uid);
        }
    }
    uids.sort_unstable();
    uids
}

fn spawn_user(exe: &Path, user: &crate::accounts::User) -> Result<Pid> {
    let name = CString::new(user.name.as_str()).context("user name contains a NUL byte")?;
    let exe_c = CString::new(exe.as_os_str().as_bytes()).context("path contains a NUL byte")?;
    match unsafe { fork() }.context("fork")? {
        ForkResult::Parent { child } => Ok(child),
        ForkResult::Child => {
            // Detach from the supervisor's session so a userspawn restart does
            // not hang up the login session.
            unsafe { libc::setsid() };
            if unsafe { libc::initgroups(name.as_ptr(), user.gid as _) } != 0 {
                child_fail("initgroups");
            }
            if unsafe { libc::setgid(user.gid) } != 0 || unsafe { libc::setuid(user.uid) } != 0 {
                child_fail("setgid/setuid");
            }
            let env = session_env(user);
            let argv = [
                CString::new("jiminit").unwrap(),
                CString::new("--user").unwrap(),
            ];
            let _ = nix::unistd::execve(&exe_c, &argv, &env);
            child_fail("exec jiminit --user");
        }
    }
}

fn child_fail(what: &str) -> ! {
    let message = format!("userspawn: {what} failed\n");
    let _ = unsafe { libc::write(2, message.as_ptr().cast(), message.len()) };
    unsafe { libc::_exit(127) }
}

fn session_env(user: &crate::accounts::User) -> Vec<CString> {
    let runtime = format!("/run/user/{}", user.uid);
    let bus = format!("unix:path={runtime}/bus");
    let mut vars: BTreeMap<&str, String> = BTreeMap::new();
    vars.insert("HOME", user.home.clone());
    vars.insert("USER", user.name.clone());
    vars.insert("LOGNAME", user.name.clone());
    vars.insert("SHELL", user.shell.clone());
    vars.insert("XDG_RUNTIME_DIR", runtime);
    vars.insert("DBUS_SESSION_BUS_ADDRESS", bus);
    vars.insert("PATH", "/usr/local/bin:/usr/bin:/bin".to_string());
    if let Ok(lang) = std::env::var("LANG") {
        vars.insert("LANG", lang);
    }
    vars.into_iter()
        .map(|(key, value)| CString::new(format!("{key}={value}")).expect("environment has no NUL"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_socket_lives_in_the_runtime_dir() {
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/4242");
        assert_eq!(
            control_path(),
            Path::new("/run/user/4242/jiminit/jiminit.sock")
        );
        std::env::remove_var("XDG_RUNTIME_DIR");
    }

    #[test]
    fn session_uids_are_read_from_run_user() {
        let dir = std::env::temp_dir().join(format!("jiminit-run-user-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for name in ["1000", "1001", "not-a-uid"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        assert_eq!(session_uids_in(&dir), vec![1000, 1001]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(session_uids_in(&dir).is_empty());
    }

    #[test]
    fn config_dir_defaults_to_dot_config() {
        std::env::set_var("HOME", "/home/example");
        std::env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(config_dir(), Path::new("/home/example/.config/jiminit"));
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg");
        assert_eq!(config_dir(), Path::new("/tmp/xdg/jiminit"));
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}
