//! Optional history sources: read an agent's own session files for last activity.
//!
//! Port of `shelf/history.py` (claude + codex readers), plus cheap v1
//! readers for the scanner kinds whose formats are trivially parseable
//! timestamped JSON/JSONL (gemini, muse, kiro, maki; see U9). All new
//! readers are **experimental**: their schemas are unverified (U3/U4).

use crate::util::{Ts, parse_iso};
use std::path::{Path, PathBuf};

/// Session ids end up in filesystem paths, so a malformed one must not
/// escape the expected directory. Fullmatch semantics (a trailing `\n` must
/// not sneak through).
pub fn valid_session_id(session_id: &str) -> bool {
    let bytes = session_id.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    if !(bytes[0].is_ascii_alphanumeric()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn claude_home() -> PathBuf {
    match std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(d) => PathBuf::from(d),
        None => home_dir().join(".claude"),
    }
}

pub fn codex_home() -> PathBuf {
    match std::env::var("CODEX_HOME").ok().filter(|s| !s.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => home_dir().join(".codex"),
    }
}

pub fn home_dir() -> PathBuf {
    match std::env::var("HOME").ok().filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from("/tmp"),
    }
}

/// The match with the latest mtime, or None. A match that vanished between
/// listing and stat is skipped rather than raising.
fn newest(paths: Vec<PathBuf>) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for p in paths {
        let mtime = match std::fs::metadata(&p).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let replace = match &best {
            None => true,
            Some((_, b)) => mtime > *b,
        };
        if replace {
            best = Some((p, mtime));
        }
    }
    best.map(|(p, _)| p)
}

fn read_dir_files(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn claude_session_file(session_id: &str) -> Option<PathBuf> {
    if !valid_session_id(session_id) {
        return None;
    }
    let projects = claude_home().join("projects");
    let mut matches = Vec::new();
    for slug in read_dir_files(&projects) {
        if !slug.is_dir() {
            continue;
        }
        let cand = slug.join(format!("{session_id}.jsonl"));
        if cand.is_file() {
            matches.push(cand);
        }
    }
    newest(matches)
}

/// The session file plus its companion directory, when those exist.
pub fn claude_session_paths(session_id: &str) -> Vec<PathBuf> {
    let Some(file) = claude_session_file(session_id) else {
        return Vec::new();
    };
    let mut paths = vec![file.clone()];
    let companion = file.with_extension("");
    if companion.is_dir() {
        paths.push(companion);
    }
    paths
}

fn walk_rollouts(dir: &Path, session_id: &str, out: &mut Vec<PathBuf>) {
    for entry in read_dir_files(dir) {
        if entry.is_dir() {
            walk_rollouts(&entry, session_id, out);
        } else if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("rollout-") && name.ends_with(&format!("-{session_id}.jsonl")) {
                out.push(entry);
            }
        }
    }
}

pub fn codex_session_file(session_id: &str) -> Option<PathBuf> {
    if !valid_session_id(session_id) {
        return None;
    }
    let mut matches = Vec::new();
    walk_rollouts(&codex_home().join("sessions"), session_id, &mut matches);
    newest(matches)
}

fn last_timestamp(
    path: &Path,
    keep: impl Fn(&serde_json::Map<String, serde_json::Value>) -> bool,
) -> Option<Ts> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut latest: Option<Ts> = None;
    for line in text.split('\n') {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let obj = match entry.as_object() {
            Some(o) => o,
            None => continue,
        };
        if !keep(obj) {
            continue;
        }
        if let Some(ts) = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| parse_iso(Some(s)))
        {
            if latest.is_none_or(|l| ts > l) {
                latest = Some(ts);
            }
        }
    }
    latest
}

fn claude_keep(entry: &serde_json::Map<String, serde_json::Value>) -> bool {
    matches!(
        entry.get("type").and_then(|v| v.as_str()),
        Some("user" | "assistant")
    ) && entry.get("isMeta").and_then(|v| v.as_bool()) != Some(true)
        && entry.get("isSidechain").and_then(|v| v.as_bool()) != Some(true)
}

fn codex_keep(entry: &serde_json::Map<String, serde_json::Value>) -> bool {
    matches!(
        entry.get("type").and_then(|v| v.as_str()),
        Some("response_item" | "event_msg")
    )
}

