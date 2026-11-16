//! Minimal logger with syslog-style severities.
//!
//! Log records go to stderr (which, for PID 1, is the kernel console) and to
//! `/dev/kmsg` when available, so messages also show up in `dmesg` and on
//! serial consoles that the kernel uses for printk. Once the config is loaded
//! stage2 also appends to a size-capped file (default `/var/log/jiminit.log`).
//!
//! Every line looks like `[   12.345] [ INFO ] message`: the first column is
//! the time since jiminit started, the second is a fixed-width 6-character
//! tag. Colors are only written to terminals and never reach kmsg or the log
//! file.
//!
//! Tags, in increasing severity: `[TRACE ]`, `[DE BUG]`, `[ INFO ]`,
//! `[ MESG ]`, `[ WARN ]`, `[ERR OR]`, `[CRITIC]`. `[ SUCC ]` and `[ FAIL ]`
//! mark the outcome of one operation (logged at INFO and ERROR severity), and
//! `[ WAIT ]` renders an animated progress line for a bounded wait.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Rotate the log file once it would grow past this size.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// How often the `[ WAIT ]` spinner redraws its line.
const WAIT_REDRAW: Duration = Duration::from_millis(100);

/// Waits shorter than this stay silent: a 20ms modprobe does not deserve a
/// progress line.
const WAIT_GRACE: Duration = Duration::from_millis(300);

/// The classic `- \ | /` spinner.
const SPINNER: [char; 4] = ['-', '\\', '|', '/'];

/// Every tag is exactly 6 characters wide between the brackets, e.g.
/// `[ WARN ]`, `[DE BUG]`, `[TRACE ]`.
const TAG_SUCC: &str = "[ SUCC ]";
const TAG_INFO: &str = "[ INFO ]";
const TAG_WARN: &str = "[ WARN ]";
const TAG_MESG: &str = "[ MESG ]";
const TAG_DEBUG: &str = "[DE BUG]";
const TAG_TRACE: &str = "[TRACE ]";
const TAG_FAIL: &str = "[ FAIL ]";
const TAG_ERROR: &str = "[ERR OR]";
const TAG_CRIT: &str = "[CRITIC]";
const TAG_WAIT: &str = "[ WAIT ]";

const ANSI_SUCC: &str = "92";
const ANSI_INFO: &str = "37";
const ANSI_WARN: &str = "93";
const ANSI_MESG: &str = "96";
const ANSI_DEBUG: &str = "34";
const ANSI_TRACE: &str = "90";
const ANSI_FAIL: &str = "91";
const ANSI_ERROR: &str = "31";
const ANSI_CRIT: &str = "41;30";
const ANSI_WAIT: &str = "36";
const ANSI_DIM: &str = "2";

static KMSG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static LOG_FILE: OnceLock<Option<Mutex<FileSink>>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static COLOR_MODE: AtomicU8 = AtomicU8::new(0);
static THRESHOLD: AtomicU8 = AtomicU8::new(Level::Info as u8);

struct FileSink {
    file: File,
    path: PathBuf,
    written: u64,
    dead: bool,
}

/// Where colors are allowed: `auto` only on terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Auto,
    Always,
    Never,
}

impl Color {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Color::Auto),
            "always" => Some(Color::Always),
            "never" => Some(Color::Never),
            _ => None,
        }
    }

    fn enabled(self, fd: i32) -> bool {
        match self {
            Color::Always => true,
            Color::Never => false,
            Color::Auto => unsafe { libc::isatty(fd) == 1 },
        }
    }
}

/// Severity of a log record; `log_level` drops everything below the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    /// Needs the user's attention (confirmation, action), above plain info.
    Message = 3,
    Warn = 4,
    Error = 5,
    Critical = 6,
}

impl Level {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "mesg" | "message" | "notice" => Some(Level::Message),
            "warn" | "warning" => Some(Level::Warn),
            "error" | "err" => Some(Level::Error),
            "crit" | "critical" => Some(Level::Critical),
            _ => None,
        }
    }

    /// The tag, color and kmsg priority a record of this level uses.
    fn tag(self) -> (&'static str, &'static str, u8) {
        match self {
            Level::Trace => (TAG_TRACE, ANSI_TRACE, 7),
            Level::Debug => (TAG_DEBUG, ANSI_DEBUG, 7),
            Level::Info => (TAG_INFO, ANSI_INFO, 6),
            Level::Message => (TAG_MESG, ANSI_MESG, 5),
            Level::Warn => (TAG_WARN, ANSI_WARN, 4),
            Level::Error => (TAG_ERROR, ANSI_ERROR, 3),
            Level::Critical => (TAG_CRIT, ANSI_CRIT, 2),
        }
    }
}

