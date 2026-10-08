//! Small helpers: UTC timestamps, atomic JSON files, and a lock file.
//!
//! Port of `shelf/util.py`. Timestamps are [`Ts`] (microseconds since the
//! Unix epoch); [`iso`] formats them exactly like Python's `iso()`.

use serde_json::Value;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Microseconds since the Unix epoch (UTC). Whole-second formatting on output.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Ts(pub i64);

impl Ts {
    pub fn as_micros(self) -> i64 {
        self.0
    }

    pub fn as_secs(self) -> i64 {
        self.0.div_euclid(1_000_000)
    }
}

/// Current UTC time.
pub fn now() -> Ts {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Ts(dur.as_micros().min(i64::MAX as u128) as i64)
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's days_from_civil).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = month as i64;
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64; // [0, 399]
    let mp = ((m + 9) % 12) as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + (day as u64 - 1); // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe as i64 - 719468
}

/// Inverse of [`days_from_civil`]: (year, month, day).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format like Python: `%Y-%m-%dT%H:%M:%SZ` (whole seconds, UTC).
pub fn iso(ts: Ts) -> String {
    let secs = ts.as_secs();
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

fn digits(s: &str, n: usize) -> Option<u32> {
    if s.len() != n || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok()
}

/// Parse an ISO 8601 timestamp; naive timestamps are UTC. Port of
/// `parse_iso`, including `+HHMM` offsets and fractions of any length.
pub fn parse_iso(text: Option<&str>) -> Option<Ts> {
    let text = text?;
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    let mut s = s.replace('Z', "+00:00");
    // Normalize a trailing +HHMM offset to +HH:MM.
    {
        let b = s.as_bytes();
        if b.len() >= 5 {
            let tail = &b[b.len() - 5..];
            if (tail[0] == b'+' || tail[0] == b'-') && tail[1..].iter().all(|c| c.is_ascii_digit())
            {
                s.insert(b.len() - 2, ':');
            }
        }
    }
    // Normalize the first fraction to exactly 6 digits.
    if let Some(dot) = s.find('.') {
        let rest = &s[dot + 1..];
        let n = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        if n > 0 {
            let mut frac: String = rest[..n].to_string();
            while frac.len() < 6 {
                frac.push('0');
            }
            frac.truncate(6);
            s = format!("{}{}{}", &s[..dot + 1], frac, &rest[n..]);
        }
    }
    // Timestamps are ASCII-only; anything else is invalid (and this makes
    // every byte slice below panic-free).
    if !s.is_ascii() {
        return None;
    }
    parse_normalized(&s)
}

fn parse_normalized(s: &str) -> Option<Ts> {
    // Split off a trailing numeric offset.
    let (main, offset_secs) = split_offset(s)?;
    let (date, time) = match main.find(['T', ' ']) {
        Some(i) => (&main[..i], Some(&main[i + 1..])),
        None => (main, None),
    };
    if date.len() != 10 || &date[4..5] != "-" || &date[7..8] != "-" {
        return None;
    }
    let year: i64 = date[..4].parse().ok()?;
    if !date[..4].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let month = digits(&date[5..7], 2)?;
    let day = digits(&date[8..10], 2)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let (hour, min, sec, micros) = match time {
        None => (0, 0, 0, 0),
        Some(t) => {
            if t.len() < 5 || &t[2..3] != ":" {
                return None;
            }
            let hour = digits(&t[..2], 2)?;
            let min = digits(&t[3..5], 2)?;
            let (sec, micros) = if t.len() > 5 {
                if &t[5..6] != ":" || t.len() < 8 {
                    return None;
                }
                let sec = digits(&t[6..8], 2)?;
                let micros = if t.len() > 8 {
                    if &t[8..9] != "." {
                        return None;
                    }
                    digits(&t[9..], 6)?
                } else {
                    0
                };
                (sec, micros)
            } else {
                (0, 0)
            };
            if hour > 23 || min > 59 || sec > 59 {
                return None;
            }
            (hour, min, sec, micros)
        }
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64 - offset_secs;
    secs.checked_mul(1_000_000)?
        .checked_add(micros as i64)
        .map(Ts)
}

/// Split `+HH:MM` / `-HH:MM` suffix, returning (main, offset seconds east).
fn split_offset(s: &str) -> Option<(&str, i64)> {
    if s.len() >= 6 {
        let tail = &s[s.len() - 6..];
        let b = tail.as_bytes();
        if (b[0] == b'+' || b[0] == b'-') && b[3] == b':' {
            let oh = digits(&tail[1..3], 2)?;
            let om = digits(&tail[4..6], 2)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let secs = oh as i64 * 3600 + om as i64 * 60;
            let rest = &s[..s.len() - 6];
            // A date-only string has no offset; but "2026-08-01" can't match
            // the pattern anyway (needs +/- at position len-6).
            return Some((rest, if b[0] == b'-' { -secs } else { secs }));
        }
    }
    Some((s, 0))
}

/// Read JSON, returning `default` for a missing or invalid file (warning on
/// the latter). Other I/O errors propagate.
pub fn read_json(path: &Path, default: Value) -> io::Result<Value> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(default),
        Err(e) => Err(e),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Ok(v),
            Err(_) => {
                crate::log_warn!("{} is not valid JSON; ignoring it", path.display());
                Ok(default)
            }
        },
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write JSON atomically (temp file + rename + fsync), like Python's
/// `atomic_write_json`. On failure the temp file is removed.
pub fn atomic_write_json(path: &Path, data: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let id = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{}.{}.{}.tmp", name, std::process::id(), id));
    let result = (|| -> io::Result<()> {
        let mut text = serde_json::to_string_pretty(data).map_err(io::Error::other)?;
        text.push('\n');
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
    Ok(())
}

/// Another process holds the lock.
#[derive(Debug)]
pub struct LockBusy(pub PathBuf);

impl fmt::Display for LockBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl std::error::Error for LockBusy {}

#[derive(Debug)]
pub enum LockError {
    Busy(PathBuf),
    Io(io::Error),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Busy(p) => write!(f, "{}", p.display()),
            LockError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::Busy(_) => None,
            LockError::Io(e) => Some(e),
        }
    }
}

impl From<io::Error> for LockError {
    fn from(e: io::Error) -> Self {
        LockError::Io(e)
    }
}

/// Exclusive lock using `flock(2)` on a lock file. The guard releases on drop;
/// the file itself is never deleted.
pub struct FileLock {
    path: PathBuf,
    wait: Duration,
}

pub struct FileGuard {
    /// Held open to keep the flock; closed (releasing the lock) on drop.
    #[allow(dead_code)]
    file: std::fs::File,
}

impl FileLock {
    pub fn new(path: &Path, wait_seconds: f64) -> Self {
        FileLock {
            path: path.to_path_buf(),
            wait: Duration::from_secs_f64(wait_seconds.max(0.0)),
        }
    }

    pub fn lock(&self) -> Result<FileGuard, LockError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.path)?;
        let deadline = Instant::now() + self.wait;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(FileGuard { file });
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                if Instant::now() >= deadline {
                    return Err(LockError::Busy(self.path.clone()));
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            return Err(LockError::Io(err));
        }
    }
}

