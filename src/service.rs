//! Service supervision: `/etc/jiminit/services.d/*.toml`.
//!
//! Each file describes one service. The file stem is the service name. Services
//! are started in dependency order (independent services start in parallel),
//! restarted on failure with exponential backoff, and stopped in reverse
//! startup order during shutdown. Their stdout/stderr is forwarded to the
//! kernel log with a `svc/<name>: ` prefix.

use crate::runlevel::Level;
use anyhow::{bail, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::WaitStatus;
use nix::unistd::{fork, ForkResult, Pid};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default directory scanned for service definitions.
pub const DEFAULT_DIR: &str = "/etc/jiminit/services.d";

const RESTART_BASE_MS: u64 = 1_000;
const RESTART_MAX_MS: u64 = 60_000;
/// Give up after this many consecutive failures (i.e. after this many restarts).
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// A run at least this long resets the failure counter.
const STABLE_UPTIME: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    #[default]
    OnFailure,
    Never,
}

#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub name: String,
    pub exec: String,
    pub args: Vec<String>,
    pub depends_on: Vec<String>,
    pub restart: RestartPolicy,
    pub oneshot: bool,
    pub working_dir: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Runlevels in which this service should run. Empty in the file means
    /// `multi-user` + `graphical`.
    pub runlevels: Vec<Level>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ServiceFile {
    exec: String,
    args: Vec<String>,
    depends_on: Vec<String>,
    restart: RestartPolicy,
    oneshot: bool,
    working_dir: Option<String>,
    env: BTreeMap<String, String>,
    runlevels: Vec<String>,
}

fn default_runlevels() -> Vec<Level> {
    vec![Level::MultiUser, Level::Graphical]
}

fn parse_runlevels(name: &str, values: &[String]) -> Result<Vec<Level>> {
    if values.is_empty() {
        return Ok(default_runlevels());
    }
    let mut levels = Vec::new();
    for value in values {
        match Level::parse(value) {
            Some(level) => {
                if !levels.contains(&level) {
                    levels.push(level);
                }
            }
            None => bail!(
                "service `{name}` has unknown runlevel `{value}` \
                 (use single, multi-user or graphical)"
            ),
        }
    }
    Ok(levels)
}

/// Parse one service definition. `name` is the file stem.
pub fn parse_service(name: &str, text: &str) -> Result<ServiceSpec> {
    let file: ServiceFile =
        toml::from_str(text).with_context(|| format!("service `{name}` is invalid"))?;

    if file.exec.trim().is_empty() {
        bail!("service `{name}` has no `exec`");
    }

    // Allow `exec = "/bin/sh -c 'echo hi'"` when no explicit `args` are given;
    // only shell-like quoting is understood, there is no expansion.
    let (exec, args) = if file.args.is_empty() {
        let mut words = split_command(&file.exec)
            .with_context(|| format!("service `{name}` has an invalid `exec`"))?
            .into_iter();
        let program = words.next().expect("checked to be non-empty above");
        (program, words.collect())
    } else {
        (file.exec.clone(), file.args.clone())
    };

    Ok(ServiceSpec {
        name: name.to_string(),
        exec,
        args,
        depends_on: file.depends_on,
        restart: file.restart,
        oneshot: file.oneshot,
        working_dir: file.working_dir,
        env: file.env,
        runlevels: parse_runlevels(name, &file.runlevels)?,
    })
}

/// Split a command into words using shell-like quoting: whitespace separates
/// words, single and double quotes group, and a backslash escapes the next
/// character (outside single quotes). No variables or globbing are expanded.
fn split_command(text: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    match chars.next() {
                        Some(escaped @ ('"' | '\\')) => word.push(escaped),
                        Some(other) => {
                            word.push('\\');
                            word.push(other);
                        }
                        None => word.push('\\'),
                    }
                } else {
                    word.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                        in_word = true;
                    }
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                c => {
                    word.push(c);
                    in_word = true;
                }
            },
        }
    }

    if quote.is_some() {
        bail!("unterminated quote");
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

fn load_dir(dir: &Path) -> Result<Vec<ServiceSpec>> {
    let entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => {
            let mut paths = Vec::new();
            for entry in entries {
                let path = entry?.path();
                if path.extension().map(|e| e == "toml").unwrap_or(false) {
                    paths.push(path);
                }
            }
            paths.sort();
            paths
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };

    let mut services = Vec::new();
    for path in entries {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse_service(&name, &text) {
                Ok(spec) => services.push(spec),
                Err(e) => crate::warn!("svc/{name}: {e:#}; skipping"),
            },
            Err(e) => crate::warn!("svc/{name}: cannot read {}: {e}", path.display()),
        }
    }
    Ok(services)
}