/// Which records are written at all.
pub fn set_level(level: Level) {
    THRESHOLD.store(level as u8, Ordering::Relaxed);
}

pub fn level() -> Level {
    match THRESHOLD.load(Ordering::Relaxed) {
        0 => Level::Trace,
        1 => Level::Debug,
        2 => Level::Info,
        3 => Level::Message,
        4 => Level::Warn,
        5 => Level::Error,
        _ => Level::Critical,
    }
}

/// Open the kernel log device and start the relative clock.
pub fn init() {
    START.get_or_init(Instant::now);
    KMSG.get_or_init(|| {
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/kmsg")
            .ok()
            .map(Mutex::new)
    });
}

pub fn set_color(color: Color) {
    let value = match color {
        Color::Auto => 0,
        Color::Always => 1,
        Color::Never => 2,
    };
    COLOR_MODE.store(value, Ordering::Relaxed);
}

fn color_mode() -> Color {
    match COLOR_MODE.load(Ordering::Relaxed) {
        1 => Color::Always,
        2 => Color::Never,
        _ => Color::Auto,
    }
}

/// Whether terminal colors are enabled for stderr (the log stream).
pub fn log_colors() -> bool {
    color_mode().enabled(2)
}

/// Whether terminal colors are enabled for stdout (query commands).
pub fn stdout_colors() -> bool {
    color_mode().enabled(1)
}

/// Whether the log stream can be redrawn in place (a real terminal).
fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// Wrap `text` in an SGR sequence when stdout colors are enabled.
pub fn paint_stdout(text: &str, code: &str) -> String {
    if stdout_colors() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The green `[ SUCC ]` tag, for query commands that print without a prefix.
pub fn succ_tag() -> String {
    paint_stdout(TAG_SUCC, ANSI_SUCC)
}

/// Start appending to `path` (an empty path disables the file sink). Errors
/// are not fatal: the first message explaining them still reaches kmsg.
pub fn init_file(path: &str) {
    let sink = if path.trim().is_empty() {
        None
    } else {
        match open_sink(Path::new(path)) {
            Ok(sink) => Some(Mutex::new(sink)),
            Err(e) => {
                warn(&format!("cannot open log file {path}: {e}"));
                None
            }
        }
    };
    let _ = LOG_FILE.set(sink);
}

fn open_sink(path: &Path) -> std::io::Result<FileSink> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    Ok(FileSink {
        file,
        path: path.to_path_buf(),
        written,
        dead: false,
    })
}

fn elapsed() -> f64 {
    START
        .get()
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or(0.0)
}

fn stamp() -> String {
    format!("[{:>9.3}]", elapsed())
}

fn emit(level: Level, msg: &str) {
    let (tag, ansi, priority) = level.tag();
    write_record(tag, ansi, priority, level, msg);
}

/// One operation succeeded (`[ SUCC ]`, gated at INFO severity).
pub fn succ(msg: &str) {
    write_record(TAG_SUCC, ANSI_SUCC, 6, Level::Info, msg);
}

/// One operation failed (`[ FAIL ]`, gated at ERROR severity).
pub fn fail(msg: &str) {
    write_record(TAG_FAIL, ANSI_FAIL, 3, Level::Error, msg);
}

/// Render one record to stderr, `/dev/kmsg` and the log file.
fn write_record(tag: &str, ansi: &str, priority: u8, level: Level, msg: &str) {
    if level < self::level() {
        return;
    }
    let stamp = stamp();
    let plain = format!("{stamp} {tag} {msg}\n");

    let mut line = String::with_capacity(plain.len() + 16);
    if log_colors() {
        line.push_str(&format!(
            "\x1b[{ANSI_DIM}m{stamp}\x1b[0m \x1b[{ansi}m{tag}\x1b[0m {msg}\n"
        ));
    } else {
        line.push_str(&plain);
    }

    write_streams(&plain, tag, priority, msg);
    // PID 1 must never panic because a log target is broken (for example the
    // console disappeared), so ignore write errors instead of using eprintln!,
    // which panics if writing to stderr fails.
    let _ = std::io::stderr().write_all(line.as_bytes());
}

