//! Maki session scanner.
//!
//! EXPERIMENTAL (U4): the `maki` CLI is not installed locally; the resume
//! flag, the store layout and whether session JSONL records the cwd are all
//! unverified. Assumed layout:
//! `${XDG_STATE_HOME:-~/.local/state}/maki/sessions/<base58>.jsonl`.
//! The workspace match binds on a cwd-like field in the JSONL head when one
//! is present; otherwise every session is an unmatched recent fallback and
//! the user confirms the pick.

use super::{
    Candidate, MAX_CANDIDATE_AGE_SECONDS, mtime_secs, now_secs, prescan, read_head, select,
};
use std::path::{Path, PathBuf};

pub fn sessions_base() -> PathBuf {
    match std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(d) => PathBuf::from(d).join("maki").join("sessions"),
        None => super::home_dir()
            .join(".local")
            .join("state")
            .join("maki")
            .join("sessions"),
    }
}

/// Candidate workspace-binding fields in a session JSONL line (unverified).
const CWD_FIELDS: &[&str] = &[
    "cwd",
    "workspace",
    "project",
    "root",
    "directory",
    "workspace_root",
];

fn mentions_cwd(path: &Path, cwd: &str) -> bool {
    if cwd.is_empty() {
        return false;
    }
    let head = read_head(path);
    let text = String::from_utf8_lossy(&head);
    for line in text.split('\n') {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(o) = v.as_object() {
            for key in CWD_FIELDS {
                if o.get(*key).and_then(|x| x.as_str()) == Some(cwd) {
                    return true;
                }
            }
        }
    }
    // Last resort: the raw cwd quoted anywhere in the head.
    let needle = format!("\"{cwd}\"");
    head.windows(needle.len()).any(|w| w == needle.as_bytes())
}

pub fn candidates(cwd: &str, base: Option<&Path>) -> Vec<Candidate> {
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for f in rd.filter_map(|e| e.ok().map(|e| e.path())) {
            if f.is_file() && f.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                files.push(f);
            }
        }
    }
    let found: Vec<(String, f64, PathBuf)> = files
        .into_iter()
        .filter_map(|p| {
            let stem = p.file_stem().and_then(|s| s.to_str()).map(str::to_string)?;
            Some((stem, mtime_secs(&p), p))
        })
        .collect();
    let cutoff = now_secs() - MAX_CANDIDATE_AGE_SECONDS;
    let order: Vec<(String, f64)> =
        prescan(found.iter().map(|(id, m, _)| (id.clone(), *m)).collect());
    let by_id: std::collections::HashMap<&str, &PathBuf> =
        found.iter().map(|(id, _, p)| (id.as_str(), p)).collect();
    let mut with_match = Vec::new();
    for (id, mtime) in order {
        if mtime < cutoff {
            continue;
        }
        let matched = by_id.get(id.as_str()).is_some_and(|p| mentions_cwd(p, cwd));
        with_match.push((id, mtime, matched));
    }
    select(with_match, cutoff)
}

/// The session file for an exact session id (for history readers).
pub fn session_file(session_id: &str) -> Option<PathBuf> {
    if !crate::history::valid_session_id(session_id) {
        return None;
    }
    let p = sessions_base().join(format!("{session_id}.jsonl"));
    p.is_file().then_some(p)
}

/// Strict existence check for a manually entered session id.
pub fn session_exists(value: &str, base: Option<&Path>) -> bool {
    if !crate::history::valid_session_id(value) {
        return false;
    }
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    root.join(format!("{value}.jsonl")).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn missing_base_yields_nothing() {
        let (_g, dir) = testutil::tempdir();
        assert!(candidates("/work/a", Some(&dir.join("nope"))).is_empty());
    }

    #[test]
    fn jsonl_cwd_match_and_fallback() {
        let (_g, dir) = testutil::tempdir();
        std::fs::write(
            dir.join("abc123.jsonl"),
            "{\"cwd\":\"/work/a\"}\n{\"x\":1}\n",
        )
        .unwrap();
        std::fs::write(dir.join("zzz999.jsonl"), "{\"role\":\"user\"}\n").unwrap();
        let cands = candidates("/work/a", Some(&dir));
        assert_eq!(cands.len(), 2, "{cands:?}");
        assert_eq!(cands[0].id, "abc123");
        assert!(cands[0].matched);
        assert!(!cands[1].matched);
    }

    #[test]
    fn exists_matches_session_files_only() {
        let (_g, dir) = testutil::tempdir();
        std::fs::write(dir.join("abc123.jsonl"), "{}\n").unwrap();
        assert!(session_exists("abc123", Some(&dir)));
        assert!(!session_exists("uq", Some(&dir)));
        assert!(!session_exists("zzz999", Some(&dir)));
    }
}
