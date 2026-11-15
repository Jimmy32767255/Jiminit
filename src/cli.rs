//! Sub-command parsing for the one-shot client.
//!
//! Every operation is a sub-command; options only modify how it runs
//! (`--watch`, `--no-color`, `--help`, `--version`). The init itself is
//! started by the kernel without arguments, so an empty command line — or one
//! carrying the internal `--stage1`/`--stage2` switches — means "become the
//! init", not "run a client command".

/// Actions that control one service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    Reload,
}

impl ServiceAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
            ServiceAction::Reload => "reload",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "start" => Some(ServiceAction::Start),
            "stop" => Some(ServiceAction::Stop),
            "restart" => Some(ServiceAction::Restart),
            "reload" => Some(ServiceAction::Reload),
            _ => None,
        }
    }
}

/// Machine-level power operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Reboot,
    PowerOff,
    Halt,
    SoftReboot,
    /// Reboot into a kernel image that is already loaded in memory.
    Kexec,
    Suspend,
    Hibernate,
}

impl PowerAction {
    /// The verb sent over the control socket.
    pub fn as_str(self) -> &'static str {
        match self {
            PowerAction::Reboot => "reboot",
            PowerAction::PowerOff => "poweroff",
            PowerAction::Halt => "halt",
            PowerAction::SoftReboot => "soft-reboot",
            PowerAction::Kexec => "kexec",
            PowerAction::Suspend => "suspend",
            PowerAction::Hibernate => "hibernate",
        }
    }

    /// Suspend and hibernate only answer after the machine wakes up, so the
    /// client must not time the reply out.
    pub fn answers_after_wake(self) -> bool {
        matches!(self, PowerAction::Suspend | PowerAction::Hibernate)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `status [service] [--watch[=SECS]]`
    Status {
        service: Option<String>,
        watch: Option<u64>,
    },
    /// `list`
    List,
    /// `metrics`
    Metrics,
    /// `service <start|stop|restart|reload> <name>`
    Service {
        action: ServiceAction,
        name: String,
    },
    /// `runlevel [name]`; no name queries the current level.
    Runlevel {
        target: Option<String>,
    },
    /// `reboot`, `poweroff`, `halt`, `soft-reboot`, `kexec`, `suspend`,
    /// `hibernate`
    Power(PowerAction),
    /// `check`
    Check,
    /// `graph`
    Graph,
    /// `notify-send <socket> <message>` — used by services for sd_notify.
    NotifySend {
        socket: String,
        message: String,
    },
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub command: Command,
    pub no_color: bool,
}

pub const USAGE: &str = "\
jiminit — experimental init and service manager

Usage:
  jiminit <command> [arguments] [options]

System control:
  reboot                    reboot the machine
  poweroff                  power off the machine
  halt                      halt the machine without powering it off
  soft-reboot               re-exec stage 2 without a kernel reboot
  kexec                     reboot into a kernel image already loaded in memory
  suspend                   suspend to RAM
  hibernate                 hibernate to disk

Services:
  status [service]          current runlevel and service state
  list                      list the configured services
  service <action> <name>   start, stop, restart or reload one service
  metrics                   Prometheus text exposition

Sessions:
  runlevel [name]           show the current runlevel, or switch to
                            single, multi-user or graphical

Configuration:
  check                     validate the configuration and service files
  graph                     service dependency graph as Graphviz DOT

Options:
  --watch[=SECS]            repeat status every SECS seconds (default 1)
  --no-color                disable colors
  -h, --help                show this help
  -V, --version             show the version

Without a command, or with the internal --stage1/--stage2 switches, jiminit
starts as the init itself.
";

