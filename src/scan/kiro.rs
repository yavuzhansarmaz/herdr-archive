//! Kiro session scanner.
//!
//! EXPERIMENTAL (U2): the `kiro-cli` binary is not installed locally; the
//! resume flag and store layout are unverified. Assumed layout:
//! `~/.kiro/sessions/cli/<uuid>.json` (metadata: cwd, timestamps) plus
//! `<uuid>.jsonl` plus `<uuid>.lock`. The id is the file stem; the
//! workspace match binds on a metadata cwd field; `.lock`-present (active)
//! wins mtime ties.

use super::{Candidate, MAX_CANDIDATE_AGE_SECONDS, mtime_secs, now_secs, prescan, select};
use std::path::{Path, PathBuf};

pub fn sessions_base() -> PathBuf {
    super::home_dir().join(".kiro").join("sessions").join("cli")
}

/// Candidate workspace-binding fields in the metadata JSON (unverified).
const CWD_FIELDS: &[&str] = &["cwd", "projectDir", "workspace", "rootPath", "directory"];

fn metadata_cwd(meta: &Path) -> Option<String> {
    let text = std::fs::read_to_string(meta).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
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

fn entries(root: &Path) -> Vec<(String, PathBuf, f64, bool)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for meta in rd.filter_map(|e| e.ok().map(|e| e.path())) {
        if !meta.is_file() || meta.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Skip `<uuid>.jsonl` companions (extension is jsonl, already excluded).
        let Some(stem) = meta
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if stem.is_empty() {
            continue;
        }
        let log = meta.with_extension("jsonl");
        let lock = meta.with_extension("lock");
        let mtime = mtime_secs(&meta).max(mtime_secs(&log));
        out.push((stem, meta, mtime, lock.exists()));
    }
    out
}

pub fn candidates(cwd: &str, base: Option<&Path>) -> Vec<Candidate> {
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    let all = entries(&root);
    // Newest-first, `.lock`-present winning ties.
    let mut found: Vec<(String, f64)> = all.iter().map(|(id, _, m, _)| (id.clone(), *m)).collect();
    found.sort_by(|a, b| {
        let la = all
            .iter()
            .find(|(id, _, _, _)| id == &a.0)
            .map(|(_, _, _, l)| *l)
            .unwrap_or(false);
        let lb = all
            .iter()
            .find(|(id, _, _, _)| id == &b.0)
            .map(|(_, _, _, l)| *l)
            .unwrap_or(false);
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| lb.cmp(&la))
    });
    found.truncate(super::SCAN_LIMIT);
    let cutoff = now_secs() - MAX_CANDIDATE_AGE_SECONDS;
    let by_id: std::collections::HashMap<&str, &PathBuf> = all
        .iter()
        .map(|(id, meta, _, _)| (id.as_str(), meta))
        .collect();
    let mut with_match = Vec::new();
    for (id, mtime) in prescan(found) {
        if mtime < cutoff {
            continue;
        }
        let matched = by_id
            .get(id.as_str())
            .and_then(|m| metadata_cwd(m))
            .as_deref()
            == Some(cwd);
        with_match.push((id, mtime, matched));
    }
    select(with_match, cutoff)
}

/// The metadata file for an exact session id (for history readers).
pub fn session_file(session_id: &str) -> Option<PathBuf> {
    if !crate::history::valid_session_id(session_id) {
        return None;
    }
    let p = sessions_base().join(format!("{session_id}.json"));
    p.is_file().then_some(p)
}

/// Strict existence check for a manually entered session id.
pub fn session_exists(value: &str, base: Option<&Path>) -> bool {
    if !crate::history::valid_session_id(value) {
        return false;
    }
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    root.join(format!("{value}.json")).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn write_entry(dir: &Path, id: &str, meta: &str, lock: bool) {
        std::fs::write(dir.join(format!("{id}.json")), meta).unwrap();
        std::fs::write(dir.join(format!("{id}.jsonl")), "{}\n").unwrap();
        if lock {
            std::fs::write(dir.join(format!("{id}.lock")), "").unwrap();
        }
    }

    #[test]
    fn missing_base_yields_nothing() {
        let (_g, dir) = testutil::tempdir();
        assert!(candidates("/work/a", Some(&dir.join("nope"))).is_empty());
    }

    #[test]
    fn stem_id_and_cwd_match() {
        let (_g, dir) = testutil::tempdir();
        write_entry(&dir, "uuid-1", r#"{"cwd":"/work/a"}"#, true);
        write_entry(&dir, "uuid-2", r#"{"cwd":"/other"}"#, false);
        let cands = candidates("/work/a", Some(&dir));
        assert_eq!(cands.len(), 2, "{cands:?}");
        assert_eq!(cands[0].id, "uuid-1");
        assert!(cands[0].matched);
        assert!(!cands[1].matched);
    }

    #[test]
    fn exists_matches_metadata_files_only() {
        let (_g, dir) = testutil::tempdir();
        write_entry(&dir, "uuid-1", r#"{"cwd":"/work/a"}"#, false);
        assert!(session_exists("uuid-1", Some(&dir)));
        assert!(!session_exists("uq", Some(&dir)));
        assert!(!session_exists("uuid-9", Some(&dir)));
    }
}