use std::os::unix::fs::OpenOptionsExt;

/// First 8 characters (Python's `value[:8]`; char-based, panic-free).
pub fn short8(value: &str) -> String {
    value.chars().take(8).collect()
}

/// The machine's local UTC offset in seconds at `ts` (each timestamp's
/// own DST offset, like Python's `astimezone()` with no zone).
pub fn local_offset(ts: Ts) -> i32 {
    let secs = ts.as_secs() as libc::time_t;
    let mut out = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&secs, &mut out) };
    // tm_gmtoff: seconds east of UTC (BSD/GNU extension; unix-only crate).
    out.tm_gmtoff as i32
}

/// Local calendar days since 1970-01-01 for `ts` at UTC offset `offset_secs`.
pub fn local_days_with(ts: Ts, offset_secs: i32) -> i64 {
    (ts.as_secs() + offset_secs as i64).div_euclid(86_400)
}

/// `shelved Dec 3 14:05` in the zone `offset_secs` describes.
pub fn shelved_local(ts: Ts, offset_secs: i32) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let shifted = ts.as_secs() + offset_secs as i64;
    let days = shifted.div_euclid(86_400);
    let sod = shifted.rem_euclid(86_400);
    let (_, m, d) = civil_from_days(days);
    format!(
        "shelved {} {} {:02}:{:02}",
        MONTHS[(m - 1) as usize],
        d,
        sod / 3600,
        (sod % 3600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z_suffix() {
        assert_eq!(parse_iso(Some("2026-09-24T10:15:00Z")), {
            let days = days_from_civil(2026, 9, 24);
            Some(Ts((days * 86_400 + 10 * 3600 + 15 * 60) * 1_000_000))
        });
    }

    #[test]
    fn millis() {
        assert_eq!(
            parse_iso(Some("2026-08-01T05:07:50.770Z")).unwrap().0 % 1_000_000,
            770_000
        );
    }

    #[test]
    fn seven_digit_fraction_truncates() {
        assert_eq!(
            parse_iso(Some("2026-08-01T05:07:50.1234567+00:00"))
                .unwrap()
                .0
                % 1_000_000,
            123_456
        );
    }

    #[test]
    fn offset_converted() {
        let a = parse_iso(Some("2026-08-01T07:00:00+02:00")).unwrap();
        let b = parse_iso(Some("2026-08-01T05:00:00Z")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn offset_without_colon() {
        let a = parse_iso(Some("2026-08-01T07:00:00+0200")).unwrap();
        let b = parse_iso(Some("2026-08-01T05:00:00Z")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn garbage() {
        for bad in [
            None,
            Some(""),
            Some("yesterday"),
            Some("42"),
            Some("2026-13-01T00:00:00Z"),
        ] {
            assert_eq!(parse_iso(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn iso_roundtrip() {
        let ts = parse_iso(Some("2026-09-24T10:15:07Z")).unwrap();
        assert_eq!(iso(ts), "2026-09-24T10:15:07Z");
        assert_eq!(parse_iso(Some(&iso(ts))), Some(ts));
    }

    #[test]
    fn naive_is_utc_midnight_when_date_only() {
        assert_eq!(
            parse_iso(Some("2026-08-01T05:07:50")),
            parse_iso(Some("2026-08-01T05:07:50Z"))
        );
    }
}