/// Dependency order, computed in waves: every service whose dependencies are
/// already launched can start together. Returns `(order, cyclic)` where
/// `cyclic` lists services skipped because of a dependency cycle.
fn startup_order(services: &[ServiceSpec]) -> (Vec<usize>, Vec<String>) {
    let mut remaining: BTreeSet<usize> = (0..services.len()).collect();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut order = Vec::new();

    loop {
        let mut ready: Vec<usize> = remaining
            .iter()
            .copied()
            .filter(|&i| services[i].depends_on.iter().all(|d| done.contains(d)))
            .collect();
        if ready.is_empty() {
            break;
        }
        ready.sort_by(|a, b| services[*a].name.cmp(&services[*b].name));
        for idx in ready {
            remaining.remove(&idx);
            done.insert(services[idx].name.clone());
            order.push(idx);
        }
    }

    let cyclic = remaining
        .iter()
        .map(|&i| services[i].name.clone())
        .collect();
    (order, cyclic)
}

/// Services that should run at `level`: tagged services plus their
/// dependencies, transitively.
fn members_of(services: &[ServiceSpec], level: Level) -> BTreeSet<usize> {
    let mut selected = BTreeSet::new();
    let mut queue: Vec<String> = Vec::new();
    for (idx, spec) in services.iter().enumerate() {
        if spec.runlevels.contains(&level) {
            selected.insert(idx);
            queue.extend(spec.depends_on.iter().cloned());
        }
    }
    while let Some(name) = queue.pop() {
        if let Some(idx) = services.iter().position(|spec| spec.name == name) {
            if selected.insert(idx) {
                queue.extend(services[idx].depends_on.iter().cloned());
            }
        }
    }
    selected
}

struct Running {
    idx: usize,
    started: Instant,
    failures: u32,
}

struct PendingRestart {
    idx: usize,
    due: Instant,
    failures: u32,
}

struct LogPipe {
    fd: OwnedFd,
    name: String,
    buf: Vec<u8>,
}

pub struct Supervisor {
    services: Vec<ServiceSpec>,
    running: HashMap<Pid, Running>,
    pending: Vec<PendingRestart>,
    logs: Vec<LogPipe>,
    stopping: bool,
    level: Level,
}

impl Supervisor {
    /// Load services from `dir`. Broken definitions and cycles are logged and
    /// skipped; an unusable directory simply yields no services.
    pub fn new(dir: &Path, level: Level) -> Self {
        let mut services = match load_dir(dir) {
            Ok(services) => services,
            Err(e) => {
                crate::warn!("svc: {e:#}");
                Vec::new()
            }
        };

        let known: BTreeSet<String> = services.iter().map(|s| s.name.clone()).collect();
        services.retain(|spec| {
            if let Some(missing) = spec.depends_on.iter().find(|d| !known.contains(*d)) {
                crate::warn!(
                    "svc/{}: depends on unknown service `{missing}`; skipping",
                    spec.name
                );
                false
            } else {
                true
            }
        });

        let (order, cyclic) = startup_order(&services);
        for name in &cyclic {
            crate::warn!("svc/{name}: dependency cycle; skipping");
        }
        // Keep only services that can be ordered, arranged in startup order so
        // that `stop()` can simply walk the vector backwards.
        services = order.iter().map(|&i| services[i].clone()).collect();

        crate::info!(
            "svc: loaded {} service(s) from {}",
            services.len(),
            dir.display()
        );

        Self {
            services,
            running: HashMap::new(),
            pending: Vec::new(),
            logs: Vec::new(),
            stopping: false,
            level,
        }
    }

