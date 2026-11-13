//! Kernel command line parsing (`/proc/cmdline`).

/// Parsed kernel command line: a list of whitespace separated tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cmdline {
    tokens: Vec<String>,
}

impl Cmdline {
    pub fn read() -> std::io::Result<Self> {
        Ok(Self::parse(&std::fs::read_to_string("/proc/cmdline")?))
    }

    pub fn parse(raw: &str) -> Self {
        Self {
            tokens: raw.split_whitespace().map(ToString::to_string).collect(),
        }
    }

    /// Value of a `key=value` token. Later tokens win, matching how the
    /// kernel applies repeated `__setup` parameters.
    pub fn value(&self, key: &str) -> Option<&str> {
        let prefix = format!("{key}=");
        self.tokens
            .iter()
            .rev()
            .find_map(|token| token.strip_prefix(prefix.as_str()))
    }

    /// Whether a bare flag such as `rw` or `quiet` is present.
    pub fn has(&self, flag: &str) -> bool {
        self.tokens.iter().any(|token| token == flag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_values_override_earlier_ones() {
        let cm = Cmdline::parse("root=/dev/sda root=/dev/vdb rw");
        assert_eq!(cm.value("root"), Some("/dev/vdb"));
    }

    #[test]
    fn parses_values_and_flags() {
        let cm = Cmdline::parse("console=ttyS0 rdinit=/init root=/dev/vda rw quiet");
        assert_eq!(cm.value("console"), Some("ttyS0"));
        assert_eq!(cm.value("root"), Some("/dev/vda"));
        assert_eq!(cm.value("missing"), None);
        assert!(cm.has("rw"));
        assert!(cm.has("quiet"));
        assert!(!cm.has("ro"));
    }

    #[test]
    fn value_with_equals_inside() {
        let cm = Cmdline::parse("root=UUID=1234-5678 rootfstype=ext4");
        assert_eq!(cm.value("root"), Some("UUID=1234-5678"));
        assert_eq!(cm.value("rootfstype"), Some("ext4"));
    }

    #[test]
    fn empty_and_extra_whitespace() {
        let cm = Cmdline::parse("  \n root=/dev/sda1\t rw ");
        assert_eq!(cm.value("root"), Some("/dev/sda1"));
        assert!(!cm.has("missing"));
        assert_eq!(cm.value("root"), Some("/dev/sda1"));
    }
}