/// Parse the client command line.
///
/// Returns `Ok(None)` when jiminit should start as the init instead of running
/// a one-shot command.
pub fn parse(args: &[String]) -> Result<Option<Parsed>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    if args
        .iter()
        .any(|arg| arg == "--stage1" || arg == "--stage2")
    {
        return Ok(None);
    }

    let mut rest: Vec<&str> = Vec::with_capacity(args.len());
    let mut no_color = false;
    for arg in args {
        if arg == "--no-color" {
            no_color = true;
        } else {
            rest.push(arg);
        }
    }

    let Some((name, tail)) = rest.split_first() else {
        return Ok(Some(Parsed {
            command: Command::Help,
            no_color,
        }));
    };

    let command = match *name {
        "-h" | "--help" | "help" => Command::Help,
        "-V" | "--version" | "version" => Command::Version,
        "status" => parse_status(tail)?,
        "list" => {
            no_arguments("list", tail)?;
            Command::List
        }
        "metrics" => {
            no_arguments("metrics", tail)?;
            Command::Metrics
        }
        "service" => parse_service(tail)?,
        "runlevel" => match tail {
            [] => Command::Runlevel { target: None },
            [target] => Command::Runlevel {
                target: Some((*target).to_string()),
            },
            _ => return Err("`runlevel` takes at most one level name".to_string()),
        },
        "reboot" => power(PowerAction::Reboot, tail)?,
        "poweroff" => power(PowerAction::PowerOff, tail)?,
        "halt" => power(PowerAction::Halt, tail)?,
        "soft-reboot" => power(PowerAction::SoftReboot, tail)?,
        "kexec" => power(PowerAction::Kexec, tail)?,
        "suspend" => power(PowerAction::Suspend, tail)?,
        "hibernate" => power(PowerAction::Hibernate, tail)?,
        "check" => {
            no_arguments("check", tail)?;
            Command::Check
        }
        "graph" => {
            no_arguments("graph", tail)?;
            Command::Graph
        }
        "notify-send" => match tail {
            [socket, message] => Command::NotifySend {
                socket: (*socket).to_string(),
                message: (*message).to_string(),
            },
            _ => return Err("usage: jiminit notify-send <socket> <message>".to_string()),
        },
        other => return Err(format!("unknown command `{other}` (try `jiminit --help`)")),
    };

    Ok(Some(Parsed { command, no_color }))
}

fn parse_status(tail: &[&str]) -> Result<Command, String> {
    let mut service = None;
    let mut watch = None;
    let mut index = 0;
    while index < tail.len() {
        let arg = tail[index];
        if arg == "--watch" {
            match tail.get(index + 1).copied().and_then(parse_seconds) {
                Some(secs) => {
                    watch = Some(secs);
                    index += 2;
                }
                None => {
                    watch = Some(1);
                    index += 1;
                }
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--watch=") {
            watch = Some(parse_seconds(value).ok_or_else(watch_error)?);
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("unknown option `{arg}` for `status`"));
        }
        if service.is_some() {
            return Err("`status` takes at most one service name".to_string());
        }
        service = Some(arg.to_string());
        index += 1;
    }
    Ok(Command::Status { service, watch })
}

fn watch_error() -> String {
    "`--watch` takes an optional number of seconds (1-3600)".to_string()
}

fn parse_seconds(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|secs| (1..=3600).contains(secs))
}

fn parse_service(tail: &[&str]) -> Result<Command, String> {
    let [action, name] = tail else {
        return Err("usage: jiminit service <start|stop|restart|reload> <name>".to_string());
    };
    let action = ServiceAction::parse(action).ok_or_else(|| {
        format!("unknown service action `{action}` (use start, stop, restart or reload)")
    })?;
    if name.starts_with('-') || name.is_empty() {
        return Err(format!("invalid service name `{name}`"));
    }
    Ok(Command::Service {
        action,
        name: (*name).to_string(),
    })
}

fn power(action: PowerAction, tail: &[&str]) -> Result<Command, String> {
    no_arguments(action.as_str(), tail)?;
    Ok(Command::Power(action))
}