/// Write a record to `/dev/kmsg` and the log file, never to stderr.
fn write_streams(plain: &str, tag: &str, priority: u8, msg: &str) {
    if let Some(Some(kmsg)) = KMSG.get() {
        if let Ok(mut file) = kmsg.lock() {
            // /dev/kmsg treats every write() as one record, so build the full
            // line first instead of using write!/writeln! (multiple writes).
            // The kernel adds its own timestamp, so ours is left out here.
            let record = format!("<{priority}>jiminit: {tag} {msg}\n");
            let _ = file.write_all(record.as_bytes());
        }
    }
    if let Some(Some(sink)) = LOG_FILE.get() {
        if let Ok(mut sink) = sink.lock() {
            sink.write(plain);
        }
    }
}

impl FileSink {
    fn write(&mut self, line: &str) {
        if self.dead {
            return;
        }
        if self.written + line.len() as u64 > MAX_FILE_BYTES {
            self.rotate();
        }
        if self.dead {
            return;
        }
        match self.file.write_all(line.as_bytes()) {
            Ok(()) => self.written += line.len() as u64,
            Err(_) => self.dead = true,
        }
    }

    fn rotate(&mut self) {
        let rotated = PathBuf::from(format!("{}.1", self.path.display()));
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(&self.path, &rotated);
        match open_sink(&self.path) {
            Ok(sink) => *self = sink,
            Err(_) => self.dead = true,
        }
    }
}

macro_rules! log_fn {
    ($name:ident, $level:expr) => {
        pub fn $name(msg: &str) {
            emit($level, msg);
        }
    };
}

log_fn!(trace, Level::Trace);
log_fn!(debug, Level::Debug);
log_fn!(info, Level::Info);
log_fn!(mesg, Level::Message);
log_fn!(warn, Level::Warn);
log_fn!(error, Level::Error);
log_fn!(crit, Level::Critical);

/// Log one static `[ WAIT ]` line for an asynchronous wait (no animation).
pub fn wait_notice(label: &str, limit: Option<Duration>) {
    write_record(
        TAG_WAIT,
        ANSI_WAIT,
        6,
        Level::Info,
        &format!("{label} (timeout {})", format_limit(limit)),
    );
}

/// `[ WAIT ] <frame> 2s/3s label`: the character after the tag is the
/// spinning frame itself and doubles as the `-` placeholder.
fn wait_line(stamp: &str, frame: char, elapsed: Duration, limit: &str, label: &str) -> String {
    format!(
        "{stamp} {TAG_WAIT} {frame} {}s/{limit} {label}",
        elapsed.as_secs()
    )
}

/// `3s` for a bounded wait, `inf` when there is no timeout.
fn format_limit(limit: Option<Duration>) -> String {
    match limit {
        Some(limit) => format!("{}s", limit.as_secs()),
        None => "inf".to_string(),
    }
}

/// A bounded wait: `[ WAIT ] - 0s/3s label` with a spinner on a terminal.
///
/// Call `tick()` from the wait loop, then `finish()` before logging the
/// outcome. Waits shorter than [`WAIT_GRACE`] stay silent. On a terminal the
/// line is redrawn in place and erased at the end; elsewhere one plain line is
/// logged (and also recorded in kmsg/the log file) once the grace passed.
pub struct Wait {
    label: String,
    /// `None` means no upper bound; the line then shows `0s/inf`.
    limit: Option<Duration>,
    started: Instant,
    last_draw: Instant,
    frame: usize,
    tty: bool,
    /// Whether the wait outlived [`WAIT_GRACE`] and was announced.
    announced: bool,
    /// Whether an animated line is on screen and must be erased.
    drawn: bool,
    /// Set once the line has been erased so `Drop` does not erase it twice.
    done: bool,
}

impl Wait {
    /// `limit: None` starts an unbounded wait (rendered as `0s/inf`).
    pub fn start(label: impl Into<String>, limit: Option<Duration>) -> Self {
        let tty = stderr_is_tty();
        Self {
            label: label.into(),
            limit,
            started: Instant::now(),
            last_draw: Instant::now() - WAIT_REDRAW,
            frame: 0,
            tty,
            announced: false,
            drawn: false,
            done: false,
        }
    }

    /// Announce the wait after [`WAIT_GRACE`] and redraw at most every
    /// [`WAIT_REDRAW`]; call it from the wait loop.
    pub fn tick(&mut self) {
        if self.done {
            return;
        }
        if !self.announced {
            if self.started.elapsed() < WAIT_GRACE {
                return;
            }
            self.announced = true;
            if self.tty {
                self.draw();
                self.drawn = true;
            } else {
                wait_notice(&self.label.clone(), self.limit);
            }
            return;
        }
        if self.tty && self.last_draw.elapsed() >= WAIT_REDRAW {
            self.draw();
        }
    }

