//! Locate muse sessions on disk by workspace.
//!
//! Port of `shelf/musescan.py` (verified behavior).

use super::{
    Candidate, MAX_CANDIDATE_AGE_SECONDS, mtime_secs, now_secs, prescan, read_head, select,
};
use std::path::{Path, PathBuf};

pub fn sessions_base() -> PathBuf {
    match std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(d) => PathBuf::from(d).join("muse").join("sessions"),
        None => super::home_dir()
            .join(".local")
            .join("share")
            .join("muse")
            .join("sessions"),
    }
}

fn mentions_workspace(log_path: &Path, cwd: &str) -> bool {
    let head = read_head(log_path);
    let needle = format!("\"workspace_root\":\"{cwd}\"");
    head.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn session_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let years = match std::fs::read_dir(root) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect::<Vec<_>>(),
        Err(_) => return found,
    };
    for year in years {
        let months = match std::fs::read_dir(&year) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect::<Vec<_>>(),
            Err(_) => continue,
        };
        for month in months {
            let days = match std::fs::read_dir(&month) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir())
                    .collect::<Vec<_>>(),
                Err(_) => continue,
            };
            for day in days {
                let ids = match std::fs::read_dir(&day) {
                    Ok(rd) => rd
                        .filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.is_dir())
                        .collect::<Vec<_>>(),
                    Err(_) => continue,
                };
                for sid in ids {
                    if sid.join("session.jsonl").is_file() {
                        found.push((sid.file_name().unwrap().to_string_lossy().into_owned(), sid));
                    }
                }
            }
        }
    }
    found
}

/// Newest-first session candidates: workspace matches first, then other
/// recent sessions as a fallback.
pub fn candidates(cwd: &str, base: Option<&Path>) -> Vec<Candidate> {
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    let found: Vec<(String, f64)> = session_dirs(&root)
        .into_iter()
        .map(|(sid, path)| {
            let mtime = mtime_secs(&path.join("session.jsonl"));
            (sid, mtime)
        })
        .collect();
    // Re-resolve paths for the scanned subset (avoids keeping them all).
    let scanned = prescan(found);
    let cutoff = now_secs() - MAX_CANDIDATE_AGE_SECONDS;
    let mut with_match = Vec::new();
    for (sid, mtime) in &scanned {
        if *mtime < cutoff {
            continue;
        }
        let log = find_log(&root, sid);
        let matched = log.as_ref().is_some_and(|p| mentions_workspace(p, cwd));
        with_match.push((sid.clone(), *mtime, matched));
    }
    select(with_match, cutoff)
}

fn find_log(root: &Path, sid: &str) -> Option<PathBuf> {
    for (id, path) in session_dirs(root) {
        if id == *sid {
            return Some(path.join("session.jsonl"));
        }
    }
    None
}

/// The session.jsonl path for an exact session id (for history readers).
pub fn session_file(session_id: &str) -> Option<PathBuf> {
    if !crate::history::valid_session_id(session_id) {
        return None;
    }
    find_log(&sessions_base(), session_id)
}

/// Strict existence check for a manually entered (pasted/typed) session ref:
/// true when `value` is the id of a session dir holding a session.jsonl, or
/// a Session Name currently claimed by one (see [`resolve_name`).
/// `base` overrides the store root (tests); None uses the real one.
pub fn session_exists(value: &str, base: Option<&Path>) -> bool {
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    if crate::history::valid_session_id(value) && find_log(&root, value).is_some() {
        return true;
    }
    // Names never touch a path (string comparison only), so the generic
    // value check is the only gate before the scan.
    if !crate::agents::valid_session_value("muse", value) {
        return false;
    }
    resolve_name_in(&root, value).is_some()
}

/// Resolve a muse Session Name (`muse resume` accepts a UUID or a Name) to
/// its session id. Names live in no metafile: each session's session.jsonl
/// carries `session.name.changed` events (`payload.new_name`), and the
/// latest one wins (renames append). Case-insensitive, matching muse's own
/// NOCASE name matching. Only raw (unwrapped) log lines are parsed —
/// `record_json`-wrapped lines occur only in `subagent/` logs, which
/// discovery does not walk.
pub fn resolve_name(name: &str, base: Option<&Path>) -> Option<String> {
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    resolve_name_in(&root, name)
}

fn resolve_name_in(root: &Path, name: &str) -> Option<String> {
    let want = name.to_lowercase();
    if want.is_empty() {
        return None;
    }
    for (sid, dir) in session_dirs(root) {
        let Ok(bytes) = std::fs::read(dir.join("session.jsonl")) else {
            continue;
        };
        if latest_name(&bytes).as_deref() == Some(want.as_str()) {
            return Some(sid);
        }
    }
    None
}

/// The latest Session Name claimed in one session log: the last `"new_name"`
/// value on a `session.name.changed` line. Byte-oriented — only the winning
/// token is JSON-unescaped — so huge logs cost a scan, not a parse.
fn latest_name(bytes: &[u8]) -> Option<String> {
    let key = b"\"new_name\"";
    let mut end = bytes.len();
    while let Some(found) = rfind(&bytes[..end], key) {
        end = found;
        if let Some(name) = name_at(bytes, found)
            && changed_on_line(bytes, found)
        {
            return Some(name);
        }
    }
    None
}

