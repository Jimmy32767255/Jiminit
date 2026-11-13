//! Minimal logger.
//!
//! Log records go to stderr (which, for PID 1, is the kernel console) and to
//! `/dev/kmsg` when available, so messages also show up in `dmesg` and on
//! serial consoles that the kernel uses for printk. Once the config is loaded
//! stage2 also appends to a size-capped file (default `/var/log/jiminit.log`).

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Rotate the log file once it would grow past this size.
const MAX_FILE_BYTES: u64 = 1 << 20;

static KMSG: OnceLock<Option<Mutex<File>>> = OnceLock::new();

struct FileSink {
    file: File,
    path: PathBuf,
    written: u64,
    dead: bool,
}

static LOG_FILE: OnceLock<Option<Mutex<FileSink>>> = OnceLock::new();

/// Open the kernel log device once. Safe to call multiple times.
pub fn init() {
    KMSG.get_or_init(|| {
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/kmsg")
            .ok()
            .map(Mutex::new)
    });
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

fn emit(level: u8, msg: &str) {
    // PID 1 must never panic because a log target is broken (for example the
    // console disappeared), so ignore write errors instead of using eprintln!,
    // which panics if writing to stderr fails.
    let _ = writeln!(std::io::stderr(), "jiminit: {msg}");
    if let Some(Some(kmsg)) = KMSG.get() {
        if let Ok(mut file) = kmsg.lock() {
            // /dev/kmsg treats every write() as one record, so build the full
            // line first instead of using write!/writeln! (multiple writes).
            let record = format!("<{level}>jiminit: {msg}\n");
            let _ = file.write_all(record.as_bytes());
        }
    }
    if let Some(Some(sink)) = LOG_FILE.get() {
        if let Ok(mut sink) = sink.lock() {
            sink.write(&format!("jiminit: {msg}\n"));
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
    emit(6, msg);
}

pub fn warn(msg: &str) {
    emit(4, msg);
}

pub fn err(msg: &str) {
    emit(3, msg);
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log::info(&format!($($arg)*))
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
