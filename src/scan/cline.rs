//! Cline session scanner.
//!
//! EXPERIMENTAL (U1): the `cline` CLI is not installed locally; the resume
//! form and store layout are unverified. Standalone layout assumed:
//! `${CLINE_SESSION_DATA_DIR:-~/.cline/data/sessions}/<id>/
//! <id>.messages.json` plus `<id>.json` manifest (title/cwd/ts).
//!
//! Deferred (U7): VS Code `globalStorage/saoudrizwan.claude-dev/tasks/`
//! fallback. Unresolved (U12): further data-dir overrides and macOS
//! `~/Library` paths.

use super::{Candidate, MAX_CANDIDATE_AGE_SECONDS, mtime_secs, now_secs, prescan, select};
use std::path::{Path, PathBuf};

pub fn sessions_base() -> PathBuf {
    if let Some(d) = std::env::var("CLINE_SESSION_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(d);
    }
    super::home_dir()
        .join(".cline")
        .join("data")
        .join("sessions")
}

/// Candidate workspace-binding fields in the `<id>.json` manifest (unverified).
const CWD_FIELDS: &[&str] = &["cwd", "projectRoot", "workspace", "rootPath", "directory"];

fn manifest_cwd(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
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

fn entries(root: &Path) -> Vec<(String, PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for dir in rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
    {
        let Some(id) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let messages = dir.join(format!("{id}.messages.json"));
        let manifest = dir.join(format!("{id}.json"));
        if messages.is_file() || manifest.is_file() {
            out.push((id, messages, manifest));
        }
    }
    out
}

/// Strict existence check for a manually entered session id: the `<id>/`
/// dir holds a messages log or a manifest.
pub fn session_exists(value: &str, base: Option<&Path>) -> bool {
    if !crate::history::valid_session_id(value) {
        return false;
    }
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    let dir = root.join(value);
    dir.is_dir()
        && (dir.join(format!("{value}.messages.json")).is_file()
            || dir.join(format!("{value}.json")).is_file())
}

pub fn candidates(cwd: &str, base: Option<&Path>) -> Vec<Candidate> {
    let root = base.map(PathBuf::from).unwrap_or_else(sessions_base);
    let all = entries(&root);
    let found: Vec<(String, f64)> = all
        .iter()
        .map(|(id, messages, manifest)| {
            let mtime = mtime_secs(messages).max(mtime_secs(manifest));
            (id.clone(), mtime)
        })
        .collect();
    let cutoff = now_secs() - MAX_CANDIDATE_AGE_SECONDS;
    let by_id: std::collections::HashMap<&str, &PathBuf> = all
        .iter()
        .map(|(id, _, manifest)| (id.as_str(), manifest))
        .collect();
    let mut with_match = Vec::new();
    for (id, mtime) in prescan(found) {
        if mtime < cutoff {
            continue;
        }
        let matched = by_id
            .get(id.as_str())
            .and_then(|m| manifest_cwd(m))
            .as_deref()
            == Some(cwd);
        with_match.push((id, mtime, matched));
    }
    select(with_match, cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn write_entry(dir: &Path, id: &str, manifest: Option<&str>) -> PathBuf {
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("{id}.messages.json")), "[]").unwrap();
        if let Some(m) = manifest {
            std::fs::write(d.join(format!("{id}.json")), m).unwrap();
        }
        d
    }

    #[test]
    fn missing_base_yields_nothing() {
        let (_g, dir) = testutil::tempdir();
        assert!(candidates("/work/a", Some(&dir.join("nope"))).is_empty());
    }

    #[test]
    fn manifest_cwd_match() {
        let (_g, dir) = testutil::tempdir();
        write_entry(&dir, "aaa", Some(r#"{"cwd":"/work/a","title":"t"}"#));
        write_entry(&dir, "bbb", Some(r#"{"cwd":"/other"}"#));
        write_entry(&dir, "ccc", None);
        let cands = candidates("/work/a", Some(&dir));
        assert_eq!(cands.len(), 3, "{cands:?}");
        assert_eq!(cands[0].id, "aaa");
        assert!(cands[0].matched);
        assert!(!cands[1].matched);
        assert!(!cands[2].matched);
    }

    #[test]
    fn exists_matches_entry_dirs_only() {
        let (_g, dir) = testutil::tempdir();
        write_entry(&dir, "aaa", Some(r#"{"cwd":"/work/a"}"#));
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        assert!(session_exists("aaa", Some(&dir)));
        assert!(!session_exists("uq", Some(&dir)));
        assert!(!session_exists("empty", Some(&dir)));
        assert!(!session_exists("../x", Some(&dir)));
    }
}
