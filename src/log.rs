//! Minimal logger.
//!
//! Log records go to stderr (which, for PID 1, is the kernel console) and to
//! `/dev/kmsg` when available, so messages also show up in `dmesg` and on
//! serial consoles that the kernel uses for printk. Once the config is loaded
//! stage2 also appends to a size-capped file (default `/var/log/jiminit.log`).
//!
//! Every line looks like `[   12.345] [  OK  ] message`: the first column is
//! the time since jiminit started, the second is a fixed-width status tag.
//! Colors are only written to terminals and never reach kmsg or the log file.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Rotate the log file once it would grow past this size.
const MAX_FILE_BYTES: u64 = 1 << 20;

const TAG_OK: &str = "[  OK  ]";
const TAG_INFO: &str = "[ INFO ]";
const TAG_WARN: &str = "[ WARN ]";
const TAG_FAIL: &str = "[ FAIL ]";

const ANSI_OK: &str = "1;32";
const ANSI_WARN: &str = "1;33";
const ANSI_FAIL: &str = "1;31";
const ANSI_DIM: &str = "2";

static KMSG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static LOG_FILE: OnceLock<Option<Mutex<FileSink>>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static COLOR_MODE: AtomicU8 = AtomicU8::new(0);

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

/// Wrap `text` in an SGR sequence when stdout colors are enabled.
pub fn paint_stdout(text: &str, code: &str) -> String {
    if stdout_colors() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The green `[  OK  ]` tag, for query commands that print without a prefix.
pub fn ok_tag() -> String {
    paint_stdout(TAG_OK, ANSI_OK)
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

fn emit(tag: &str, ansi: &str, priority: u8, msg: &str) {
    let stamp = format!("[{:>9.3}]", elapsed());
    let plain = format!("{stamp} {tag} {msg}\n");

    let mut line = String::with_capacity(plain.len() + 16);
    if log_colors() {
        line.push_str(&format!(
            "\x1b[{ANSI_DIM}m{stamp}\x1b[0m \x1b[{ansi}m{tag}\x1b[0m {msg}\n"
        ));
    } else {
        line.push_str(&plain);
    }
    // PID 1 must never panic because a log target is broken (for example the
    // console disappeared), so ignore write errors instead of using eprintln!,
    // which panics if writing to stderr fails.
    let _ = std::io::stderr().write_all(line.as_bytes());

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
            sink.write(&plain);
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

pub fn info(msg: &str) {
    emit(TAG_INFO, "0", 6, msg);
}

pub fn ok(msg: &str) {
    emit(TAG_OK, ANSI_OK, 6, msg);
}

pub fn warn(msg: &str) {
    emit(TAG_WARN, ANSI_WARN, 4, msg);
}

pub fn err(msg: &str) {
    emit(TAG_FAIL, ANSI_FAIL, 3, msg);
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log::info(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! ok {
    ($($arg:tt)*) => {
        $crate::log::ok(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log::warn(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => {
        $crate::log::err(&format!($($arg)*))
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
}