    /// Indices of services that belong to `level`: those tagged for it plus
    /// their dependencies (a dependency shared with another level still has
    /// to run when the dependent runs).
    fn members(&self) -> BTreeSet<usize> {
        members_of(&self.services, self.level)
    }

    fn is_running(&self, idx: usize) -> bool {
        self.running.values().any(|run| run.idx == idx)
    }

    /// Start every member of the current runlevel that is not running yet.
    /// The vector is already in dependency order, so a single pass suffices.
    pub fn start_members(&mut self) {
        let members = self.members();
        for idx in 0..self.services.len() {
            if members.contains(&idx) && !self.is_running(idx) {
                let name = self.services[idx].name.clone();
                if let Err(e) = self.spawn(idx, 0) {
                    crate::err!("svc/{name}: cannot start: {e:#}");
                }
            }
        }
    }

    /// Switch to `level`: stop services that no longer belong, drop their
    /// pending restarts and remember the new level so exits do not restart
    /// them. Starting the new members is a separate step so callers can wait
    /// for the old ones to exit first.
    pub fn set_level(&mut self, level: Level) {
        if level == self.level {
            return;
        }
        crate::info!(
            "svc: runlevel {self_level} -> {level}",
            self_level = self.level
        );
        self.level = level;
        let members = self.members();
        self.pending
            .retain(|restart| members.contains(&restart.idx));

        let mut stopping: Vec<(usize, Pid)> = self
            .running
            .iter()
            .filter(|(_, run)| !members.contains(&run.idx))
            .map(|(pid, run)| (run.idx, *pid))
            .collect();
        stopping.sort_by_key(|(idx, _)| std::cmp::Reverse(*idx));
        for (idx, pid) in stopping {
            crate::info!(
                "svc/{}: stopping for runlevel change (SIGTERM)",
                self.services[idx].name
            );
            signal_service(pid, Signal::SIGTERM);
        }
    }

    /// PIDs that are running but do not belong to the current runlevel.
    pub fn stopping_pids(&self) -> Vec<Pid> {
        let members = self.members();
        self.running
            .iter()
            .filter(|(_, run)| !members.contains(&run.idx))
            .map(|(pid, _)| *pid)
            .collect()
    }

    pub fn any_running(&self, pids: &[Pid]) -> bool {
        pids.iter().any(|pid| self.running.contains_key(pid))
    }

    /// SIGKILL the given service process groups.
    /// Spawn due restarts. Called from the main loop on every wakeup.
    pub fn tick(&mut self) {
        if self.stopping || self.pending.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut due = Vec::new();
        let mut waiting = Vec::new();
        for item in self.pending.drain(..) {
            if item.due <= now {
                due.push(item);
            } else {
                waiting.push(item);
            }
        }
        self.pending = waiting;
        for item in due {
            let name = self.services[item.idx].name.clone();
            if let Err(e) = self.spawn(item.idx, item.failures) {
                crate::err!("svc/{name}: cannot restart: {e:#}");
            }
        }
    }

    fn spawn(&mut self, idx: usize, failures: u32) -> Result<()> {
        let spec = self.services[idx].clone();
        let (read_fd, write_fd) = pipe_cloexec().context("pipe")?;
        set_nonblocking(&read_fd);

        match unsafe { fork() }.context("fork")? {
            ForkResult::Parent { child } => {
                drop(write_fd);
                self.logs.push(LogPipe {
                    fd: read_fd,
                    name: spec.name.clone(),
                    buf: Vec::new(),
                });
                self.running.insert(
                    child,
                    Running {
                        idx,
                        started: Instant::now(),
                        failures,
                    },
                );
                crate::info!("svc/{}: started (pid {child})", spec.name);
                Ok(())
            }
            ForkResult::Child => child_exec(&spec, write_fd),
        }
    }

