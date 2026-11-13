//! Minimal logger.
//!
//! Log records go to stderr (which, for PID 1, is the kernel console) and to
//! `/dev/kmsg` when available, so messages also show up in `dmesg` and on
//! serial consoles that the kernel uses for printk.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static KMSG: OnceLock<Option<Mutex<File>>> = OnceLock::new();

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

fn emit(level: u8, msg: &str) {
    eprintln!("jiminit: {msg}");
    if let Some(Some(kmsg)) = KMSG.get() {
        if let Ok(mut file) = kmsg.lock() {
            // /dev/kmsg treats every write() as one record, so build the full
            // line first instead of using write!/writeln! (multiple writes).
            let record = format!("<{level}>jiminit: {msg}\n");
            let _ = file.write_all(record.as_bytes());
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