    /// Erase the animated line (on a terminal) before the caller logs a result.
    pub fn finish(mut self) {
        self.erase();
    }

    fn draw(&mut self) {
        // The character right after the tag is the animation frame itself;
        // it also serves as the placeholder in `[ WAIT ] - 0s/3s label`.
        let spin = SPINNER[self.frame % SPINNER.len()];
        self.frame += 1;
        self.last_draw = Instant::now();
        let stamp = stamp();
        let limit = format_limit(self.limit);
        let line = wait_line(
            &stamp,
            spin,
            self.started.elapsed(),
            limit.as_str(),
            &self.label,
        );
        let mut out = String::with_capacity(line.len() + 24);
        out.push_str("\r\x1b[2K");
        if log_colors() {
            out.push_str(&format!(
                "\x1b[{ANSI_DIM}m{stamp}\x1b[0m \x1b[{ANSI_WAIT}m{TAG_WAIT}\x1b[0m {spin} {}s/{limit} {}",
                self.started.elapsed().as_secs(),
                self.label
            ));
        } else {
            out.push_str(&line);
        }
        let _ = std::io::stderr().write_all(out.as_bytes());
        let _ = std::io::stderr().flush();
    }

    fn erase(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if self.tty && self.drawn {
            let _ = std::io::stderr().write_all(b"\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        self.erase();
    }
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::log::trace(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::log::debug(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log::info(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! mesg {
    ($($arg:tt)*) => {
        $crate::log::mesg(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log::warn(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::log::error(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! crit {
    ($($arg:tt)*) => {
        $crate::log::crit(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! succ {
    ($($arg:tt)*) => {
        $crate::log::succ(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! fail {
    ($($arg:tt)*) => {
        $crate::log::fail(&format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_color_modes() {
        assert_eq!(Color::parse("auto"), Some(Color::Auto));
        assert_eq!(Color::parse(" ALWAYS "), Some(Color::Always));
        assert_eq!(Color::parse("never"), Some(Color::Never));
        assert_eq!(Color::parse("sometimes"), None);
    }

    #[test]
    fn parses_levels_and_orders_them() {
        assert_eq!(Level::parse("TRACE"), Some(Level::Trace));
        assert_eq!(Level::parse("debug"), Some(Level::Debug));
        assert_eq!(Level::parse("notice"), Some(Level::Message));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("err"), Some(Level::Error));
        assert_eq!(Level::parse("crit"), Some(Level::Critical));
        assert_eq!(Level::parse("nonsense"), None);
        assert!(Level::Trace < Level::Debug);
        assert!(Level::Info < Level::Message);
        assert!(Level::Message < Level::Warn);
        assert!(Level::Warn < Level::Error);
        assert!(Level::Error < Level::Critical);
    }

    #[test]
    fn tags_are_six_characters_wide() {
        for tag in [
            TAG_SUCC, TAG_INFO, TAG_WARN, TAG_MESG, TAG_DEBUG, TAG_TRACE, TAG_FAIL, TAG_ERROR,
            TAG_CRIT, TAG_WAIT,
        ] {
            assert!(tag.starts_with('[') && tag.ends_with(']'), "{tag}");
            assert_eq!(tag.chars().count(), 8, "{tag}");
        }
    }

    #[test]
    fn threshold_filters_lower_levels() {
        set_level(Level::Warn);
        assert!(Level::Message < level());
        assert!(Level::Warn >= level());
        set_level(Level::Info);
        assert!(Level::Debug < level());
        assert!(Level::Info >= level());
    }

    #[test]
    fn wait_line_puts_the_frame_right_after_the_tag() {
        let line = wait_line(
            "[    2.000]",
            '/',
            Duration::from_secs(2),
            "3s",
            "stopping sessions",
        );
        assert_eq!(line, "[    2.000] [ WAIT ] / 2s/3s stopping sessions");
        let line = wait_line(
            "[    2.000]",
            '-',
            Duration::from_secs(2),
            "inf",
            "waiting forever",
        );
        assert!(line.contains("2s/inf waiting forever"), "{line}");
    }

    #[test]
    fn formats_wait_limits() {
        assert_eq!(format_limit(Some(Duration::from_secs(3))), "3s");
        assert_eq!(format_limit(None), "inf");
    }

    #[test]
    fn wait_renders_without_a_terminal() {
        // The test harness has no tty on stderr, so this exercises the static
        // path and must not panic.
        let wait = Wait::start("test wait", Some(Duration::from_secs(3)));
        assert!(!wait.tty);
        drop(wait);
    }
}