    /// Handle a reaped child. Returns false if `pid` is not a supervised
    /// service (callers still log it).
    pub fn handle_exit(&mut self, pid: Pid, status: &WaitStatus) -> bool {
        let Some(run) = self.running.remove(&pid) else {
            return false;
        };
        let spec = &self.services[run.idx];
        let success = matches!(status, WaitStatus::Exited(_, 0));
        let uptime = run.started.elapsed();
        let mut failures = run.failures;
        if uptime >= STABLE_UPTIME {
            failures = 0;
        }

        crate::info!("svc/{}: {} after {:?}", spec.name, describe(status), uptime);

        // A service stopped because of a runlevel switch must not come back.
        if !self.members().contains(&run.idx) {
            return true;
        }

        if self.stopping || spec.oneshot {
            return true;
        }
        let restart = match spec.restart {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => !success,
            RestartPolicy::Never => false,
        };
        if !restart {
            return true;
        }

        failures += 1;
        if failures >= MAX_CONSECUTIVE_FAILURES {
            crate::warn!(
                "svc/{}: {failures} consecutive failures; giving up",
                spec.name
            );
            return true;
        }
        let delay = backoff_delay(failures);
        crate::warn!(
            "svc/{}: restarting in {} ms (attempt {failures})",
            spec.name,
            delay.as_millis()
        );
        self.pending.push(PendingRestart {
            idx: run.idx,
            due: Instant::now() + delay,
            failures,
        });
        true
    }

    /// Number of supervised services currently running.
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn log_count(&self) -> usize {
        self.logs.len()
    }

    pub fn log_fds(&self) -> Vec<BorrowedFd<'_>> {
        self.logs.iter().map(|p| p.fd.as_fd()).collect()
    }

    /// Forward buffered service output to the kernel log, line by line.
    pub fn drain_logs(&mut self) {
        let mut closed = Vec::new();
        for i in 0..self.logs.len() {
            let mut eof = false;
            {
                let pipe = &mut self.logs[i];
                let raw = pipe.fd.as_raw_fd();
                loop {
                    let mut chunk = [0u8; 4096];
                    let n = unsafe { libc::read(raw, chunk.as_mut_ptr().cast(), chunk.len()) };
                    if n > 0 {
                        pipe.buf.extend_from_slice(&chunk[..n as usize]);
                    } else if n == 0 {
                        eof = true;
                        break;
                    } else {
                        let err = std::io::Error::last_os_error();
                        if err.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        break;
                    }
                }
                while let Some(pos) = pipe.buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pipe.buf.drain(..=pos).collect();
                    emit_line(&pipe.name, &line[..line.len() - 1]);
                }
                if eof && !pipe.buf.is_empty() {
                    let line = std::mem::take(&mut pipe.buf);
                    emit_line(&pipe.name, &line);
                }
                // A service that never prints a newline must not grow our
                // memory without bound.
                if pipe.buf.len() > 64 * 1024 {
                    pipe.buf.truncate(64 * 1024);
                }
            }
            if eof {
                closed.push(i);
            }
        }
        for i in closed.into_iter().rev() {
            self.logs.remove(i);
        }
    }

    /// Ask every running service to stop, in reverse startup order.
    pub fn stop(&mut self) {
        self.stopping = true;
        self.pending.clear();

        let mut running: Vec<(usize, Pid)> = self
            .running
            .iter()
            .map(|(pid, run)| (run.idx, *pid))
            .collect();
        running.sort_by_key(|(idx, _)| std::cmp::Reverse(*idx));

        for (idx, pid) in running {
            let name = &self.services[idx].name;
            crate::info!("svc/{name}: stopping (SIGTERM)");
            signal_service(pid, Signal::SIGTERM);
        }
    }

    /// SIGKILL every service that is still running (shutdown fallback).
    pub fn kill_all(&mut self) {
        for pid in self.running.keys() {
            signal_service(*pid, Signal::SIGKILL);
        }
        self.pending.clear();
    }
}

/// Signal a service process and its process group. Before the child reaches
/// setsid its pid is not a group id, and after the leader exits the group can
/// still hold children, so both targets are needed.
fn signal_service(pid: Pid, signal: Signal) {
    let _ = kill(pid, signal);
    if pid.as_raw() > 1 {
        let _ = kill(Pid::from_raw(-pid.as_raw()), signal);
    }
}