pub fn claude_last(session_id: &str) -> Option<Ts> {
    claude_session_file(session_id).and_then(|p| last_timestamp(&p, claude_keep))
}

pub fn codex_last(session_id: &str) -> Option<Ts> {
    codex_session_file(session_id).and_then(|p| last_timestamp(&p, codex_keep))
}

// --- v1 additions (experimental, U9) ---
//
// Each reader below follows the same shape: locate the kind's session file
// for `session_id` (reusing the scanner's base-dir logic), then take the
// latest parseable timestamp. Any failure returns None.

/// EXPERIMENTAL (U3): gemini `session-*.json` schema unverified.
pub fn gemini_last(session_id: &str) -> Option<Ts> {
    let path = crate::scan::gemini::session_file(session_id)?;
    gemini_file_last(&path)
}

fn gemini_file_last(path: &Path) -> Option<Ts> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Candidate timestamp fields, newest wins.
    let mut best: Option<Ts> = None;
    for key in [
        "lastUpdated",
        "updatedAt",
        "timestamp",
        "createdAt",
        "startTime",
    ] {
        if let Some(ts) = v
            .get(key)
            .and_then(|x| x.as_str())
            .and_then(|s| parse_iso(Some(s)))
        {
            if best.is_none_or(|b| ts > b) {
                best = Some(ts);
            }
        }
        // Numeric epoch-millis form.
        if let Some(ms) = v.get(key).and_then(|x| x.as_i64()) {
            let ts = Ts(ms * 1000);
            if best.is_none_or(|b| ts > b) {
                best = Some(ts);
            }
        }
    }
    // Fall back to message-level timestamps.
    if best.is_none() {
        if let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) {
            for m in msgs {
                if let Some(ts) = m
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .and_then(|s| parse_iso(Some(s)))
                {
                    if best.is_none_or(|b| ts > b) {
                        best = Some(ts);
                    }
                }
            }
        }
    }
    best.or_else(|| file_mtime_ts(path))
}

/// EXPERIMENTAL: muse `session.jsonl` message timestamps.
pub fn muse_last(session_id: &str) -> Option<Ts> {
    let path = crate::scan::muse::session_file(session_id)?;
    // Muse session.jsonl lines carry their own envelope; accept any line
    // with a parseable `timestamp`.
    last_timestamp(&path, |_| true).or_else(|| file_mtime_ts(&path))
}

/// EXPERIMENTAL: kiro `~/.kiro/sessions/cli/<uuid>.json` metadata timestamps.
pub fn kiro_last(session_id: &str) -> Option<Ts> {
    let path = crate::scan::kiro::session_file(session_id)?;
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut best: Option<Ts> = None;
    for key in ["updatedAt", "lastUpdated", "timestamp", "modifiedAt"] {
        if let Some(ts) = v
            .get(key)
            .and_then(|x| x.as_str())
            .and_then(|s| parse_iso(Some(s)))
        {
            if best.is_none_or(|b| ts > b) {
                best = Some(ts);
            }
        }
    }
    best.or_else(|| file_mtime_ts(&path))
}

/// EXPERIMENTAL (U4): maki session JSONL timestamps.
pub fn maki_last(session_id: &str) -> Option<Ts> {
    let path = crate::scan::maki::session_file(session_id)?;
    last_timestamp(&path, |_| true).or_else(|| file_mtime_ts(&path))
}

fn file_mtime_ts(path: &Path) -> Option<Ts> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(Ts(dur.as_micros().min(i64::MAX as u128) as i64))
}

/// Last activity from the agent's own files, or None if there is no reader
/// or it failed.
pub fn last_activity(agent: &str, session_value: &str) -> Option<Ts> {
    match agent {
        "claude" => claude_last(session_value),
        "codex" => codex_last(session_value),
        "gemini" => gemini_last(session_value),
        "muse" => muse_last(session_value),
        "kiro" => kiro_last(session_value),
        "maki" => maki_last(session_value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_validation() {
        assert!(valid_session_id("abc-123_X.y"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../x"));
        assert!(!valid_session_id("a/b"));
        assert!(!valid_session_id("abc\n"));
        assert!(!valid_session_id("-abc"));
        assert!(!valid_session_id(&"x".repeat(129)));
        assert!(valid_session_id(&"x".repeat(128)));
    }
}
