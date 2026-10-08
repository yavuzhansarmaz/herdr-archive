//! Amp thread scanner (CLI-backed).
//!
//! EXPERIMENTAL (U5): the `amp` CLI is not installed locally; the
//! `threads list --json` output schema and the `threads continue <id>`
//! resume form are unverified. Amp threads are server-authoritative —
//! `~/.local/share/amp` is a legacy cache and is deliberately NOT scanned.
//!
//! Best-effort by design: auth/network failure, a missing CLI, a timeout,
//! or an unparseable reply all yield no candidates (the manual flow falls
//! back to paste/shell/abort), never an error.

use super::{Candidate, MAX_CANDIDATE_AGE_SECONDS, now_secs, prescan, select};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for `amp threads list --json` before killing it.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(5);

const ID_FIELDS: &[&str] = &["id", "threadId", "thread_id"];
const CWD_FIELDS: &[&str] = &[
    "cwd",
    "project",
    "projectDir",
    "workspace",
    "rootPath",
    "directory",
];
const TS_FIELDS: &[&str] = &[
    "updatedAt",
    "updated_at",
    "lastUpdated",
    "timestamp",
    "createdAt",
];

fn amp_command() -> PathBuf {
    match std::env::var("HERDR_ARCHIVE_AMP_CMD")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(c) => PathBuf::from(c),
        None => PathBuf::from("amp"),
    }
}

fn run_list(cmd: &PathBuf) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(["threads", "list", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + LIST_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                use std::io::Read;
                if let Some(mut stdout) = child.stdout.take() {
                    if stdout.read_to_string(&mut out).is_err() {
                        return None;
                    }
                }
                return Some(out);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

fn thread_list(text: &str) -> Vec<serde_json::Value> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    if let Some(a) = v.as_array() {
        return a.clone();
    }
    if let Some(a) = v.get("threads").and_then(|t| t.as_array()) {
        return a.clone();
    }
    Vec::new()
}

fn thread_time_ms(v: &serde_json::Value) -> Option<f64> {
    let o = v.as_object()?;
    for key in TS_FIELDS {
        if let Some(ts) = o
            .get(*key)
            .and_then(|x| x.as_str())
            .and_then(|s| crate::util::parse_iso(Some(s)))
        {
            return Some(ts.0 as f64 / 1_000_000.0);
        }
        if let Some(ms) = o.get(*key).and_then(|x| x.as_i64()) {
            return Some(ms as f64 / 1000.0);
        }
        if let Some(sec) = o.get(*key).and_then(|x| x.as_u64()) {
            return Some(sec as f64);
        }
    }
    None
}

/// Candidates via `amp threads list --json`, matched on cwd.
pub fn candidates(cwd: &str) -> Vec<Candidate> {
    candidates_with_command(cwd, &amp_command())
}

/// Same, with an explicit command path (tests point this at a stub).
pub fn candidates_with_command(cwd: &str, cmd: &PathBuf) -> Vec<Candidate> {
    let Some(text) = run_list(cmd) else {
        return Vec::new();
    };
    let now = now_secs();
    let cutoff = now - MAX_CANDIDATE_AGE_SECONDS;
    let mut found: Vec<(String, f64, bool)> = Vec::new();
    for t in thread_list(&text) {
        let Some(o) = t.as_object() else {
            continue;
        };
        let Some(id) = ID_FIELDS
            .iter()
            .filter_map(|k| o.get(*k).and_then(|x| x.as_str()))
            .find(|s| !s.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        // Threads listed by the server are live state; without a timestamp
        // they count as current rather than being cut off.
        let mtime = thread_time_ms(&t).unwrap_or(now);
        let matched = CWD_FIELDS
            .iter()
            .any(|k| o.get(*k).and_then(|x| x.as_str()) == Some(cwd));
        found.push((id, mtime, matched));
    }
    found.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    found.truncate(super::SCAN_LIMIT);
    let order = prescan(found.iter().map(|(id, m, _)| (id.clone(), *m)).collect());
    let by_id: std::collections::HashMap<&str, bool> =
        found.iter().map(|(id, _, m)| (id.as_str(), *m)).collect();
    let with_match: Vec<(String, f64, bool)> = order
        .into_iter()
        .filter(|(_, m)| *m >= cutoff)
        .map(|(id, m)| {
            let matched = by_id.get(id.as_str()).copied().unwrap_or(false);
            (id, m, matched)
        })
        .collect();
    select(with_match, cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use std::os::unix::fs::PermissionsExt;

    fn stub_amp(dir: &std::path::Path, script: &str) -> PathBuf {
        let p = dir.join("amp");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn missing_cli_yields_nothing() {
        let (_g, dir) = testutil::tempdir();
        assert!(candidates_with_command("/work/a", &dir.join("no-such-bin")).is_empty());
    }

    #[test]
    fn failing_cli_yields_nothing() {
        let (_g, dir) = testutil::tempdir();
        let cmd = stub_amp(&dir, "#!/bin/sh\necho not-logged-in >&2\nexit 1\n");
        assert!(candidates_with_command("/work/a", &cmd).is_empty());
    }

    #[test]
    fn parses_thread_list_and_matches_cwd() {
        let (_g, dir) = testutil::tempdir();
        let cmd = stub_amp(
            &dir,
            "#!/bin/sh\ncat <<'EOF'\n[{\"id\":\"T-aaa\",\"cwd\":\"/work/a\"},{\"id\":\"T-bbb\",\"cwd\":\"/other\"}]\nEOF\n",
        );
        let cands = candidates_with_command("/work/a", &cmd);
        assert_eq!(cands.len(), 2, "{cands:?}");
        assert_eq!(cands[0].id, "T-aaa");
        assert!(cands[0].matched);
        assert!(!cands[1].matched);
    }

    #[test]
    fn garbage_output_yields_nothing() {
        let (_g, dir) = testutil::tempdir();
        let cmd = stub_amp(&dir, "#!/bin/sh\necho 'not json'\n");
        assert!(candidates_with_command("/work/a", &cmd).is_empty());
    }

    #[test]
    fn hanging_cli_times_out() {
        let (_g, dir) = testutil::tempdir();
        let cmd = stub_amp(&dir, "#!/bin/sh\nsleep 60\n");
        let start = Instant::now();
        assert!(candidates_with_command("/work/a", &cmd).is_empty());
        assert!(start.elapsed() < Duration::from_secs(30));
    }
}
