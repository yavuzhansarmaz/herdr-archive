//! Gemini session scanner.
//!
//! EXPERIMENTAL (U3): the `gemini` CLI is not installed locally, so the
//! `--resume` flag and the `session-*.json` schema below are unverified
//! guesses from agent research. Store layout: `~/.gemini/tmp/*/chats/
//! session-*.json`. The resume id is parsed from the session JSON (candidate
//! fields); the workspace match binds on a cwd/project field inside the
//! JSON — never on the `<project_hash>` directory name algorithm.

use super::{
    Candidate, MAX_CANDIDATE_AGE_SECONDS, mtime_secs, now_secs, prescan, read_head, select,
};
use std::path::{Path, PathBuf};

pub fn sessions_base() -> PathBuf {
    super::home_dir().join(".gemini").join("tmp")
}

/// Candidate id fields inside a gemini session JSON object (unverified).
const ID_FIELDS: &[&str] = &["sessionId", "session_id", "id", "uuid", "resumeId"];

/// Candidate workspace-binding fields (unverified).
const CWD_FIELDS: &[&str] = &[
    "cwd",
    "projectDir",
    "project_dir",
    "project",
    "workspace",
    "rootPath",
    "directory",
];

fn chat_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for proj in rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
    {
        let chats = proj.join("chats");
        let Ok(rd) = std::fs::read_dir(&chats) else {
            continue;
        };
        for f in rd.filter_map(|e| e.ok().map(|e| e.path())) {
            if f.is_file()
                && f.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("session-") && n.ends_with(".json"))
            {
                out.push(f);
            }
        }
    }
    out
}

fn parse_session(path: &Path) -> Option<serde_json::Value> {
    let head = read_head(path);
    serde_json::from_slice::<serde_json::Value>(&head).ok()
}

fn session_id(path: &Path, v: &serde_json::Value) -> Option<String> {
    if let Some(o) = v.as_object() {
        for key in ID_FIELDS {
            if let Some(s) = o
                .get(*key)
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                return Some(s.to_string());
            }
        }
    }
    // Fall back to the `session-<id>.json` file stem.
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("session-"))
        .map(str::to_string)
}

fn workspace_of(v: &serde_json::Value) -> Option<String> {
    let o = v.as_object()?;
    for key in CWD_FIELDS {
        if let Some(s) = o
            .get(*key)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
        {
            return Some(s.to_string());
        }
    }
    None
}

fn is_match(path: &Path, v: &serde_json::Value, cwd: &str) -> bool {
    if workspace_of(v).as_deref() == Some(cwd) {
        return true;
    }
    // Last resort: the raw cwd quoted anywhere in the head.
    let head = read_head(path);
    let needle = format!("\"{cwd}\"");
    !cwd.is_empty() && head.windows(needle.len()).any(|w| w == needle.as_bytes())
}

pub fn candidates(cwd: &str, base: Option<&Path>) -> Vec<Candidate> {
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    let files = chat_files(&root);
    let found: Vec<(String, f64)> = files
        .iter()
        .filter_map(|p| {
            let v = parse_session(p)?;
            let id = session_id(p, &v)?;
            Some((id, mtime_secs(p)))
        })
        .collect();
    let cutoff = now_secs() - MAX_CANDIDATE_AGE_SECONDS;
    // Re-resolve each scanned id to its file for the workspace check.
    let by_id: std::collections::HashMap<String, &PathBuf> = {
        let mut m = std::collections::HashMap::new();
        for p in &files {
            if let Some(v) = parse_session(p) {
                if let Some(id) = session_id(p, &v) {
                    m.entry(id).or_insert(p);
                }
            }
        }
        m
    };
    let mut with_match = Vec::new();
    for (id, mtime) in prescan(found) {
        if mtime < cutoff {
            continue;
        }
        let matched = by_id
            .get(&id)
            .and_then(|p| parse_session(p).map(|v| is_match(p, &v, cwd)))
            .unwrap_or(false);
        with_match.push((id, mtime, matched));
    }
    select(with_match, cutoff)
}

/// The session file for an exact session id (for history readers).
pub fn session_file(wanted: &str) -> Option<PathBuf> {
    session_file_in(wanted, None)
}

fn session_file_in(wanted: &str, base: Option<&Path>) -> Option<PathBuf> {
    if !crate::history::valid_session_id(wanted) {
        return None;
    }
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    for p in chat_files(&root) {
        if let Some(v) = parse_session(&p) {
            if session_id(&p, &v).as_deref() == Some(wanted) {
                return Some(p);
            }
        }
    }
    None
}

/// Strict existence check for a manually entered session id.
pub fn session_exists(value: &str, base: Option<&Path>) -> bool {
    session_file_in(value, base).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn write_session(dir: &Path, proj: &str, name: &str, body: &str) -> PathBuf {
        let d = dir.join(proj).join("chats");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn missing_base_yields_nothing() {
        let (_g, dir) = testutil::tempdir();
        assert!(candidates("/work/a", Some(&dir.join("nope"))).is_empty());
    }

    #[test]
    fn id_from_json_and_cwd_match() {
        let (_g, dir) = testutil::tempdir();
        write_session(
            &dir,
            "projA",
            "session-aaa.json",
            r#"{"sessionId":"uuid-1","cwd":"/work/a"}"#,
        );
        write_session(
            &dir,
            "projB",
            "session-bbb.json",
            r#"{"sessionId":"uuid-2","cwd":"/other"}"#,
        );
        write_session(
            &dir,
            "projC",
            "session-ccc.json",
            r#"{"id":"uuid-3","projectDir":"/work/a"}"#,
        );
        write_session(&dir, "projD", "not-a-session.txt", "junk");
        write_session(&dir, "projE", "session-bad.json", "{not json");
        let cands = candidates("/work/a", Some(&dir));
        assert_eq!(cands.len(), 3, "{cands:?}");
        assert!(cands[0].matched);
        assert!(cands[1].matched);
        assert!(!cands[2].matched);
        let ids: Vec<&str> = cands.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"uuid-1"));
        assert!(ids.contains(&"uuid-2"));
        assert!(ids.contains(&"uuid-3"));
    }

    #[test]
    fn id_falls_back_to_filename_stem() {
        let (_g, dir) = testutil::tempdir();
        write_session(&dir, "p", "session-deadbeef.json", r#"{"cwd":"/work/a"}"#);
        let cands = candidates("/work/a", Some(&dir));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "deadbeef");
        assert!(cands[0].matched);
    }

    #[test]
    fn age_cutoff_applies() {
        let (_g, dir) = testutil::tempdir();
        let p = write_session(
            &dir,
            "p",
            "session-old.json",
            r#"{"sessionId":"old-1","cwd":"/work/a"}"#,
        );
        testutil::backdate(&p, 61 * 86400);
        assert!(candidates("/work/a", Some(&dir)).is_empty());
    }

    #[test]
    fn exists_matches_stored_ids_only() {
        let (_g, dir) = testutil::tempdir();
        write_session(&dir, "p", "session-aaa.json", r#"{"sessionId":"uuid-1"}"#);
        assert!(session_exists("uuid-1", Some(&dir)));
        // existence has no age gate (unlike candidates), but unknown ids fail
        assert!(!session_exists("uq", Some(&dir)));
        assert!(!session_exists("uuid-9", Some(&dir)));
        assert!(!session_exists("../x", Some(&dir)));
    }
}