/// The `"new_name"` value at a key match: `"new_name" <ws> : <ws> "value"`,
/// JSON-unescaped. None when the match is not shaped like that.
fn name_at(bytes: &[u8], at: usize) -> Option<String> {
    let mut i = at + b"\"new_name\"".len();
    while bytes
        .get(i)
        .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        i += 1;
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while bytes
        .get(i)
        .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    let mut escaped = false;
    while let Some(&b) = bytes.get(j) {
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == b'"' {
            break;
        }
        j += 1;
    }
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    let token = std::str::from_utf8(&bytes[i..=j]).ok()?;
    let name: String = serde_json::from_str(token).ok()?;
    Some(name.to_lowercase())
}

/// Whether the log line holding a `"new_name"` match is a
/// `session.name.changed` record (guards against a future collision with an
/// unrelated same-named key).
fn changed_on_line(bytes: &[u8], at: usize) -> bool {
    let start = bytes[..at]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |p| p + 1);
    let end = bytes[at..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |p| at + p);
    rfind(&bytes[start..end], b"session.name.changed").is_some()
}

/// Last occurrence of `needle` in `haystack` (byte offset), if any.
fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn missing_base_yields_nothing() {
        let (_g, dir) = testutil::tempdir();
        assert!(candidates("/work/a", Some(&dir.join("nonexistent"))).is_empty());
    }

    #[test]
    fn match_first_newest_first() {
        let (_g, dir) = testutil::tempdir();
        // sessions/2026/01/02/<id>/session.jsonl
        for (id, ws, old) in [
            ("id-old-match", "/work/a", true),
            ("id-new-nomatch", "/elsewhere", false),
            ("id-new-match", "/work/a", false),
        ] {
            let d = dir.join("2026").join("01").join("02").join(id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("session.jsonl"),
                format!("{{\"workspace_root\":\"{ws}\"}}\n"),
            )
            .unwrap();
            if old {
                testutil::backdate(&d.join("session.jsonl"), 61 * 86400);
            }
        }
        let cands = candidates("/work/a", Some(&dir));
        // old match is past the 60-day cutoff
        assert_eq!(cands.len(), 2);
        assert!(cands[0].matched);
        assert_eq!(cands[0].id, "id-new-match");
        assert!(!cands[1].matched);
    }

    fn write_log(dir: &Path, id: &str, lines: &[&str]) -> PathBuf {
        let d = dir.join("2026").join("01").join("02").join(id);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("session.jsonl");
        std::fs::write(&p, lines.join("\n")).unwrap();
        d
    }

    fn named(id: &str, name: &str) -> String {
        format!(
            r#"{{"id":"e1","payload_type":"session.name.changed","payload":{{"new_name":"{name}","previous_name":null,"session_id":"{id}"}}}}"#
        )
    }

    #[test]
    fn exists_accepts_uuid_with_log_rejects_garbage_and_missing() {
        let (_g, dir) = testutil::tempdir();
        let uuid = "01a0da74-9f52-7760-a301-60a6f87abcf5";
        write_log(&dir, uuid, &[r#"{"workspace_root":"/work/a"}"#]);
        // dir without session.jsonl is not a session
        std::fs::create_dir_all(dir.join("2026").join("01").join("02").join("no-log")).unwrap();
        assert!(session_exists(uuid, Some(&dir)));
        assert!(!session_exists("uq", Some(&dir))); // the live failure: 2-char garbage
        assert!(!session_exists(
            "00000000-0000-4000-8000-000000000000",
            Some(&dir)
        ));
        assert!(!session_exists("no-log", Some(&dir)));
        assert!(!session_exists("-bad", Some(&dir)));
        assert!(!session_exists("", Some(&dir)));
    }

    #[test]
    fn exists_resolves_session_names_latest_wins_case_insensitive() {
        let (_g, dir) = testutil::tempdir();
        let uuid = "01a0da74-9f52-7760-a301-60a6f87abcf5";
        write_log(
            &dir,
            uuid,
            &[
                r#"{"payload_type":"session.opened.observed"}"#,
                &named(uuid, "old-name"),
                &named(uuid, "gentle-pisces"),
            ],
        );
        assert!(session_exists("gentle-pisces", Some(&dir)));
        assert!(session_exists("Gentle-Pisces", Some(&dir)));
        // stale previous name no longer resolves
        assert!(!session_exists("old-name", Some(&dir)));
        assert_eq!(
            resolve_name("gentle-pisces", Some(&dir)).as_deref(),
            Some(uuid)
        );
        assert_eq!(
            resolve_name("GENTLE-pisces", Some(&dir)).as_deref(),
            Some(uuid)
        );
        assert_eq!(resolve_name("uq", Some(&dir)), None);
    }

    #[test]
    fn name_match_requires_a_name_changed_line() {
        // A colliding `"new_name"` key on some other record type is ignored.
        let (_g, dir) = testutil::tempdir();
        write_log(
            &dir,
            "sid-1",
            &[r#"{"payload_type":"runtime.session","payload":{"new_name":"tricky"}}"#],
        );
        assert!(!session_exists("tricky", Some(&dir)));
        assert_eq!(resolve_name("tricky", Some(&dir)), None);
    }
}
