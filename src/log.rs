//! Minimal file+stderr logger matching the Python shelf's log format.
//!
//! Format: `<asctime> <LEVEL> [<session>] <message>` where asctime is local
//! time like `2026-10-08 12:34:56,789`. Level names mirror Python's logging
//! (`INFO`, `WARNING`, `ERROR`).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

struct Inner {
    session: String,
    log_path: Option<PathBuf>,
    file: Option<File>,
    stderr: bool,
}

impl Inner {
    fn new() -> Self {
        Inner {
            session: "unknown".to_string(),
            log_path: None,
            file: None,
            stderr: true,
        }
    }
}

static LOGGER: OnceLock<Mutex<Inner>> = OnceLock::new();

fn logger() -> &'static Mutex<Inner> {
    LOGGER.get_or_init(|| Mutex::new(Inner::new()))
}

/// Local-time timestamp like Python's `%(asctime)s`: `2026-10-08 12:34:56,789`.
fn asctime_now() -> String {
    let now = std::time::SystemTime::now();
    let dur = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as libc::time_t;
    let millis = dur.subsec_millis();
    let mut out = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&secs, &mut out) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02},{:03}",
        out.tm_year + 1900,
        out.tm_mon + 1,
        out.tm_mday,
        out.tm_hour,
        out.tm_min,
        out.tm_sec,
        millis
    )
}

/// Configure logging for this invocation: rotate `herdr-archive.log` past 1 MiB,
/// remember the session tag. The file itself opens lazily on first emit.
pub fn setup(state_dir: &Path, session_name: &str) {
    let mut inner = logger().lock().unwrap();
    inner.session = session_name.to_string();
    inner.stderr = true;
    if std::fs::create_dir_all(state_dir).is_ok() {
        let log_path = state_dir.join("herdr-archive.log");
        rotate_if_large(&log_path);
        inner.log_path = Some(log_path);
        inner.file = None;
    }
}

/// Rotate `path` to `path.1` when it is larger than 1 MiB. Never fails.
pub fn rotate_if_large(path: &Path) {
    match std::fs::metadata(path) {
        Ok(md) if md.len() > 1_000_000 => {
            let backup = path.with_file_name(format!(
                "{}.1",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("herdr-archive.log")
            ));
            let _ = std::fs::rename(path, backup);
        }
        _ => {}
    }
}

/// Show or hide log lines on stderr (popups hide them; the file keeps all).
pub fn set_stderr(enabled: bool) {
    logger().lock().unwrap().stderr = enabled;
}

pub fn emit(level: &str, message: &str) {
    let line = format!(
        "{} {} [{}] {}\n",
        asctime_now(),
        level,
        session_tag(),
        message
    );
    let mut inner = logger().lock().unwrap();
    if inner.file.is_none() {
        if let Some(path) = inner.log_path.clone() {
            inner.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok();
        }
    }
    if let Some(file) = inner.file.as_mut() {
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }
    if inner.stderr {
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

fn session_tag() -> String {
    logger().lock().unwrap().session.clone()
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::emit("INFO", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::emit("WARNING", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::emit("ERROR", &format!($($arg)*))
    };
}
