//! Named runlevels and the numeric aliases SysV tools use.
//!
//! Only the three levels Jiminit actually distinguishes exist: `single`
//! (maintenance shell), `multi-user` (services + getty) and `graphical`
//! (multi-user plus anything tagged graphical). `0`/`poweroff` and
//! `6`/`reboot` are accepted as targets by the control client but are not
//! runlevels: they end up in an orderly shutdown instead.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Single,
    MultiUser,
    Graphical,
}

impl Level {
    pub const DEFAULT: Level = Level::MultiUser;

    pub fn name(self) -> &'static str {
        match self {
            Level::Single => "single",
            Level::MultiUser => "multi-user",
            Level::Graphical => "graphical",
        }
    }

    /// Accepts the English name and the traditional numeric alias
    /// (`1`, `3`, `5`).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "single" | "1" => Some(Level::Single),
            "multi-user" | "multiuser" | "3" => Some(Level::MultiUser),
            "graphical" | "5" => Some(Level::Graphical),
            _ => None,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A parsed `jiminit --runlevel` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Level(Level),
    PowerOff,
    Reboot,
}

impl Target {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "0" | "poweroff" | "halt" => Some(Target::PowerOff),
            "6" | "reboot" => Some(Target::Reboot),
            name => Level::parse(name).map(Target::Level),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_numeric_aliases() {
        assert_eq!(Level::parse("single"), Some(Level::Single));
        assert_eq!(Level::parse("1"), Some(Level::Single));
        assert_eq!(Level::parse("multi-user"), Some(Level::MultiUser));
        assert_eq!(Level::parse("MultiUser"), Some(Level::MultiUser));
        assert_eq!(Level::parse("3"), Some(Level::MultiUser));
        assert_eq!(Level::parse("graphical"), Some(Level::Graphical));
        assert_eq!(Level::parse("5"), Some(Level::Graphical));
        assert_eq!(Level::parse("bogus"), None);
    }

    #[test]
    fn power_targets_are_not_levels() {
        assert_eq!(Target::parse("poweroff"), Some(Target::PowerOff));
        assert_eq!(Target::parse("0"), Some(Target::PowerOff));
        assert_eq!(Target::parse("reboot"), Some(Target::Reboot));
        assert_eq!(Target::parse("6"), Some(Target::Reboot));
        assert_eq!(
            Target::parse("multi-user"),
            Some(Target::Level(Level::MultiUser))
        );
    }

    #[test]
    fn display_uses_english_names() {
        assert_eq!(Level::Single.to_string(), "single");
        assert_eq!(Level::MultiUser.to_string(), "multi-user");
        assert_eq!(Level::Graphical.to_string(), "graphical");
    }
}