fn backoff_delay(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(6);
    let ms = RESTART_BASE_MS.saturating_mul(1u64 << shift);
    Duration::from_millis(ms.min(RESTART_MAX_MS))
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

fn emit_line(name: &str, line: &[u8]) {
    crate::info!("svc/{name}: {}", String::from_utf8_lossy(line));
}

fn pipe_cloexec() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn set_nonblocking(fd: &OwnedFd) {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
}

/// Child side: detach from the console, wire stdin to /dev/null and
/// stdout/stderr to the log pipe, then exec. Never returns.
fn child_exec(spec: &ServiceSpec, log_fd: OwnedFd) -> ! {
    // Children must not inherit the signal mask blocked for PID 1. Note that
    // unblocking an empty set is a no-op, so replace the whole mask instead.
    clear_signal_mask();
    let _ = nix::unistd::setsid();

    let log_raw = log_fd.as_raw_fd();
    if let Ok(null) = nix::fcntl::open(
        "/dev/null",
        nix::fcntl::OFlag::O_RDONLY,
        nix::sys::stat::Mode::empty(),
    ) {
        let _ = unsafe { libc::dup2(null.as_raw_fd(), 0) };
    }
    let _ = unsafe { libc::dup2(log_raw, 1) };
    let _ = unsafe { libc::dup2(log_raw, 2) };
    // Keep the log fd alive across exec is not needed: 1 and 2 are the copies.
    std::mem::forget(log_fd);

    if let Some(dir) = &spec.working_dir {
        if let Err(e) = std::env::set_current_dir(dir) {
            child_fail(spec, &format!("cannot chdir to {dir}: {e}"));
        }
    }

    let path = match CString::new(spec.exec.as_bytes()) {
        Ok(path) => path,
        Err(_) => child_fail(spec, "exec path contains a NUL byte"),
    };
    let mut argv = vec![CString::new(spec.exec.as_bytes()).expect("checked above")];
    for arg in &spec.args {
        match CString::new(arg.as_bytes()) {
            Ok(arg) => argv.push(arg),
            Err(_) => child_fail(spec, "argument contains a NUL byte"),
        }
    }

    let env = build_env(spec);
    let error = nix::unistd::execve(&path, &argv, &env);
    child_fail(spec, &format!("cannot exec {}: {error:?}", spec.exec))
}

/// Clear the inherited signal mask (`SIG_SETMASK` with an empty set).
fn clear_signal_mask() {
    use nix::sys::signal::{sigprocmask, SigSet, SigmaskHow};
    let empty = SigSet::empty();
    let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&empty), None);
}

fn child_fail(spec: &ServiceSpec, message: &str) -> ! {
    let line = format!("cannot start service: {message}\n");
    let prefix = format!("svc/{}: ", spec.name);
    let _ = unsafe { libc::write(2, prefix.as_ptr().cast(), prefix.len()) };
    let _ = unsafe { libc::write(2, line.as_ptr().cast(), line.len()) };
    unsafe { libc::_exit(127) }
}

