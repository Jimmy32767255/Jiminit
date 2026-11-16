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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
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

/// Resource limits applied through the service cgroup (v2 syntax).
#[derive(Debug, Clone, Default)]
pub struct Limits {
    /// `memory.max`: bytes, optionally with a K/M/G/T suffix, or `max`.
    pub memory_max: Option<String>,
    /// `pids.max`: maximum number of tasks.
    pub pids_max: Option<u64>,
    /// `cpu.max`: `<quota> <period>` in microseconds.
    pub cpu_max: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LimitsFile {
    memory_max: Option<String>,
    pids_max: Option<u64>,
    cpu_max: Option<String>,
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
    /// Run as this user (name or numeric uid). `None` keeps root.
    pub user: Option<String>,
    /// Override the primary group (name or numeric gid).
    pub group: Option<String>,
    /// Supplementary groups; empty means "groups the user belongs to".
    pub supplementary_groups: Vec<String>,
    /// Wait for `READY=1` on the sd_notify socket before starting dependents.
    pub notify: bool,
    /// The service must send `WATCHDOG=1` on its notify socket this often.
    pub watchdog: Option<Duration>,
    /// cgroup v2 resource limits (empty means unlimited).
    pub limits: Limits,
    /// Append stdout/stderr here instead of forwarding to the init log.
    pub log_file: Option<String>,
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
    user: Option<String>,
    group: Option<String>,
    supplementary_groups: Vec<String>,
    #[serde(default)]
    notify: bool,
    /// Human-readable watchdog interval, e.g. `30s` or `500ms`.
    watchdog: Option<String>,
    #[serde(default)]
    limits: LimitsFile,
    /// Per-service append log file (1 MiB rotation at spawn).
    log_file: Option<String>,
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
        user: file.user,
        group: file.group,
        supplementary_groups: file.supplementary_groups,
        notify: file.notify,
        watchdog: parse_watchdog(name, file.watchdog.as_deref())?,
        limits: parse_limits(name, file.limits)?,
        log_file: file.log_file.filter(|path| !path.trim().is_empty()),
    })
}

fn parse_limits(name: &str, file: LimitsFile) -> Result<Limits> {
    let memory_max = file.memory_max.map(|v| v.trim().to_string());
    if let Some(value) = &memory_max {
        if !valid_memory_max(value) {
            bail!(
                "service `{name}` has an invalid `limits.memory_max` value `{value}`: \
                 expected bytes with an optional K/M/G/T suffix, or `max`"
            );
        }
    }
    let cpu_max = file.cpu_max.map(|v| v.trim().to_string());
    if let Some(value) = &cpu_max {
        let parts: Vec<&str> = value.split_whitespace().collect();
        let valid = parts.len() == 2
            && parts.iter().all(|part| {
                part.parse::<u64>()
                    .map(|number| number > 0)
                    .unwrap_or(false)
            });
        if !valid {
            bail!(
                "service `{name}` has an invalid `limits.cpu_max` value `{value}`: \
                 expected \"<quota> <period>\" in microseconds"
            );
        }
    }
    Ok(Limits {
        memory_max,
        pids_max: file.pids_max,
        cpu_max,
    })
}

fn parse_watchdog(name: &str, value: Option<&str>) -> Result<Option<Duration>> {
    let Some(text) = value else {
        return Ok(None);
    };
    match parse_duration(text) {
        Some(duration) => Ok(Some(duration)),
        None => bail!(
            "service `{name}` has an invalid `watchdog` value `{text}`: \
             expected a positive interval such as `500ms`, `30s` or `5m`"
        ),
    }
}

/// Parse `500ms`, `30s`, `5m`, `1h`; a bare number means seconds.
fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (number, unit_ms) = if let Some(value) = text.strip_suffix("ms") {
        (value, 1u64)
    } else if let Some(value) = text.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = text.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = text.strip_suffix('h') {
        (value, 3_600_000)
    } else if let Some(value) = text.strip_suffix('d') {
        (value, 86_400_000)
    } else {
        (text, 1_000)
    };
    let number: u64 = number.trim().parse().ok()?;
    if number == 0 {
        return None;
    }
    Some(Duration::from_millis(number.checked_mul(unit_ms)?))
}

fn valid_memory_max(value: &str) -> bool {
    if value == "max" {
        return true;
    }
    let digits = value.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &value[digits.len()..];
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && matches!(suffix, "" | "k" | "K" | "m" | "M" | "g" | "G" | "t" | "T")
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

pub(crate) fn load_dir(dir: &Path) -> Result<Vec<ServiceSpec>> {
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
                Err(e) => crate::error!("svc/{name}: {e:#}; skipping"),
            },
            Err(e) => crate::error!("svc/{name}: cannot read {}: {e}", path.display()),
        }
    }
    Ok(services)
}

