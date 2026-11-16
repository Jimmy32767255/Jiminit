//! jiminit — a small experimental Rust init.
//!
//! Two modes:
//! * initramfs stage 1 (`root=` on the kernel command line): mount the real
//!   root and switch_root into it.
//! * root stage 2 (PID 1 on the final root): hostname, console, getty/login,
//!   service supervision, runlevel switching, reaping and shutdown.
//! * a small client (`jiminit status`, `jiminit reboot`, `jiminit service
//!   start NAME`, ...) that talks to the running init over /run/jiminit.sock.
//!
//! PID 1 must never exit, so every failure ends up in a rescue shell loop.

mod accounts;
mod cgroup;
mod cli;
mod cmdline;
mod coldplug;
mod config;
mod crypttab;
mod fstab;
mod log;
mod luks;
mod modules;
mod mount;
mod power;
mod rootdev;
mod runlevel;
mod service;
mod stage1;
mod stage2;
mod user;

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Running from the initramfs, needs to mount the real root.
    Initramfs,
    /// Running on the real root as the long-lived PID 1.
    Root,
}

fn main() {
    if let Some(code) = run_command() {
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
        Ok(Err(e)) => crate::crit!("{stage:?} failed: {e:#}"),
        Err(_) => crate::crit!("{stage:?} panicked"),
    }

    stage2::rescue();
}

/// Run a one-shot client command. Returns `None` when jiminit should start as
/// the init itself (no arguments, or the internal stage switches).
fn run_command() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match cli::parse(&args) {
        Ok(None) => return None,
        Ok(Some(parsed)) => parsed,
        Err(error) => {
            eprintln!("jiminit: {error}");
            eprintln!();
            eprint!("{}", cli::USAGE);
            return Some(2);
        }
    };
    // A one-shot client should die quietly when the reader of a pipe goes
    // away, like any other CLI; the init itself keeps ignoring SIGPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    log::init();
    if parsed.no_color {
        log::set_color(log::Color::Never);
    } else if let Ok(cfg) = config::load(std::path::Path::new(config::DEFAULT_PATH)) {
        log::set_color(cfg.color_choice());
    }

    let user_socket = user::control_path();
    let path: &Path = if parsed.user {
        &user_socket
    } else {
        Path::new(stage2::CONTROL_PATH)
    };

    let code = match parsed.command {
        cli::Command::Help => {
            print!("{}", cli::USAGE);
            0
        }
        cli::Command::Version => {
            println!("jiminit {}", env!("CARGO_PKG_VERSION"));
            0
        }
        cli::Command::RunUser => run_or_report(user::run(), "start the user instance"),
        cli::Command::Userspawn => run_or_report(user::userspawn(), "start userspawn"),
        cli::Command::StopInstance => {
            if parsed.user {
                run_client(path, "stop", Some(CLIENT_TIMEOUT))
            } else {
                usage_error("`stop` is only for a user instance (jiminit --user stop)")
            }
        }
        cli::Command::Check => {
            if parsed.user {
                usage_error("`check` validates the system profile; run it without --user")
            } else {
                run_check()
            }
        }
        cli::Command::Graph => {
            if parsed.user {
                usage_error("`graph` validates the system profile; run it without --user")
            } else {
                run_graph()
            }
        }
        cli::Command::NotifySend { socket, message } => notify_send(&socket, &message),
        cli::Command::Status { service, watch } => run_status(path, service.as_deref(), watch),
        cli::Command::List => run_client(path, "list", Some(CLIENT_TIMEOUT)),
        cli::Command::Metrics => run_client(path, "metrics", Some(CLIENT_TIMEOUT)),
        cli::Command::Service { action, name } => run_client(
            path,
            &format!("service {} {name}", action.as_str()),
            Some(CLIENT_TIMEOUT),
        ),
        cli::Command::Runlevel { target } => match target {
            Some(_) if parsed.user => {
                usage_error("a user instance has no runlevels to switch; `runlevel` only queries")
            }
            Some(level) => run_client(path, &format!("runlevel {level}"), Some(CLIENT_TIMEOUT)),
            None => run_client(path, "runlevel", Some(CLIENT_TIMEOUT)),
        },
        cli::Command::Power(action) => {
            if parsed.user {
                usage_error("power operations are not available in a user instance")
            } else {
                // Suspend and hibernate only answer after the machine wakes up.
                let timeout = if action.answers_after_wake() {
                    None
                } else {
                    Some(CLIENT_TIMEOUT)
                };
                run_client(path, action.as_str(), timeout)
            }
        }
    };
    Some(code)
}

/// How long the client waits for the init's reply before giving up.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

/// Run a blocking instance command and map failures to exit code 1.
fn run_or_report(result: anyhow::Result<()>, what: &str) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            crate::error!("cannot {what}: {e:#}");
            1
        }
    }
}

/// Reject a command that does not fit the selected mode.
fn usage_error(message: &str) -> i32 {
    eprintln!("jiminit: {message}");
    2
}