fn build_env(spec: &ServiceSpec) -> Vec<CString> {
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
    set(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    set("HOME", "/root");
    set("JIMINIT_SERVICE", &spec.name);
    for (key, value) in &spec.env {
        set(key, value);
    }

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
    use super::*;

    fn spec(name: &str, deps: &[&str]) -> ServiceSpec {
        ServiceSpec {
            name: name.to_string(),
            exec: "/bin/true".to_string(),
            args: Vec::new(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            restart: RestartPolicy::OnFailure,
            oneshot: false,
            working_dir: None,
            env: BTreeMap::new(),
            runlevels: vec![Level::MultiUser, Level::Graphical],
        }
    }

    // The `index` helper is only used to inspect orderings in tests.
    fn index(services: &[ServiceSpec], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| services[i].name.clone()).collect()
    }

    #[test]
    fn parses_defaults() {
        let svc = parse_service("demo", "exec = \"/usr/bin/demo\"\n").unwrap();
        assert_eq!(svc.name, "demo");
        assert_eq!(svc.exec, "/usr/bin/demo");
        assert!(svc.args.is_empty());
        assert_eq!(svc.restart, RestartPolicy::OnFailure);
        assert!(!svc.oneshot);
    }

    #[test]
    fn parses_kebab_restart_policy_and_args() {
        let text = r#"
exec = "/bin/sh"
args = ["-c", "echo hi"]
restart = "always"
depends_on = ["a"]
env = { FOO = "bar" }
oneshot = true
"#;
        let svc = parse_service("demo", text).unwrap();
        assert_eq!(svc.restart, RestartPolicy::Always);
        assert_eq!(svc.args, ["-c", "echo hi"]);
        assert_eq!(svc.depends_on, ["a"]);
        assert!(svc.oneshot);
        assert_eq!(svc.env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn splits_exec_when_no_args_are_given() {
        let svc = parse_service("demo", "exec = \"/bin/sh -c true\"\n").unwrap();
        assert_eq!(svc.exec, "/bin/sh");
        assert_eq!(svc.args, ["-c", "true"]);
    }

    #[test]
    fn exec_split_supports_quotes_and_escapes() {
        assert_eq!(
            split_command("/bin/sh -c 'echo hello world'").unwrap(),
            ["/bin/sh", "-c", "echo hello world"]
        );
        assert_eq!(
            split_command("echo hello\\ world").unwrap(),
            ["echo", "hello world"]
        );
        assert_eq!(split_command(r#""a b" c"#).unwrap(), ["a b", "c"]);
    }

    #[test]
    fn rejects_unterminated_quotes() {
        assert!(split_command("/bin/sh -c 'oops").is_err());
        assert!(parse_service("demo", "exec = \"/bin/sh -c 'oops\"\n").is_err());
    }

    #[test]
    fn rejects_missing_exec() {
        assert!(parse_service("demo", "restart = \"never\"\n").is_err());
    }

    #[test]
    fn orders_dependencies_before_dependents() {
        let services = vec![spec("web", &["db"]), spec("db", &[]), spec("init", &[])];
        let (order, cyclic) = startup_order(&services);
        assert!(cyclic.is_empty());
        let names = index(&services, &order);
        let pos = |n: &str| names.iter().position(|x| x == n).unwrap();
        assert!(pos("db") < pos("web"));
        assert!(pos("init") < pos("web"));
    }

    #[test]
    fn detects_cycles() {
        let services = vec![spec("a", &["b"]), spec("b", &["a"])];
        let (order, cyclic) = startup_order(&services);
        assert!(order.is_empty());
        let mut cyclic = cyclic;
        cyclic.sort();
        assert_eq!(cyclic, ["a", "b"]);
    }

    #[test]
    fn runlevels_default_to_multi_user_and_graphical() {
        let svc = parse_service("demo", "exec = \"/bin/true\"\n").unwrap();
        assert_eq!(svc.runlevels, vec![Level::MultiUser, Level::Graphical]);
    }

    #[test]
    fn runlevels_accept_names_and_numeric_aliases() {
        let text = "exec = \"/bin/true\"\nrunlevels = [\"single\", \"5\"]\n";
        let svc = parse_service("demo", text).unwrap();
        assert_eq!(svc.runlevels, vec![Level::Single, Level::Graphical]);
    }

    #[test]
    fn rejects_unknown_runlevel_tag() {
        let text = "exec = \"/bin/true\"\nrunlevels = [\"graphics\"]\n";
        assert!(parse_service("demo", text).is_err());
    }

    #[test]
    fn single_level_only_keeps_tagged_services_and_their_deps() {
        let mut services = vec![spec("web", &["db"]), spec("db", &[]), spec("console", &[])];
        services[2].runlevels = vec![Level::Single];
        let single = members_of(&services, Level::Single);
        assert_eq!(single, BTreeSet::from([2]));
        let graphical = members_of(&services, Level::Graphical);
        // `console` was retagged to single only, so it is not a member here.
        assert_eq!(graphical, BTreeSet::from([0, 1]));
    }

    #[test]
    fn dependencies_are_pulled_into_the_level() {
        let mut services = vec![spec("web", &["db"]), spec("db", &[])];
        services[0].runlevels = vec![Level::Single];
        services[1].runlevels = vec![Level::MultiUser];
        let single = members_of(&services, Level::Single);
        assert_eq!(single, BTreeSet::from([0, 1]));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(1), Duration::from_millis(1_000));
        assert_eq!(backoff_delay(2), Duration::from_millis(2_000));
        assert_eq!(backoff_delay(3), Duration::from_millis(4_000));
        assert_eq!(backoff_delay(7), Duration::from_millis(60_000));
        assert_eq!(backoff_delay(30), Duration::from_millis(60_000));
    }
}