/// Dependency order, computed in waves: every service whose dependencies are
/// already launched can start together. Returns `(order, cyclic)` where
/// `cyclic` lists services skipped because of a dependency cycle.
pub(crate) fn startup_order(services: &[ServiceSpec]) -> (Vec<usize>, Vec<String>) {
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

/// Indented dependency overview for `check`: one line per service, deeper
/// dependencies indented by two spaces, with the direct dependencies noted.
pub(crate) fn tree_lines(services: &[ServiceSpec]) -> Vec<String> {
    let (order, _) = startup_order(services);
    let mut depth = vec![0usize; services.len()];
    for idx in order {
        depth[idx] = services[idx]
            .depends_on
            .iter()
            .filter_map(|name| services.iter().position(|spec| &spec.name == name))
            .map(|dep| depth[dep] + 1)
            .max()
            .unwrap_or(0)
            .min(services.len());
    }
    let mut rows: Vec<(usize, &str)> = (0..services.len())
        .map(|idx| (depth[idx], services[idx].name.as_str()))
        .collect();
    rows.sort();
    rows.into_iter()
        .map(|(indent, name)| {
            let spec = services.iter().find(|spec| spec.name == name).unwrap();
            let mut line = format!("{}{}", "  ".repeat(indent), name);
            if !spec.depends_on.is_empty() {
                line.push_str(&format!("  (after {})", spec.depends_on.join(", ")));
            }
            line
        })
        .collect()
}

/// Graphviz DOT description of the dependency graph for `graph`.
pub(crate) fn dot_graph(services: &[ServiceSpec]) -> String {
    let mut out = String::from("digraph jiminit {\n    rankdir=LR;\n    node [shape=box];\n");
    for spec in services {
        out.push_str(&format!("    \"{}\";\n", spec.name));
    }
    for spec in services {
        for dep in &spec.depends_on {
            if services.iter().any(|candidate| &candidate.name == dep) {
                out.push_str(&format!("    \"{dep}\" -> \"{}\";\n", spec.name));
            }
        }
    }
    out.push_str("}\n");
    out
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
    /// False until the service sends READY=1 (notify services only).
    ready: bool,
    /// When we stop waiting for READY=1 and treat the service as ready.
    ready_deadline: Option<Instant>,
    /// Last `memory.events` oom_kill count we reported.
    oom_seen: u64,
    /// When the next WATCHDOG=1 must arrive, if the service has a watchdog.
    watchdog_deadline: Option<Instant>,
}

struct NotifyPipe {
    fd: UnixDatagram,
    name: String,
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
    /// How often each service has been started (first start included).
    starts: Vec<u32>,
    services: Vec<ServiceSpec>,
    running: HashMap<Pid, Running>,
    pending: Vec<PendingRestart>,
    /// Members that have not been spawned yet, in startup order. A service is
    /// only spawned once all of its dependencies are ready.
    pending_start: Vec<usize>,
    logs: Vec<LogPipe>,
    notify: Vec<NotifyPipe>,
    /// Services stopped on request; their exit must not trigger a restart.
    manual_stop: HashSet<usize>,
    /// Services that ended for good (or were stopped because a dependency
    /// did). Dependents must not start while a dependency is in this set.
    failed: HashSet<usize>,
    /// Oneshot services that completed successfully.
    completed: HashSet<usize>,
    /// Services that should start again once their stop completes.
    restart_after: HashSet<usize>,
    ready_timeout: Duration,
    stopping: bool,
    level: Level,
    /// Runtime directory for notify sockets (default `/run/jiminit`; a user
    /// instance points it at `$XDG_RUNTIME_DIR/jiminit`).
    run_dir: PathBuf,
}

impl Supervisor {
    /// Load services from `dir`. Broken definitions and cycles are logged and
    /// skipped; an unusable directory simply yields no services.
    pub fn new(dir: &Path, level: Level) -> Self {
        let mut services = match load_dir(dir) {
            Ok(services) => services,
            Err(e) => {
                crate::error!("svc: {e:#}");
                Vec::new()
            }
        };

        let known: BTreeSet<String> = services.iter().map(|s| s.name.clone()).collect();
        services.retain(|spec| {
            if let Some(missing) = spec.depends_on.iter().find(|d| !known.contains(*d)) {
                crate::error!(
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
            crate::error!("svc/{name}: dependency cycle; skipping");
        }
        // Keep only services that can be ordered, arranged in startup order so
        // that `stop()` can simply walk the vector backwards.
        services = order.iter().map(|&i| services[i].clone()).collect();

        // Controllers are enabled only when a service actually asks for the
        // corresponding limit; this keeps the rest of the system untouched.
        let mut controllers = Vec::new();
        if services.iter().any(|s| s.limits.memory_max.is_some()) {
            controllers.push("memory");
        }
        if services.iter().any(|s| s.limits.pids_max.is_some()) {
            controllers.push("pids");
        }
        if services.iter().any(|s| s.limits.cpu_max.is_some()) {
            controllers.push("cpu");
        }
        if let Err(e) = crate::cgroup::enable_controllers(&controllers) {
            crate::error!("cgroup: {e:#}");
        }

        crate::info!(
            "svc: loaded {} service(s) from {}",
            services.len(),
            dir.display()
        );

        Self {
            starts: vec![0; services.len()],
            services,
            running: HashMap::new(),
            pending: Vec::new(),
            pending_start: Vec::new(),
            logs: Vec::new(),
            notify: Vec::new(),
            manual_stop: HashSet::new(),
            failed: HashSet::new(),
            completed: HashSet::new(),
            restart_after: HashSet::new(),
            ready_timeout: Duration::from_millis(5_000),
            stopping: false,
            level,
            run_dir: PathBuf::from("/run/jiminit"),
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

    pub fn set_ready_timeout(&mut self, timeout: Duration) {
        self.ready_timeout = timeout;
    }

    /// Where sd_notify sockets live; a user instance keeps them in its own
    /// `$XDG_RUNTIME_DIR` instead of `/run/jiminit`.
    pub fn set_run_dir(&mut self, dir: PathBuf) {
        self.run_dir = dir;
    }

    /// Queue every member of the current runlevel that is not running yet.
    /// `tick()` spawns them once their dependencies are ready.
    pub fn start_members(&mut self) {
        let members = self.members();
        self.pending_start.clear();
        for idx in 0..self.services.len() {
            if members.contains(&idx) && !self.is_running(idx) {
                self.pending_start.push(idx);
            }
        }
    }

    fn service_ready(&self, idx: usize) -> bool {
        if self.failed.contains(&idx) {
            return false;
        }
        if self.services[idx].oneshot {
            // A oneshot dependency is "ready" once it has run (its exit is
            // the completion signal), so do not deadlock waiting for READY.
            return !self.is_running(idx) && !self.pending_start.contains(&idx);
        }
        self.running.values().any(|run| run.idx == idx && run.ready)
    }

    fn deps_ready(&self, idx: usize) -> bool {
        self.services[idx].depends_on.iter().all(|dep| {
            self.services
                .iter()
                .position(|spec| &spec.name == dep)
                .map(|dep_idx| self.service_ready(dep_idx))
                .unwrap_or(true)
        })
    }

    /// First dependency that has failed for good, if any.
    fn failed_dependency(&self, idx: usize) -> Option<String> {
        self.services[idx].depends_on.iter().find_map(|dep| {
            let dep_idx = self.services.iter().position(|spec| &spec.name == dep)?;
            self.failed.contains(&dep_idx).then(|| dep.clone())
        })
    }

    /// Mark a service as failed for good and stop everything that depends on
    /// it (transitively). Dependents are not restarted automatically.
    fn mark_failed(&mut self, idx: usize) {
        if !self.failed.insert(idx) {
            return;
        }
        let name = self.services[idx].name.clone();
        self.stop_dependents(&name, "failed");
    }

    fn stop_dependents(&mut self, parent_name: &str, reason: &str) {
        let mut stack = vec![parent_name.to_string()];
        let mut seen = HashSet::new();
        while let Some(parent_name) = stack.pop() {
            let targets: Vec<usize> = self
                .services
                .iter()
                .enumerate()
                .filter(|(i, spec)| {
                    !seen.contains(i) && spec.depends_on.iter().any(|d| d == &parent_name)
                })
                .map(|(i, _)| i)
                .collect();
            for target in targets {
                seen.insert(target);
                let target_name = self.services[target].name.clone();
                self.failed.insert(target);
                self.pending_start.retain(|i| *i != target);
                self.pending.retain(|item| item.idx != target);
                if let Some(pid) = self.pid_for(target) {
                    crate::warn!(
                        "svc/{target_name}: stopping because dependency `{parent_name}` {reason}"
                    );
                    self.manual_stop.insert(target);
                    signal_service(pid, Signal::SIGTERM);
                } else {
                    crate::warn!(
                        "svc/{target_name}: not started: dependency `{parent_name}` {reason}"
                    );
                }
                stack.push(target_name);
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
        self.pending_start.retain(|idx| members.contains(idx));

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
    /// Start due restarts and services whose dependencies became ready.
    /// Called from the main loop on every wakeup.
    /// One human-readable line per service for `jiminit status`.
    pub fn status_lines(&self) -> Vec<String> {
        (0..self.services.len())
            .map(|idx| self.status_line(idx))
            .collect()
    }

    /// The status line of one service, for `jiminit status <name>`.
    pub fn status_line_for(&self, name: &str) -> Result<String> {
        let idx = self
            .services
            .iter()
            .position(|spec| spec.name == name)
            .with_context(|| format!("unknown service `{name}`"))?;
        Ok(self.status_line(idx))
    }

    /// Configured service names for `jiminit list`.
    pub fn list_lines(&self) -> Vec<String> {
        self.services.iter().map(|spec| spec.name.clone()).collect()
    }

    fn status_line(&self, idx: usize) -> String {
        let spec = &self.services[idx];
        let running = self.running.iter().find(|(_, run)| run.idx == idx);
        let restarts = self.starts[idx].saturating_sub(1);
        let line = match running {
            Some((pid, run)) => {
                let stats = crate::cgroup::stats(&spec.name);
                format!(
                    "{} running pid={} restarts={restarts} ready={} memory={} cpu={} peak={} pids={} io={}",
                    spec.name,
                    pid.as_raw(),
                    if run.ready { "yes" } else { "no" },
                    stats.memory.map(human_bytes).unwrap_or_else(|| "-".into()),
                    stats
                        .cpu_usec
                        .map(human_duration)
                        .unwrap_or_else(|| "-".into()),
                    stats
                        .memory_peak
                        .map(human_bytes)
                        .unwrap_or_else(|| "-".into()),
                    stats
                        .pids
                        .map(|pids| pids.to_string())
                        .unwrap_or_else(|| "-".into()),
                    match (stats.io_read, stats.io_write) {
                        (Some(read), Some(write)) => format!(
                            "{}/{}",
                            human_bytes_zero(read),
                            human_bytes_zero(write)
                        ),
                        _ => "-".to_string(),
                    }
                )
            }
            None => format!(
                "{} stopped pid=- restarts={restarts} ready=- memory=- cpu=- peak=- pids=- io=-",
                spec.name
            ),
        };
        line
    }

    /// Prometheus text exposition for all services plus the current runlevel.
    pub fn metrics(&self, runlevel: &str) -> String {
        let mut out = String::new();
        out.push_str("# HELP jiminit_build_info Build information for this init.\n");
        out.push_str("# TYPE jiminit_build_info gauge\n");
        out.push_str(&format!(
            "jiminit_build_info{{version=\"{}\"}} 1\n",
            escape_label(env!("CARGO_PKG_VERSION"))
        ));
        out.push_str("# HELP jiminit_runlevel_info Runlevel selected at boot or by switch.\n");
        out.push_str("# TYPE jiminit_runlevel_info gauge\n");
        out.push_str(&format!(
            "jiminit_runlevel_info{{runlevel=\"{}\"}} 1\n",
            escape_label(runlevel)
        ));
        out.push_str("# HELP jiminit_service_info Service state, one series per service.\n");
        out.push_str("# TYPE jiminit_service_info gauge\n");
        out.push_str("# HELP jiminit_service_restarts_total Restarts since stage 2 started.\n");
        out.push_str("# TYPE jiminit_service_restarts_total counter\n");
        out.push_str(
            "# HELP jiminit_service_cpu_seconds_total CPU time used by the service cgroup.\n",
        );
        out.push_str("# TYPE jiminit_service_cpu_seconds_total counter\n");
        out.push_str("# HELP jiminit_service_memory_bytes memory.current of the service cgroup.\n");
        out.push_str("# TYPE jiminit_service_memory_bytes gauge\n");
        out.push_str(
            "# HELP jiminit_service_memory_peak_bytes memory.peak of the service cgroup.\n",
        );
        out.push_str("# TYPE jiminit_service_memory_peak_bytes gauge\n");
        out.push_str("# HELP jiminit_service_pids Number of tasks in the service cgroup.\n");
        out.push_str("# TYPE jiminit_service_pids gauge\n");
        out.push_str(
            "# HELP jiminit_service_io_read_bytes_total Bytes read, summed over devices.\n",
        );
        out.push_str("# TYPE jiminit_service_io_read_bytes_total counter\n");
        out.push_str(
            "# HELP jiminit_service_io_write_bytes_total Bytes written, summed over devices.\n",
        );
        out.push_str("# TYPE jiminit_service_io_write_bytes_total counter\n");

        let mut order: Vec<usize> = (0..self.services.len()).collect();
        order.sort_by(|a, b| self.services[*a].name.cmp(&self.services[*b].name));
        for idx in order {
            let spec = &self.services[idx];
            let name = escape_label(&spec.name);
            let running = self.running.iter().find(|(_, run)| run.idx == idx);
            let restarts = self.starts[idx].saturating_sub(1);
            match running {
                Some((_, run)) => {
                    out.push_str(&format!(
                        "jiminit_service_info{{name=\"{name}\",state=\"running\",ready=\"{}\"}} 1\n",
                        if run.ready { "yes" } else { "no" }
                    ));
                    out.push_str(&format!(
                        "jiminit_service_restarts_total{{name=\"{name}\"}} {restarts}\n"
                    ));
                    let stats = crate::cgroup::stats(&spec.name);
                    if let Some(usec) = stats.cpu_usec {
                        out.push_str(&format!(
                            "jiminit_service_cpu_seconds_total{{name=\"{name}\"}} {:.6}\n",
                            usec as f64 / 1_000_000.0
                        ));
                    }
                    if let Some(bytes) = stats.memory {
                        out.push_str(&format!(
                            "jiminit_service_memory_bytes{{name=\"{name}\"}} {bytes}\n"
                        ));
                    }
                    if let Some(bytes) = stats.memory_peak {
                        out.push_str(&format!(
                            "jiminit_service_memory_peak_bytes{{name=\"{name}\"}} {bytes}\n"
                        ));
                    }
                    if let Some(pids) = stats.pids {
                        out.push_str(&format!("jiminit_service_pids{{name=\"{name}\"}} {pids}\n"));
                    }
                    if let Some(bytes) = stats.io_read {
                        out.push_str(&format!(
                            "jiminit_service_io_read_bytes_total{{name=\"{name}\"}} {bytes}\n"
                        ));
                    }
                    if let Some(bytes) = stats.io_write {
                        out.push_str(&format!(
                            "jiminit_service_io_write_bytes_total{{name=\"{name}\"}} {bytes}\n"
                        ));
                    }
                }
                None => {
                    out.push_str(&format!(
                        "jiminit_service_info{{name=\"{name}\",state=\"stopped\",ready=\"no\"}} 1\n"
                    ));
                    out.push_str(&format!(
                        "jiminit_service_restarts_total{{name=\"{name}\"}} {restarts}\n"
                    ));
                }
            }
        }
        out
    }

    pub fn tick(&mut self) {
        if self.stopping {
            return;
        }
        let now = Instant::now();

        // Drop cgroup directories whose processes are gone; remove_dir fails
        // harmlessly while the service still has processes.
        for idx in 0..self.services.len() {
            if !self.is_running(idx) {
                crate::cgroup::remove(&self.services[idx].name);
            }
        }

        // A service that never sends READY=1 must not block its dependents.
        let mut timed_out = Vec::new();
        for run in self.running.values_mut() {
            if !run.ready {
                if let Some(deadline) = run.ready_deadline {
                    if now >= deadline {
                        run.ready = true;
                        run.ready_deadline = None;
                        timed_out.push(run.idx);
                    }
                }
            }
        }
        for idx in timed_out {
            crate::warn!(
                "svc/{}: no READY=1 within {} ms; assuming ready",
                self.services[idx].name,
                self.ready_timeout.as_millis()
            );
        }

        // A watchdog service that stops reporting is treated as failed: kill
        // it and let the normal exit/restart path decide what happens next.
        let mut expired = Vec::new();
        for (pid, run) in &self.running {
            if let Some(deadline) = run.watchdog_deadline {
                if now >= deadline {
                    expired.push((*pid, run.idx));
                }
            }
        }
        for (pid, idx) in expired {
            if let Some(run) = self.running.get_mut(&pid) {
                run.watchdog_deadline = None;
            }
            crate::error!(
                "svc/{}: watchdog timeout (pid {pid}); killing",
                self.services[idx].name
            );
            signal_service(pid, Signal::SIGKILL);
        }

        // Report OOM kills recorded in the service cgroup. The exit itself is
        // handled by the normal restart policy.
        let mut oom = Vec::new();
        for run in self.running.values() {
            let spec = &self.services[run.idx];
            if spec.limits.memory_max.is_none() {
                continue;
            }
            let count = crate::cgroup::oom_kill_count(&spec.name);
            if count > run.oom_seen {
                oom.push((run.idx, count));
            }
        }
        for (idx, count) in oom {
            if let Some(run) = self.running.values_mut().find(|run| run.idx == idx) {
                run.oom_seen = count;
            }
            crate::error!(
                "svc/{}: OOM-killed ({} total)",
                self.services[idx].name,
                count
            );
        }

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
                crate::fail!("svc/{name}: cannot restart: {e:#}");
            }
        }

        // Spawn queued members in startup order. A dependency in the same
        // queue is spawned first, so `deps_ready` sees it as running.
        let queued = std::mem::take(&mut self.pending_start);
        let mut blocked = Vec::new();
        for idx in queued {
            if self.is_running(idx) {
                continue;
            }
            let name = self.services[idx].name.clone();
            if let Some(dep) = self.failed_dependency(idx) {
                crate::warn!("svc/{name}: not started: dependency `{dep}` failed");
                self.failed.insert(idx);
                continue;
            }
            if !self.deps_ready(idx) {
                blocked.push(idx);
                continue;
            }
            if let Err(e) = self.spawn(idx, 0) {
                crate::fail!("svc/{name}: cannot start: {e:#}");
                self.mark_failed(idx);
            }
        }
        self.pending_start = blocked;
    }

    /// File descriptors of the sd_notify sockets, for `poll()`.
    pub fn notify_fds(&self) -> Vec<BorrowedFd<'_>> {
        self.notify.iter().map(|p| p.fd.as_fd()).collect()
    }

    /// Read pending sd_notify messages and mark services ready.
    pub fn drain_notify(&mut self) {
        let mut ready = Vec::new();
        let mut status = Vec::new();
        let mut watchdog = Vec::new();
        for pipe in &self.notify {
            loop {
                let mut buf = [0u8; 1024];
                match pipe.fd.recv(&mut buf) {
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        for line in text.lines() {
                            let line = line.trim();
                            if line == "READY=1" {
                                ready.push(pipe.name.clone());
                            } else if line == "WATCHDOG=1" {
                                watchdog.push(pipe.name.clone());
                            } else if let Some(value) = line.strip_prefix("STATUS=") {
                                status.push((pipe.name.clone(), value.to_string()));
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        crate::error!("svc/{}: notify socket: {e}", pipe.name);
                        break;
                    }
                }
            }
        }
        for name in ready {
            let Some(idx) = self.services.iter().position(|s| s.name == name) else {
                continue;
            };
            let mut newly = false;
            for run in self.running.values_mut() {
                if run.idx == idx && !run.ready {
                    run.ready = true;
                    run.ready_deadline = None;
                    newly = true;
                }
            }
            if newly {
                crate::succ!("svc/{name}: ready");
            }
        }
        for (name, value) in status {
            crate::info!("svc/{name}: status: {value}");
        }
        for name in watchdog {
            let Some(idx) = self.services.iter().position(|s| s.name == name) else {
                continue;
            };
            let Some(interval) = self.services[idx].watchdog else {
                continue;
            };
            for run in self.running.values_mut() {
                if run.idx == idx {
                    run.watchdog_deadline = Some(Instant::now() + interval);
                }
            }
        }
    }

    /// Runtime control from the control socket.
    pub fn control(&mut self, name: &str, action: &str) -> Result<String> {
        let Some(idx) = self.services.iter().position(|spec| spec.name == name) else {
            bail!("unknown service `{name}`");
        };
        if !self.members().contains(&idx) {
            bail!(
                "service `{name}` does not belong to runlevel {}",
                self.level
            );
        }
        match action {
            "start" => {
                if self.is_running(idx) {
                    bail!("service `{name}` is already running");
                }
                if let Some(dep) = self.failed_dependency(idx) {
                    bail!("service `{name}` cannot start: dependency `{dep}` failed");
                }
                self.pending_start.retain(|i| *i != idx);
                self.pending.retain(|item| item.idx != idx);
                self.spawn(idx, 0)
                    .with_context(|| format!("cannot start service `{name}`"))?;
                Ok(format!("ok {name} started"))
            }
            "stop" => {
                let Some(pid) = self.pid_for(idx) else {
                    return Ok(format!("ok {name} not running"));
                };
                self.manual_stop.insert(idx);
                self.restart_after.remove(&idx);
                crate::info!("svc/{name}: stopping on request (SIGTERM)");
                signal_service(pid, Signal::SIGTERM);
                self.failed.insert(idx);
                self.stop_dependents(name, "was stopped");
                Ok(format!("ok {name} stopping"))
            }
            "restart" => {
                let Some(pid) = self.pid_for(idx) else {
                    self.pending_start.retain(|i| *i != idx);
                    self.spawn(idx, 0)
                        .with_context(|| format!("cannot start service `{name}`"))?;
                    return Ok(format!("ok {name} started"));
                };
                self.manual_stop.insert(idx);
                self.restart_after.insert(idx);
                crate::info!("svc/{name}: restarting on request (SIGTERM)");
                signal_service(pid, Signal::SIGTERM);
                Ok(format!("ok {name} restarting"))
            }
            "reload" => {
                let Some(pid) = self.pid_for(idx) else {
                    bail!("service `{name}` is not running");
                };
                crate::info!("svc/{name}: reloading on request (SIGHUP)");
                signal_service(pid, Signal::SIGHUP);
                Ok(format!("ok {name} reloaded"))
            }
            other => bail!("unknown service action `{other}` (use start, stop, restart or reload)"),
        }
    }

    fn pid_for(&self, idx: usize) -> Option<Pid> {
        self.running
            .iter()
            .find(|(_, run)| run.idx == idx)
            .map(|(pid, _)| *pid)
    }

    fn notify_socket(&mut self, name: &str) -> Result<String> {
        let dir = self.run_dir.join("notify");
        let dir = dir.as_path();
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{}.sock", name.replace('/', "_")));
        let _ = std::fs::remove_file(&path);
        let socket = UnixDatagram::bind(&path)
            .with_context(|| format!("cannot bind notify socket {}", path.display()))?;
        socket.set_nonblocking(true)?;
        // Services may run as unprivileged users and still have to report.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
        self.notify.retain(|pipe| pipe.name != name);
        self.notify.push(NotifyPipe {
            fd: socket,
            name: name.to_string(),
        });
        Ok(path.display().to_string())
    }

    fn spawn(&mut self, idx: usize, failures: u32) -> Result<()> {
        let spec = self.services[idx].clone();

        let credentials = match spec.user.as_deref() {
            Some(user) => Some(
                crate::accounts::resolve(
                    user,
                    spec.group.as_deref(),
                    &spec.supplementary_groups,
                    Path::new(crate::accounts::PASSWD),
                    Path::new(crate::accounts::GROUP),
                )
                .with_context(|| {
                    format!("cannot resolve user `{user}` for service `{}`", spec.name)
                })?,
            ),
            None => None,
        };
        let notify_path = if spec.notify || spec.watchdog.is_some() {
            Some(self.notify_socket(&spec.name)?)
        } else {
            None
        };
        // Create the cgroup and write limits before fork, so even a fast
        // child cannot allocate beyond them.
        crate::cgroup::configure(
            &spec.name,
            spec.limits.memory_max.as_deref(),
            spec.limits.pids_max,
            spec.limits.cpu_max.as_deref(),
        );
        let oom_seen = crate::cgroup::oom_kill_count(&spec.name);

        let (read_fd, write_fd) = match spec.log_file.as_deref() {
            Some(path) => match open_service_log(path) {
                Ok(fd) => (None, fd),
                Err(e) => {
                    crate::warn!(
                        "svc/{}: cannot open log file {path}: {e}; using the init log",
                        spec.name
                    );
                    let (read_fd, write_fd) = pipe_cloexec().context("pipe")?;
                    set_nonblocking(&read_fd);
                    (Some(read_fd), write_fd)
                }
            },
            None => {
                let (read_fd, write_fd) = pipe_cloexec().context("pipe")?;
                set_nonblocking(&read_fd);
                (Some(read_fd), write_fd)
            }
        };

        self.starts[idx] += 1;
        self.failed.remove(&idx);
        self.completed.remove(&idx);
        match unsafe { fork() }.context("fork")? {
            ForkResult::Parent { child } => {
                drop(write_fd);
                if let Some(read_fd) = read_fd {
                    self.logs.push(LogPipe {
                        fd: read_fd,
                        name: spec.name.clone(),
                        buf: Vec::new(),
                    });
                }
                let ready_deadline = if spec.notify {
                    Some(Instant::now() + self.ready_timeout)
                } else {
                    None
                };
                self.running.insert(
                    child,
                    Running {
                        idx,
                        started: Instant::now(),
                        failures,
                        ready: !spec.notify,
                        ready_deadline,
                        oom_seen,
                        watchdog_deadline: spec.watchdog.map(|d| Instant::now() + d),
                    },
                );
                crate::succ!("svc/{}: started (pid {child})", spec.name);
                if let Some(path) = &notify_path {
                    if spec.notify {
                        crate::log::wait_notice(
                            &format!("svc/{}: waiting for READY=1 on {path}", spec.name),
                            Some(self.ready_timeout),
                        );
                    }
                    if let Some(watchdog) = spec.watchdog {
                        crate::info!(
                            "svc/{}: watchdog every {} ms on {path}",
                            spec.name,
                            watchdog.as_millis()
                        );
                    }
                }
                Ok(())
            }
            ForkResult::Child => child_exec(&spec, write_fd, credentials, notify_path),
        }
    }

    /// Handle a reaped child. Returns false if `pid` is not a supervised
    /// service (callers still log it).
    pub fn handle_exit(&mut self, pid: Pid, status: &WaitStatus) -> bool {
        let Some(run) = self.running.remove(&pid) else {
            return false;
        };
        let idx = run.idx;
        let name = self.services[idx].name.clone();
        let spec = self.services[idx].clone();
        let success = matches!(status, WaitStatus::Exited(_, 0));
        let uptime = run.started.elapsed();
        let mut failures = run.failures;
        if uptime >= STABLE_UPTIME {
            failures = 0;
        }

        crate::info!("svc/{name}: {} after {:?}", describe(status), uptime);

        // Read the OOM counter before the cgroup directory disappears: a
        // service killed by the kernel may be reaped before the next tick.
        if spec.limits.memory_max.is_some() {
            let count = crate::cgroup::oom_kill_count(&name);
            if count > run.oom_seen {
                crate::error!("svc/{name}: OOM-killed ({count} total)");
            }
        }

        // A stop requested through the control socket must not be undone by
        // the restart policy; an explicit restart starts once the exit is seen.
        if self.manual_stop.remove(&idx) {
            if self.restart_after.remove(&idx) {
                self.pending.push(PendingRestart {
                    idx,
                    due: Instant::now(),
                    failures: 0,
                });
            } else {
                self.mark_failed(idx);
            }
            return true;
        }

        // A service stopped because of a runlevel switch must not come back.
        if !self.members().contains(&idx) {
            return true;
        }

        if self.stopping {
            return true;
        }

        if spec.oneshot {
            if success {
                self.completed.insert(idx);
            } else {
                self.mark_failed(idx);
            }
            return true;
        }

        let restart = match spec.restart {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => !success,
            RestartPolicy::Never => false,
        };
        if !restart {
            self.mark_failed(idx);
            return true;
        }

        failures += 1;
        if failures >= MAX_CONSECUTIVE_FAILURES {
            crate::fail!("svc/{name}: {failures} consecutive failures; giving up");
            self.mark_failed(idx);
            return true;
        }
        let delay = backoff_delay(failures);
        crate::warn!(
            "svc/{name}: restarting in {} ms (attempt {failures})",
            delay.as_millis()
        );
        self.pending.push(PendingRestart {
            idx,
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
        self.pending_start.clear();

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
        for (pid, run) in &self.running {
            crate::cgroup::kill(&self.services[run.idx].name);
            signal_service(*pid, Signal::SIGKILL);
        }
        self.pending.clear();
        self.pending_start.clear();
    }

    /// Last resort for a runlevel switch: kill the whole cgroup of every
    /// service that no longer belongs to the level. A service that daemonizes
    /// (a display manager and the X server or session it starts) leaves its
    /// process group, so signalling the tracked pid misses exactly the
    /// processes that still hold the display.
    pub fn kill_foreign(&mut self) {
        let members = self.members();
        let targets: Vec<usize> = self
            .running
            .values()
            .filter(|run| !members.contains(&run.idx))
            .map(|run| run.idx)
            .collect();
        for idx in targets {
            crate::warn!(
                "svc/{}: still running after the runlevel change; killing its cgroup",
                self.services[idx].name
            );
            crate::cgroup::kill(&self.services[idx].name);
        }
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
/// Open (and rotate) a per-service log file, returning the fd handed to the
/// child as stdout/stderr.
fn open_service_log(path: &str) -> Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;

    const MAX_BYTES: u64 = 1 << 20;
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let rotated = path.with_extension("log.1");
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(path, rotated);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o644)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok(OwnedFd::from(file))
}

fn child_exec(
    spec: &ServiceSpec,
    log_fd: OwnedFd,
    credentials: Option<crate::accounts::Credentials>,
    notify_path: Option<String>,
) -> ! {
    // Children must not inherit the signal mask blocked for PID 1. Note that
    // unblocking an empty set is a no-op, so replace the whole mask instead.
    clear_signal_mask();
    let _ = nix::unistd::setsid();
    // Join the service cgroup before dropping privileges: the cgroup files
    // are owned by root.
    crate::cgroup::attach(&spec.name, std::process::id());

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
    // Do not leak PID 1's sockets or the initramfs fds into services.
    for fd in 3..=1024 {
        unsafe { libc::close(fd) };
    }

    if let Some(dir) = &spec.working_dir {
        if let Err(e) = std::env::set_current_dir(dir) {
            child_fail(spec, &format!("cannot chdir to {dir}: {e}"));
        }
    }

    if let Some(creds) = &credentials {
        drop_privileges(spec, creds);
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

    let home = credentials.as_ref().and_then(|c| c.home.as_deref());
    let env = build_env(spec, notify_path.as_deref(), home);
    let error = nix::unistd::execve(&path, &argv, &env);
    child_fail(spec, &format!("cannot exec {}: {error:?}", spec.exec))
}

/// Switch to the configured user, groups first, then gid, then uid.
fn drop_privileges(spec: &ServiceSpec, creds: &crate::accounts::Credentials) {
    if !creds.groups.is_empty() {
        let gids: Vec<libc::gid_t> = creds.groups.iter().map(|g| *g as libc::gid_t).collect();
        if unsafe { libc::setgroups(gids.len(), gids.as_ptr()) } != 0 {
            child_fail(
                spec,
                &format!(
                    "cannot set supplementary groups: {}",
                    std::io::Error::last_os_error()
                ),
            );
        }
    }
    if unsafe { libc::setgid(creds.gid) } != 0 {
        child_fail(
            spec,
            &format!(
                "cannot setgid({}): {}",
                creds.gid,
                std::io::Error::last_os_error()
            ),
        );
    }
    if unsafe { libc::setuid(creds.uid) } != 0 {
        child_fail(
            spec,
            &format!(
                "cannot setuid({}): {}",
                creds.uid,
                std::io::Error::last_os_error()
            ),
        );
    }
}

/// Clear the inherited signal mask (`SIG_SETMASK` with an empty set).
fn clear_signal_mask() {
    use nix::sys::signal::{sigprocmask, SigSet, SigmaskHow};
    let empty = SigSet::empty();
    let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&empty), None);
}

/// Compact byte count for status lines: `0` when unknown.
fn human_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "-".to_string();
    }
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    if bytes < 1024 * 1024 {
        return format!("{:.1}K", bytes as f64 / 1024.0);
    }
    format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
}

/// Like [`human_bytes`] but keeps an explicit `0B` for counters where zero is
/// meaningful (I/O), instead of the "unavailable" dash.
fn human_bytes_zero(bytes: u64) -> String {
    if bytes == 0 {
        "0B".to_string()
    } else {
        human_bytes(bytes)
    }
}

fn human_duration(usec: u64) -> String {
    if usec < 1_000 {
        format!("{usec}us")
    } else if usec < 1_000_000 {
        format!("{:.1}ms", usec as f64 / 1_000.0)
    } else if usec < 60_000_000 {
        format!("{:.2}s", usec as f64 / 1_000_000.0)
    } else if usec < 3_600_000_000 {
        format!("{:.1}m", usec as f64 / 60_000_000.0)
    } else {
        format!("{:.1}h", usec as f64 / 3_600_000_000.0)
    }
}

/// Escape a Prometheus label value.
fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn child_fail(spec: &ServiceSpec, message: &str) -> ! {
    let line = format!("cannot start service: {message}\n");
    let prefix = format!("svc/{}: ", spec.name);
    let _ = unsafe { libc::write(2, prefix.as_ptr().cast(), prefix.len()) };
    let _ = unsafe { libc::write(2, line.as_ptr().cast(), line.len()) };
    unsafe { libc::_exit(127) }
}

fn build_env(spec: &ServiceSpec, notify_path: Option<&str>, home: Option<&str>) -> Vec<CString> {
    let mut vars: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        vars.insert(
            key.as_os_str().as_bytes().to_vec(),
            value.as_os_str().as_bytes().to_vec(),
        );
    }
    let inherited_home = vars.contains_key(b"HOME".as_slice());
    let mut set = |key: &str, value: &str| {
        vars.insert(key.as_bytes().to_vec(), value.as_bytes().to_vec());
    };
    set(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    // An inherited HOME belongs to the instance (a user session); only fall
    // back to /root when neither the spec nor the environment names one.
    match home {
        Some(home) => set("HOME", home),
        None if !inherited_home => set("HOME", "/root"),
        None => {}
    }
    set("JIMINIT_SERVICE", &spec.name);
    if let Some(user) = &spec.user {
        set("USER", user);
        set("LOGNAME", user);
    }
    if let Some(path) = notify_path {
        set("NOTIFY_SOCKET", path);
    }
    if let Some(watchdog) = spec.watchdog {
        set("WATCHDOG_USEC", &watchdog.as_micros().to_string());
    }
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
            user: None,
            group: None,
            supplementary_groups: Vec::new(),
            notify: false,
            watchdog: None,
            limits: Limits::default(),
            log_file: None,
        }
    }

    #[test]
    fn formats_byte_counts() {
        assert_eq!(human_bytes(0), "-");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1536), "1.5K");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0M");
    }

    #[test]
    fn parses_limits() {
        let svc = parse_service(
            "demo",
            "exec = \"/bin/sleep\"\nargs = [\"1\"]\n\
             [limits]\nmemory_max = \"64M\"\npids_max = 64\n\
             cpu_max = \"50000 100000\"\n",
        )
        .unwrap();
        assert_eq!(svc.limits.memory_max.as_deref(), Some("64M"));
        assert_eq!(svc.limits.pids_max, Some(64));
        assert_eq!(svc.limits.cpu_max.as_deref(), Some("50000 100000"));
    }

    #[test]
    fn accepts_memory_suffixes() {
        for value in ["max", "1K", "2m", "3G", "4t", "1048576"] {
            let text = format!("exec = \"/bin/true\"\n[limits]\nmemory_max = \"{value}\"\n");
            assert!(parse_service("demo", &text).is_ok(), "value `{value}`");
        }
    }

    #[test]
    fn rejects_invalid_limits() {
        for (text, needle) in [
            (
                "exec = \"/bin/true\"\n[limits]\nmemory_max = \"64 MB\"\n",
                "memory_max",
            ),
            (
                "exec = \"/bin/true\"\n[limits]\ncpu_max = \"50000\"\n",
                "cpu_max",
            ),
            (
                "exec = \"/bin/true\"\n[limits]\npids_max = -1\n",
                "pids_max",
            ),
            (
                "exec = \"/bin/true\"\n[limits]\nswap_max = \"1G\"\n",
                "swap_max",
            ),
        ] {
            let err = parse_service("demo", text).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains(needle), "{message}");
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
    fn parses_watchdog_intervals() {
        let svc = parse_service("demo", "exec = \"/bin/true\"\nwatchdog = \"500ms\"\n").unwrap();
        assert_eq!(svc.watchdog, Some(Duration::from_millis(500)));
        let svc = parse_service("demo", "exec = \"/bin/true\"\nwatchdog = \"30s\"\n").unwrap();
        assert_eq!(svc.watchdog, Some(Duration::from_secs(30)));
        let svc = parse_service("demo", "exec = \"/bin/true\"\nwatchdog = \"5m\"\n").unwrap();
        assert_eq!(svc.watchdog, Some(Duration::from_secs(300)));
        let svc = parse_service("demo", "exec = \"/bin/true\"\nwatchdog = \"45\"\n").unwrap();
        assert_eq!(svc.watchdog, Some(Duration::from_secs(45)));
    }

    #[test]
    fn rejects_invalid_watchdog_intervals() {
        for value in ["0s", "soon", "-1s", "1.5s"] {
            let text = format!("exec = \"/bin/true\"\nwatchdog = \"{value}\"\n");
            assert!(
                parse_service("demo", &text).is_err(),
                "`{value}` should be rejected"
            );
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(1), Duration::from_millis(1_000));
        assert_eq!(backoff_delay(2), Duration::from_millis(2_000));
        assert_eq!(backoff_delay(3), Duration::from_millis(4_000));
        assert_eq!(backoff_delay(7), Duration::from_millis(60_000));
        assert_eq!(backoff_delay(30), Duration::from_millis(60_000));
    }

    #[test]
    fn formats_durations() {
        assert_eq!(human_duration(0), "0us");
        assert_eq!(human_duration(1_500), "1.5ms");
        assert_eq!(human_duration(2_500_000), "2.50s");
        assert_eq!(human_duration(90_000_000), "1.5m");
        assert_eq!(human_duration(7_200_000_000), "2.0h");
    }

    #[test]
    fn escapes_prometheus_labels() {
        assert_eq!(escape_label("plain-name"), "plain-name");
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(human_bytes_zero(0), "0B");
        assert_eq!(human_bytes_zero(2048), "2.0K");
        assert_eq!(human_bytes(0), "-");
    }
    #[test]
    fn example_services_parse_and_are_acyclic() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("contrib/example/services");
        let services = load_dir(&dir).expect("example service files");
        assert!(services.len() >= 20, "only {} services", services.len());
        let (order, cyclic) = startup_order(&services);
        assert!(cyclic.is_empty(), "dependency cycle: {cyclic:?}");
        assert_eq!(order.len(), services.len());
        assert!(services.iter().any(|spec| spec.name == "userspawn"));
    }

    #[test]
    fn dependency_tree_is_indented() {
        let services = vec![spec("a", &[]), spec("b", &["a"]), spec("c", &["a"])];
        assert_eq!(
            tree_lines(&services),
            vec!["a", "  b  (after a)", "  c  (after a)"]
        );
    }

    #[test]
    fn dot_graph_lists_edges() {
        let services = vec![spec("a", &[]), spec("b", &["a"])];
        let dot = dot_graph(&services);
        assert!(dot.starts_with("digraph jiminit {"));
        assert!(dot.contains("\"a\" -> \"b\";"));
    }
}