/// `status [service] [--watch]`: print once, or clear and repeat.
fn run_status(path: &Path, service: Option<&str>, watch: Option<u64>) -> i32 {
    let request = match service {
        Some(name) => format!("status {name}"),
        None => "status".to_string(),
    };
    let mut first = true;
    loop {
        if !first {
            clear_screen();
        }
        first = false;
        let code = run_client(path, &request, Some(CLIENT_TIMEOUT));
        let Some(secs) = watch else {
            return code;
        };
        std::thread::sleep(Duration::from_secs(secs));
    }
}

fn clear_screen() {
    if unsafe { libc::isatty(1) } == 1 {
        print!("\x1b[2J\x1b[H");
        let _ = std::io::stdout().flush();
    }
}

/// Validate the configuration and every service file without starting anything.
fn run_check() -> i32 {
    match check_config() {
        Ok(()) => 0,
        Err(e) => {
            crate::fail!("configuration check failed: {e:#}");
            1
        }
    }
}

fn check_config() -> anyhow::Result<()> {
    use anyhow::Context;
    let cfg = crate::config::load(std::path::Path::new(crate::config::DEFAULT_PATH))
        .context("load configuration")?;
    crate::log::set_color(cfg.color_choice());
    let services = crate::service::load_dir(std::path::Path::new(&cfg.services_dir))
        .context("load service definitions")?;
    let (order, cyclic) = crate::service::startup_order(&services);
    if !cyclic.is_empty() {
        anyhow::bail!("dependency cycle involving {}", cyclic.join(", "));
    }
    println!(
        "{} configuration ok: runlevel {}, getty {}",
        crate::log::succ_tag(),
        cfg.default_runlevel,
        cfg.getty
    );
    println!("         log: {}", cfg.log_file);
    if services.is_empty() {
        println!("         services: none");
        return Ok(());
    }
    println!("         services ({}):", services.len());
    for line in crate::service::tree_lines(&services) {
        println!("           {line}");
    }
    let names: Vec<&str> = order
        .iter()
        .map(|index| services[*index].name.as_str())
        .collect();
    println!("         start order: {}", names.join(" -> "));
    Ok(())
}

/// Print the service dependency graph as Graphviz DOT.
fn run_graph() -> i32 {
    match graph_config() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("jiminit: cannot build dependency graph: {e:#}");
            1
        }
    }
}

fn graph_config() -> anyhow::Result<()> {
    use anyhow::Context;
    let cfg = crate::config::load(std::path::Path::new(crate::config::DEFAULT_PATH))
        .context("load configuration")?;
    let services = crate::service::load_dir(std::path::Path::new(&cfg.services_dir))
        .context("load service definitions")?;
    let (_, cyclic) = crate::service::startup_order(&services);
    if !cyclic.is_empty() {
        anyhow::bail!("dependency cycle involving {}", cyclic.join(", "));
    }
    print!("{}", crate::service::dot_graph(&services));
    Ok(())
}

fn run_client(path: &Path, request: &str, read_timeout: Option<Duration>) -> i32 {
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!(
                "jiminit: cannot connect to {}: {e} (is the instance running?)",
                path.display()
            );
            return 1;
        }
    };
    let _ = stream.set_read_timeout(read_timeout);

    if let Err(e) = stream.write_all(request.as_bytes()) {
        eprintln!("jiminit: cannot send to {}: {e}", path.display());
        return 1;
    }

    let mut reply = String::new();
    if let Err(e) = stream.read_to_string(&mut reply) {
        eprintln!("jiminit: no reply from init: {e}");
        return 1;
    }
    // The session may be hung up while the reply is on the way (a runlevel
    // switch stops the session that asked for it), so ignore write errors.
    let _ = std::io::stdout().write_all(colorize_reply(&reply).as_bytes());
    if reply.starts_with("error") {
        1
    } else {
        0
    }
}

/// Give `status` a little color: green running, dim stopped, red failed.
fn colorize_reply(reply: &str) -> String {
    if !log::stdout_colors() {
        return reply.to_string();
    }
    let mut out = String::with_capacity(reply.len() + 32);
    for line in reply.lines() {
        let code = if line.contains(" running ") {
            Some("1;32")
        } else if line.contains(" failed") || line.starts_with("error") {
            Some("1;31")
        } else if line.contains(" stopped ") {
            Some("2")
        } else {
            None
        };
        match code {
            Some(code) => out.push_str(&format!("\x1b[{code}m{line}\x1b[0m\n")),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if !reply.ends_with('\n') {
        out.pop();
    }
    out
}

/// Send one sd_notify datagram, e.g. `jiminit notify-send "$NOTIFY_SOCKET" READY=1`.
fn notify_send(path: &str, message: &str) -> i32 {
    let socket = match UnixDatagram::unbound() {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("jiminit: cannot create datagram socket: {e}");
            return 1;
        }
    };
    match socket.send_to(message.as_bytes(), path) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("jiminit: cannot send to {path}: {e}");
            1
        }
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
            crate::error!("cannot read /proc/cmdline ({e}); guessing stage from the root fs");
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
