//! Per-kind session-store scanners for the 6 never-reporting agent kinds.
//!
//! Each scanner returns newest-first [`Candidate`]s: workspace matches first,
//! then other recent sessions as a fallback. Shared guardrails (ported from
//! the fork's musescan): newest-first scan cap, max matched / max recent
//! caps, 60-day age cutoff, 1 MiB read heads, all I/O failures skip, never
//! raise, and session-id validation before any id touches a path.
//!
//! Only `muse` is verified (ported as-is from musescan). The other five are
//! **experimental** (U1-U5): their CLIs and stores could not be verified
//! locally, so schemas are best-effort guesses with fixture-based tests.
//! Scanners never run during auto-sweep — only in the manual confirm flow.

pub mod amp;
pub mod cline;
pub mod gemini;
pub mod kiro;
pub mod maki;
pub mod muse;

pub const MAX_MATCHED: usize = 6;
pub const MAX_RECENT: usize = 4;
pub const MAX_CANDIDATE_AGE_SECONDS: f64 = 60.0 * 86400.0;
pub const SCAN_LIMIT: usize = 120;
pub const LOG_HEAD_BYTES: usize = 1024 * 1024;

/// One discovered session: `matched` means it belongs to this workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub mtime: f64,
    pub matched: bool,
}

/// Kinds with a scanner (the 6 herdr never reports sessions for).
pub const SCANNER_KINDS: &[&str] = &["muse", "gemini", "cline", "kiro", "maki", "amp"];

/// Candidates for `kind`, or None when no scanner covers it.
pub fn candidates_for(kind: &str, cwd: &str) -> Option<Vec<Candidate>> {
    match kind {
        "muse" => Some(muse::candidates(cwd, None)),
        "gemini" => Some(gemini::candidates(cwd, None)),
        "cline" => Some(cline::candidates(cwd, None)),
        "kiro" => Some(kiro::candidates(cwd, None)),
        "maki" => Some(maki::candidates(cwd, None)),
        "amp" => Some(amp::candidates(cwd)),
        _ => None,
    }
}

/// Strict existence check for a manually entered (pasted/typed) session id.
/// `base` overrides the scanner's store root (tests); None uses the real one.
/// Returns None for kinds with no verifiable local store — amp (server-side
/// threads) and non-scanner kinds, which never reach the paste prompt — where
/// the caller keeps the generic `valid_session_value` check.
pub fn session_exists(kind: &str, value: &str, base: Option<&std::path::Path>) -> Option<bool> {
    match kind {
        "muse" => Some(muse::session_exists(value, base)),
        "gemini" => Some(gemini::session_exists(value, base)),
        "cline" => Some(cline::session_exists(value, base)),
        "kiro" => Some(kiro::session_exists(value, base)),
        "maki" => Some(maki::session_exists(value, base)),
        _ => None,
    }
}

/// Current Unix time in seconds.
pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn home_dir() -> std::path::PathBuf {
    match std::env::var("HOME").ok().filter(|s| !s.is_empty()) {
        Some(h) => std::path::PathBuf::from(h),
        None => std::path::PathBuf::from("/tmp"),
    }
}

/// mtime of `path` in seconds, or 0.0 when it cannot be stated.
pub fn mtime_secs(path: &std::path::Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// First [`LOG_HEAD_BYTES`] of a file, or empty when unreadable.
pub fn read_head(path: &std::path::Path) -> Vec<u8> {
    use std::io::Read;
    let mut buf = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(LOG_HEAD_BYTES as u64).read_to_end(&mut buf);
    }
    buf
}

/// Sort newest-first and apply the scan cap, so callers only read file
/// heads for sessions that can actually become candidates.
pub fn prescan(mut found: Vec<(String, f64)>) -> Vec<(String, f64)> {
    found.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    found.truncate(SCAN_LIMIT);
    found
}

/// Apply the shared newest-first scan cap, 60-day age cutoff, and
/// matched/recent caps to pre-sorted (newest-first) `(id, mtime)` entries.
pub fn select(mut found: Vec<(String, f64, bool)>, cutoff: f64) -> Vec<Candidate> {
    found.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut matched = Vec::new();
    let mut recent = Vec::new();
    for (id, mtime, is_match) in found.into_iter().take(SCAN_LIMIT) {
        if mtime < cutoff {
            continue;
        }
        if !crate::history::valid_session_id(&id) {
            continue;
        }
        let entry = Candidate {
            id,
            mtime,
            matched: is_match,
        };
        if is_match {
            if matched.len() < MAX_MATCHED {
                matched.push(entry);
            }
        } else if recent.len() < MAX_RECENT {
            recent.push(entry);
        }
    }
    matched.extend(recent);
    matched
}