fn no_arguments(command: &str, tail: &[&str]) -> Result<(), String> {
    match tail {
        [] => Ok(()),
        [extra] => Err(format!("`{command}` takes no arguments (got `{extra}`)")),
        _ => Err(format!("`{command}` takes no arguments")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Option<Parsed>, String> {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        parse(&owned)
    }

    fn command(args: &[&str]) -> Command {
        parse_args(args).unwrap().unwrap().command
    }

    #[test]
    fn empty_command_line_means_init() {
        assert_eq!(parse_args(&[]).unwrap(), None);
        assert_eq!(parse_args(&["--stage1"]).unwrap(), None);
        assert_eq!(
            parse_args(&["--stage2", "--no-color"]).unwrap(),
            None,
            "internal stage switches win over modifiers"
        );
    }

    #[test]
    fn power_commands_are_subcommands() {
        assert_eq!(command(&["reboot"]), Command::Power(PowerAction::Reboot));
        assert_eq!(
            command(&["poweroff"]),
            Command::Power(PowerAction::PowerOff)
        );
        assert_eq!(command(&["halt"]), Command::Power(PowerAction::Halt));
        assert_eq!(
            command(&["soft-reboot"]),
            Command::Power(PowerAction::SoftReboot)
        );
        assert_eq!(command(&["suspend"]), Command::Power(PowerAction::Suspend));
        assert_eq!(
            command(&["hibernate"]),
            Command::Power(PowerAction::Hibernate)
        );
    }

    #[test]
    fn service_takes_action_then_name() {
        assert_eq!(
            command(&["service", "restart", "ticker"]),
            Command::Service {
                action: ServiceAction::Restart,
                name: "ticker".into()
            }
        );
        assert!(parse_args(&["service", "ticker", "restart"]).is_err());
        assert!(parse_args(&["service", "start"]).is_err());
        assert!(parse_args(&["service", "bounce", "ticker"]).is_err());
    }

    #[test]
    fn status_accepts_one_service_and_watch() {
        assert_eq!(
            command(&["status"]),
            Command::Status {
                service: None,
                watch: None
            }
        );
        assert_eq!(
            command(&["status", "sddm"]),
            Command::Status {
                service: Some("sddm".into()),
                watch: None
            }
        );
        assert_eq!(
            command(&["status", "sddm", "--watch"]),
            Command::Status {
                service: Some("sddm".into()),
                watch: Some(1)
            }
        );
        assert_eq!(
            command(&["status", "--watch=5"]),
            Command::Status {
                service: None,
                watch: Some(5)
            }
        );
        assert_eq!(
            command(&["status", "--watch", "5"]),
            Command::Status {
                service: None,
                watch: Some(5)
            }
        );
        assert!(parse_args(&["status", "a", "b"]).is_err());
        assert!(parse_args(&["status", "--watch=0"]).is_err());
        assert!(parse_args(&["status", "--bogus"]).is_err());
    }

    #[test]
    fn runlevel_without_name_queries() {
        assert_eq!(command(&["runlevel"]), Command::Runlevel { target: None });
        assert_eq!(
            command(&["runlevel", "single"]),
            Command::Runlevel {
                target: Some("single".into())
            }
        );
        assert!(parse_args(&["runlevel", "a", "b"]).is_err());
    }

    #[test]
    fn no_color_is_a_modifier_for_any_command() {
        let parsed = parse_args(&["status", "--no-color"]).unwrap().unwrap();
        assert!(parsed.no_color);
        assert_eq!(
            parsed.command,
            Command::Status {
                service: None,
                watch: None
            }
        );
        assert!(
            parse_args(&["metrics", "--no-color"])
                .unwrap()
                .unwrap()
                .no_color
        );
    }

    #[test]
    fn local_commands_take_no_arguments() {
        assert_eq!(command(&["check"]), Command::Check);
        assert_eq!(command(&["graph"]), Command::Graph);
        assert_eq!(command(&["list"]), Command::List);
        assert_eq!(command(&["metrics"]), Command::Metrics);
        assert!(parse_args(&["check", "now"]).is_err());
        assert!(parse_args(&["list", "--watch"]).is_err());
    }

    #[test]
    fn unknown_commands_are_rejected() {
        let error = parse_args(&["rebooot"]).unwrap_err();
        assert!(error.contains("unknown command `rebooot`"), "{error}");
    }

    #[test]
    fn notify_send_takes_socket_and_message() {
        assert_eq!(
            command(&["notify-send", "/run/notify.sock", "READY=1"]),
            Command::NotifySend {
                socket: "/run/notify.sock".into(),
                message: "READY=1".into()
            }
        );
        assert!(parse_args(&["notify-send", "/run/notify.sock"]).is_err());
    }

    #[test]
    fn help_and_version() {
        assert_eq!(command(&["--help"]), Command::Help);
        assert_eq!(command(&["-h"]), Command::Help);
        assert_eq!(command(&["version"]), Command::Version);
        assert_eq!(command(&["-V"]), Command::Version);
    }

    #[test]
    fn power_actions_map_to_protocol_verbs() {
        assert_eq!(PowerAction::SoftReboot.as_str(), "soft-reboot");
        assert_eq!(PowerAction::Kexec.as_str(), "kexec");
        assert!(PowerAction::Suspend.answers_after_wake());
        assert!(!PowerAction::Reboot.answers_after_wake());
    }
}
