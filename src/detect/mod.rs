//! Bind a live muse pane to its session without asking.
//!
//! When the archive-tab popup meets a muse pane with no herdr-reported
//! session, the scanner picker is the fallback — but a running muse process
//! often *shows* its session: a resumed CLI carries `resume <id>` in argv,
//! an open `session.jsonl` fd names the session's log, and (when no log is
//! held open) exactly one open `.session.lock` names it instead.
//! [`detect_live_session`] reads those three signals via `pane.process_info`
//! (+ `/proc` on Linux), validates the id against the local muse store, and
//! hands the manual flow a [`Detected`] session to use silently. Anything
//! uncertain returns None: no signal falls through to the scanner picker
//! unchanged, while two or more distinct VALIDATED lock ids (ambiguous)
//! scopes the picker to exactly those sessions
//! ([`ambiguous_from_snapshot`]).
//!
//! Linux-only: the fd leg needs `/proc`, so on other OSes detection returns
//! None (the picker covers those). Validation walks the on-disk session
//! store, never the sqlite index — cf. the sibling `herdr-muse-resume`
//! plugin, which solves the different problem of picking the newest valid
//! session per workspace for restart restores (single-binary offline
//! install: no sqlite dependency here).

use serde_json::Value;
use std::path::{Path, PathBuf};

/// How a live session was identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// `resume <id>` in the process argv — the CLI states its session.
    Certain,
    /// An open session handle: `session.jsonl` (`detect:fd`), or — when no
    /// log is held open — exactly one `.session.lock` (`detect:lock`).
    Strong,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub id: String,
    pub confidence: Confidence,
    /// `resolved_by` marker: `detect:argv` (Certain), `detect:fd` or
    /// `detect:lock` (Strong).
    pub provenance: &'static str,
}

pub const PROVENANCE_ARGV: &str = "detect:argv";
pub const PROVENANCE_FD: &str = "detect:fd";
pub const PROVENANCE_LOCK: &str = "detect:lock";
/// `resolved_by` marker for a session the user picked from the
/// ambiguity-scoped list (lock leg found 2+ validated live sessions and
/// could not choose silently).
pub const PROVENANCE_AMBIGUOUS: &str = "detect:ambiguous";

/// One foreground process as reported by `pane.process_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProc {
    pub pid: Option<u32>,
    pub name: String,
    pub argv: Vec<String>,
}

/// Parse `foreground_processes` out of a `pane.process_info` result. Pure;
/// mirrors the extraction in `archive::launch_argv`. Malformed entries are
/// skipped, never fatal.
pub fn parse_foreground(process_info: &Value) -> Vec<ForegroundProc> {
    let mut out = Vec::new();
    let procs = process_info
        .get("foreground_processes")
        .and_then(Value::as_array);
    for proc in procs.cloned().unwrap_or_default() {
        let argv: Vec<String> = proc
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.push(ForegroundProc {
            pid: proc
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|p| u32::try_from(p).ok()),
            name: proc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            argv,
        });
    }
    out
}

/// The session ref carried by `argv` (`muse resume <id>`), if any. Pure:
/// returns the token after the first `resume`; the caller validates it
/// against the session store before trusting it.
pub fn resume_id_from_argv(argv: &[String]) -> Option<String> {
    let mut it = argv.iter().peekable();
    while let Some(tok) = it.next() {
        if tok == "resume" {
            return it.next().cloned();
        }
    }
    None
}

/// One open session log found among a process's fd targets. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdHit {
    pub id: String,
    /// The same process also holds `<session-dir>/.session.lock` open.
    pub lock_bonus: bool,
}

/// The session id behind an fd target pointing at a muse session log:
/// `<...>/sessions/<...>/<id>/session.jsonl` with a well-formed id.
/// Anything else (other files, stray same-named paths outside a `sessions`
/// tree) yields None.
fn fd_session_id(target: &str) -> Option<(String, PathBuf)> {
    let target = target.strip_suffix(" (deleted)").unwrap_or(target);
    let path = Path::new(target);
    if path.file_name().and_then(|s| s.to_str()) != Some("session.jsonl") {
        return None;
    }
    let dir = path.parent()?;
    let id = dir.file_name()?.to_str()?;
    if !crate::history::valid_session_id(id) {
        return None;
    }
    if !path.components().any(|c| c.as_os_str() == "sessions") {
        return None;
    }
    Some((id.to_string(), dir.to_path_buf()))
}

/// Scan fd symlink targets for an open muse session log. Pure. When several
/// processes' worth of targets name different sessions, a lock-held hit wins,
/// else the first hit wins.
pub fn session_from_fd_targets(targets: &[String]) -> Option<FdHit> {
    let lock_dirs: std::collections::HashSet<PathBuf> = targets
        .iter()
        .filter_map(|t| {
            let t = t.strip_suffix(" (deleted)").unwrap_or(t);
            let p = Path::new(t);
            if p.file_name().and_then(|s| s.to_str()) == Some(".session.lock") {
                p.parent().map(Path::to_path_buf)
            } else {
                None
            }
        })
        .collect();
    let mut fallback: Option<FdHit> = None;
    for t in targets {
        let Some((id, dir)) = fd_session_id(t) else {
            continue;
        };
        let hit = FdHit {
            id,
            lock_bonus: lock_dirs.contains(&dir),
        };
        if hit.lock_bonus {
            return Some(hit);
        }
        if fallback.is_none() {
            fallback = Some(hit);
        }
    }
    fallback
}

/// The session id behind an fd target pointing at a muse session lock:
/// `<...>/sessions/<...>/<id>/.session.lock` with a well-formed id.
/// Targets under a `subagent/` segment are ignored — a live pid has been
/// observed holding its own lock *and* subagent locks, so those never name
/// the session. Anything else yields None.
fn lock_session_id(target: &str) -> Option<String> {
    let target = target.strip_suffix(" (deleted)").unwrap_or(target);
    let path = Path::new(target);
    if path.file_name().and_then(|s| s.to_str()) != Some(".session.lock") {
        return None;
    }
    if path.components().any(|c| c.as_os_str() == "subagent") {
        return None;
    }
    let dir = path.parent()?;
    let id = dir.file_name()?.to_str()?;
    if !crate::history::valid_session_id(id) {
        return None;
    }
    if !path.components().any(|c| c.as_os_str() == "sessions") {
        return None;
    }
    Some(id.to_string())
}

/// Distinct session ids behind muse session-lock fd targets, in first-seen
/// order. Pure. Subagent locks are ignored; duplicate fds to the same lock
/// collapse to one. Store validation is the caller's job.
fn distinct_lock_ids(targets: &[String]) -> Vec<String> {
    let mut distinct: Vec<String> = Vec::new();
    for t in targets {
        let Some(id) = lock_session_id(t) else {
            continue;
        };
        if !distinct.contains(&id) {
            distinct.push(id);
        }
    }
    distinct
}

/// Scan fd symlink targets for muse session locks. Pure. Exactly one
/// distinct non-subagent lock id → that id; zero, or two or more distinct
/// ids (ambiguous — the picker decides), → None. Duplicate fds to the same
/// lock collapse to one. Store validation is the caller's job.
pub fn lock_id_from_fd_targets(targets: &[String]) -> Option<String> {
    let distinct = distinct_lock_ids(targets);
    if distinct.len() == 1 {
        distinct.into_iter().next()
    } else {
        None
    }
}

/// Live fd targets of `pid` via `/proc` (Linux; elsewhere empty). All I/O
/// failures yield an empty list — detection skips, never raises.
pub fn read_fd_targets(pid: u32) -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            return Vec::new();
        };
        for entry in rd.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                out.push(target.to_string_lossy().into_owned());
            }
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        Vec::new()
    }
}

fn exists_in_store(id: &str, store_base: Option<&Path>) -> bool {
    crate::scan::muse::session_exists(id, store_base)
}

/// The pane's muse processes: name or argv[0] matching the built-in muse
/// row (versioned binaries included). Empty when muse is not built in.
fn muse_procs(procs: &[ForegroundProc]) -> Vec<&ForegroundProc> {
    let table = crate::agents::builtin();
    let Some(entry) = table.get("muse") else {
        return Vec::new();
    };
    procs
        .iter()
        .filter(|p| {
            crate::agents::matches_program(entry, &p.name)
                || p.argv
                    .first()
                    .is_some_and(|a0| crate::agents::matches_program(entry, a0))
        })
        .collect()
}

/// Popup-time preview of what resolve-time detection will find for one
/// pane. Informational only: confirm-time detection re-runs and stays the
/// authority — argv and lock state can change between the popup and confirm,
/// so the preview value is never trusted, only shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    /// Exactly what [`detect_from_snapshot`] would return: a validated
    /// Certain/Strong hit.
    Hit(Detected),
    /// Two or more distinct VALIDATED session locks (resolve-time detection
    /// yields None and the picker lists exactly these); `count` is the
    /// number of validated ids, matching the resolve-time picker's rows.
    Ambiguous { count: usize },
    /// No signal at all.
    Miss,
}

/// Full detection preview over an injected snapshot: a process list, an
/// fd-target provider, and a muse store root. The unit-test seam (no
/// socket, no `/proc`): production passes the real fd reader and `None`
/// for the store. Same legs as [`detect_from_snapshot`], except the lock
/// leg reports ambiguity instead of collapsing it to None.
pub fn preview_from_snapshot(
    procs: &[ForegroundProc],
    fd_targets: &dyn Fn(u32) -> Vec<String>,
    store_base: Option<&Path>,
) -> Preview {
    let muse_procs = muse_procs(procs);
    // Leg 1: `resume <id>` in argv — Certain once the id exists in the store.
    for proc in &muse_procs {
        if let Some(id) = resume_id_from_argv(&proc.argv) {
            if exists_in_store(&id, store_base) {
                return Preview::Hit(Detected {
                    id,
                    confidence: Confidence::Certain,
                    provenance: PROVENANCE_ARGV,
                });
            }
        }
    }
    // Leg 2: an open session.jsonl fd — Strong. A lock-held hit wins
    // outright; otherwise the first validated hit wins.
    let mut fallback: Option<Detected> = None;
    for proc in &muse_procs {
        let Some(pid) = proc.pid else { continue };
        let Some(hit) = session_from_fd_targets(&fd_targets(pid)) else {
            continue;
        };
        if !exists_in_store(&hit.id, store_base) {
            continue;
        }
        let detected = Detected {
            id: hit.id,
            confidence: Confidence::Strong,
            provenance: PROVENANCE_FD,
        };
        if hit.lock_bonus {
            return Preview::Hit(detected);
        }
        if fallback.is_none() {
            fallback = Some(detected);
        }
    }
    if let Some(detected) = fallback {
        return Preview::Hit(detected);
    }
    // Leg 3: a session lock with no open log — Strong. Bare `muse`
    // processes usually hold `.session.lock` open but not `session.jsonl`,
    // so this leg fires where leg 2 cannot. Exactly one distinct
    // non-subagent lock id across the pane's muse pids, validated against
    // the store; zero is a miss, two or more validated is ambiguity for the
    // picker. Ambiguity counts VALIDATED ids only, so the popup count
    // matches the resolve-time picker's rows exactly; a lock whose session
    // is gone from the store names nothing resumable.
    let mut all_targets: Vec<String> = Vec::new();
    for proc in &muse_procs {
        let Some(pid) = proc.pid else { continue };
        all_targets.extend(fd_targets(pid));
    }
    let distinct = distinct_lock_ids(&all_targets);
    if distinct.len() >= 2 {
        let validated: Vec<String> = distinct
            .into_iter()
            .filter(|id| exists_in_store(id, store_base))
            .collect();
        if validated.len() >= 2 {
            return Preview::Ambiguous {
                count: validated.len(),
            };
        }
        return Preview::Miss;
    }
    let Some(id) = distinct.into_iter().next() else {
        return Preview::Miss;
    };
    if !exists_in_store(&id, store_base) {
        return Preview::Miss;
    }
    Preview::Hit(Detected {
        id,
        confidence: Confidence::Strong,
        provenance: PROVENANCE_LOCK,
    })
}

/// Full detection over an injected snapshot: a process list, an fd-target
/// provider, and a muse store root. The unit-test seam (no socket, no
/// `/proc`): production passes the real fd reader and `None` for the store.
/// The resolve-time authority: a preview [`Preview::Hit`], and nothing else,
/// becomes a detection.
pub fn detect_from_snapshot(
    procs: &[ForegroundProc],
    fd_targets: &dyn Fn(u32) -> Vec<String>,
    store_base: Option<&Path>,
) -> Option<Detected> {
    match preview_from_snapshot(procs, fd_targets, store_base) {
        Preview::Hit(detected) => Some(detected),
        Preview::Ambiguous { .. } | Preview::Miss => None,
    }
}

/// Resolve-time lock-leg ambiguity over an injected snapshot: the
/// store-validated session ids behind the pane's muse session locks, in
/// first-seen order — exactly the rows the picker must list. Empty unless
/// the argv/fd legs miss AND two or more distinct lock ids validate.
/// Recomputed fresh at resolve time; the popup preview (count only) is
/// informational and never trusted.
pub fn ambiguous_from_snapshot(
    procs: &[ForegroundProc],
    fd_targets: &dyn Fn(u32) -> Vec<String>,
    store_base: Option<&Path>,
) -> Vec<String> {
    if !matches!(
        preview_from_snapshot(procs, fd_targets, store_base),
        Preview::Ambiguous { .. }
    ) {
        return Vec::new();
    }
    // The preview carries only the count: re-derive the validated ids.
    let mut all_targets: Vec<String> = Vec::new();
    for proc in muse_procs(procs) {
        let Some(pid) = proc.pid else { continue };
        all_targets.extend(fd_targets(pid));
    }
    distinct_lock_ids(&all_targets)
        .into_iter()
        .filter(|id| exists_in_store(id, store_base))
        .collect()
}

/// Live-session detection for one pane: `pane.process_info` → argv/fd
/// signals, validated against the local muse store. Linux-only; elsewhere
/// (or on any failure, or when nothing validates) None. Never guesses.
pub fn detect_live_session(pane_id: &str, client: &mut dyn crate::Herdr) -> Option<Detected> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    detect_live_session_with_store(pane_id, client, None)
}

/// [`detect_live_session`] with an injected muse store root (tests).
pub fn detect_live_session_with_store(
    pane_id: &str,
    client: &mut dyn crate::Herdr,
    store_base: Option<&Path>,
) -> Option<Detected> {
    let info = client
        .call("pane.process_info", serde_json::json!({"pane_id": pane_id}))
        .ok()?
        .get("process_info")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    let procs = parse_foreground(&info);
    detect_from_snapshot(&procs, &read_fd_targets, store_base)
}

/// Popup-time preview of what [`detect_live_session`] will find for one
/// pane. The open-archive popup calls this before composing its question so
/// users see the session that confirm-time detection is about to use.
/// Informational only: confirm re-runs [`detect_live_session`] and never
/// trusts this value. Linux-only; elsewhere (or on any failure) Miss.
pub fn preview_live_session(pane_id: &str, client: &mut dyn crate::Herdr) -> Preview {
    if !cfg!(target_os = "linux") {
        return Preview::Miss;
    }
    preview_live_session_with_store(pane_id, client, None)
}

/// [`preview_live_session`] with an injected muse store root (tests).
pub fn preview_live_session_with_store(
    pane_id: &str,
    client: &mut dyn crate::Herdr,
    store_base: Option<&Path>,
) -> Preview {
    let Ok(reply) = client.call("pane.process_info", serde_json::json!({"pane_id": pane_id}))
    else {
        return Preview::Miss;
    };
    let info = reply
        .get("process_info")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    let procs = parse_foreground(&info);
    preview_from_snapshot(&procs, &read_fd_targets, store_base)
}

/// Resolve-time lock-leg ambiguity for one pane: the store-validated
/// session ids behind its muse session locks — exactly the rows the picker
/// must list. Recomputed fresh (the popup preview is informational only);
/// empty unless detection cannot choose silently. Linux-only; elsewhere (or
/// on any failure) empty. Never guesses.
pub fn ambiguous_live_sessions(pane_id: &str, client: &mut dyn crate::Herdr) -> Vec<String> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    ambiguous_live_sessions_with_store(pane_id, client, None)
}

/// [`ambiguous_live_sessions`] with an injected muse store root (tests).
pub fn ambiguous_live_sessions_with_store(
    pane_id: &str,
    client: &mut dyn crate::Herdr,
    store_base: Option<&Path>,
) -> Vec<String> {
    let Ok(reply) = client.call("pane.process_info", serde_json::json!({"pane_id": pane_id}))
    else {
        return Vec::new();
    };
    let info = reply
        .get("process_info")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));
    let procs = parse_foreground(&info);
    ambiguous_from_snapshot(&procs, &read_fd_targets, store_base)
}

/// Short source name behind a `detect:*` provenance marker (`argv`, `fd`,
/// `lock`); unknown markers pass through unchanged.
fn source_of(provenance: &str) -> &str {
    provenance.strip_prefix("detect:").unwrap_or(provenance)
}

/// One-line popup text for a preview hit, e.g.
/// `Session 01a11b5e… (auto-detected via argv)`.
pub fn describe_hit(detected: &Detected) -> String {
    let short: String = detected.id.chars().take(8).collect();
    format!(
        "Session {short}… (auto-detected via {})",
        source_of(detected.provenance)
    )
}

/// One-line popup text for preview ambiguity, e.g.
/// `2 candidate sessions — you'll pick one`.
pub fn describe_ambiguous(count: usize) -> String {
    format!("{count} candidate sessions — you'll pick one")
}

/// The archive-popup activity line given the sweep-computed `base` line and
/// a detection preview. A hit or ambiguity replaces an `Activity unknown`
/// line; a miss — or any base line that already names an activity — stays
/// `base` unchanged.
pub fn popup_activity(base: &str, preview: &Preview) -> String {
    if !base.starts_with("Activity unknown") {
        return base.to_string();
    }
    match preview {
        Preview::Hit(detected) => describe_hit(detected),
        Preview::Ambiguous { count } => describe_ambiguous(*count),
        Preview::Miss => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const UUID: &str = "01a0da74-9f52-7760-a301-60a6f87abcf5";
    const UUID2: &str = "00000000-0000-4000-8000-000000000001";

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Fixture muse store: `<base>/2026/01/02/<id>/session.jsonl`.
    fn muse_store(base: &Path, ids: &[&str]) {
        for id in ids {
            let d = base.join("2026").join("01").join("02").join(id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("session.jsonl"),
                "{\"workspace_root\":\"/work/a\"}\n",
            )
            .unwrap();
        }
    }

    fn proc(pid: Option<u32>, name: &str, argv: &[&str]) -> ForegroundProc {
        ForegroundProc {
            pid,
            name: name.to_string(),
            argv: sv(argv),
        }
    }

    #[test]
    fn argv_parser_finds_resume_id() {
        assert_eq!(
            resume_id_from_argv(&sv(&["muse", "resume", UUID])).as_deref(),
            Some(UUID)
        );
        // versioned binary, flags before the subcommand
        assert_eq!(
            resume_id_from_argv(&sv(&[
                "/home/u/.local/bin/muse-bin-1.4.4-R5419.1",
                "--model",
                "opus",
                "resume",
                UUID
            ]))
            .as_deref(),
            Some(UUID)
        );
        assert_eq!(resume_id_from_argv(&sv(&["muse"])), None);
        assert_eq!(resume_id_from_argv(&[]), None);
        // trailing `resume` with no value carries nothing
        assert_eq!(resume_id_from_argv(&sv(&["muse", "resume"])), None);
        // no resume subcommand (plain TUI launch)
        assert_eq!(resume_id_from_argv(&sv(&["muse", "--model", "opus"])), None);
    }

    #[test]
    fn parse_foreground_tolerates_shape_gaps() {
        let info = json!({"foreground_processes": [
            {"pid": 123, "name": "muse", "argv": ["muse", "resume", UUID], "cwd": "/work/a"},
            {"name": "sh", "argv": ["sh"]},
            {"pid": -1, "argv": []},
        ]});
        let procs = parse_foreground(&info);
        assert_eq!(procs.len(), 3);
        assert_eq!(procs[0].pid, Some(123));
        assert_eq!(procs[0].argv.len(), 3);
        assert_eq!(procs[1].pid, None);
        assert_eq!(procs[2].pid, None); // negative pid is not a pid
        assert!(parse_foreground(&json!({})).is_empty());
        assert!(parse_foreground(&json!({"foreground_processes": {}})).is_empty());
    }

    #[test]
    fn fd_parser_finds_session_log() {
        let log = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/session.jsonl");
        let hit =
            session_from_fd_targets(&sv(&["/dev/pts/3", "socket:[12345]", log.as_str()])).unwrap();
        assert_eq!(hit.id, UUID);
        assert!(!hit.lock_bonus);
    }

    #[test]
    fn fd_parser_rejects_strays() {
        // Outside a `sessions` tree: not a muse session log.
        let stray = format!("/tmp/other/{UUID}/session.jsonl");
        assert_eq!(session_from_fd_targets(&sv(&[stray.as_str()])), None);
        // Malformed id (path escape attempt) is rejected.
        let evil = "/home/u/.local/share/muse/sessions/2026/10/08/../session.jsonl";
        assert_eq!(session_from_fd_targets(&sv(&[evil])), None);
        // Other files are not session logs.
        let other = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/other.jsonl");
        assert_eq!(session_from_fd_targets(&sv(&[other.as_str()])), None);
        assert_eq!(session_from_fd_targets(&[]), None);
    }

    #[test]
    fn fd_parser_lock_bonus_and_deleted_suffix() {
        let dir = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}");
        let hit = session_from_fd_targets(&sv(&[
            format!("{dir}/session.jsonl (deleted)").as_str(),
            format!("{dir}/.session.lock").as_str(),
        ]))
        .unwrap();
        assert_eq!(hit.id, UUID);
        assert!(hit.lock_bonus);
        // A lock in another session's dir is no bonus.
        let other = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}");
        let hit = session_from_fd_targets(&sv(&[
            format!("{dir}/session.jsonl").as_str(),
            format!("{other}/.session.lock").as_str(),
        ]))
        .unwrap();
        assert!(!hit.lock_bonus);
    }

    #[test]
    fn fd_parser_prefers_lock_held_hit() {
        let a = format!("/s/muse/sessions/2026/10/08/{UUID}");
        let b = format!("/s/muse/sessions/2026/10/08/{UUID2}");
        let hit = session_from_fd_targets(&sv(&[
            format!("{a}/session.jsonl").as_str(),
            format!("{b}/session.jsonl").as_str(),
            format!("{b}/.session.lock").as_str(),
        ]))
        .unwrap();
        assert_eq!(hit.id, UUID2);
        assert!(hit.lock_bonus);
    }

    #[test]
    fn lock_parser_single_lock() {
        let lock = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        assert_eq!(
            lock_id_from_fd_targets(&sv(&["/dev/pts/3", "socket:[12345]", lock.as_str()])),
            Some(UUID.to_string())
        );
        // Duplicate fds to the same lock still name one session.
        assert_eq!(
            lock_id_from_fd_targets(&sv(&[lock.as_str(), lock.as_str()])),
            Some(UUID.to_string())
        );
        // A deleted-marked lock target parses like the jsonl leg's.
        assert_eq!(
            lock_id_from_fd_targets(&sv(&[format!("{lock} (deleted)").as_str()])),
            Some(UUID.to_string())
        );
        assert_eq!(lock_id_from_fd_targets(&[]), None);
    }

    #[test]
    fn lock_parser_ignores_subagent_locks() {
        let own = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let sub_own = format!(
            "/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/subagent/abc/.session.lock"
        );
        let sub_other = format!(
            "/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/subagent/abc/.session.lock"
        );
        // Own lock plus subagent locks (own dir and another session's) → own.
        assert_eq!(
            lock_id_from_fd_targets(&sv(&[own.as_str(), sub_own.as_str(), sub_other.as_str()])),
            Some(UUID.to_string())
        );
        // Subagent locks alone are no signal.
        assert_eq!(
            lock_id_from_fd_targets(&sv(&[sub_own.as_str(), sub_other.as_str()])),
            None
        );
        // A bare `subagent` substring that is not a path segment still parses.
        let tricky = format!("/home/u/notsubagent-x/muse/sessions/2026/10/08/{UUID}/.session.lock");
        assert_eq!(
            lock_id_from_fd_targets(&sv(&[tricky.as_str()])),
            Some(UUID.to_string())
        );
    }

    #[test]
    fn lock_parser_rejects_ambiguity_and_strays() {
        let a = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let b = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/.session.lock");
        // Two distinct session locks: ambiguous, the picker decides.
        assert_eq!(
            lock_id_from_fd_targets(&sv(&[a.as_str(), b.as_str()])),
            None
        );
        // Outside a `sessions` tree: not a muse session lock.
        let stray = format!("/tmp/other/{UUID}/.session.lock");
        assert_eq!(lock_id_from_fd_targets(&sv(&[stray.as_str()])), None);
        // Malformed id (path escape attempt) is rejected.
        let evil = "/home/u/.local/share/muse/sessions/2026/10/08/../.session.lock";
        assert_eq!(lock_id_from_fd_targets(&sv(&[evil])), None);
        // Other files are not session locks.
        let other = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/session.jsonl");
        assert_eq!(lock_id_from_fd_targets(&sv(&[other.as_str()])), None);
    }

    #[test]
    fn snapshot_argv_certain_beats_fd() {
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID, UUID2]);
        let fd_log = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/session.jsonl");
        let procs = vec![proc(
            Some(99),
            "muse-bin-1.4.4-R5419.1",
            &["muse-bin-1.4.4-R5419.1", "resume", UUID],
        )];
        let d = detect_from_snapshot(&procs, &|_| sv(&[fd_log.as_str()]), Some(&dir)).unwrap();
        assert_eq!(d.id, UUID);
        assert_eq!(d.confidence, Confidence::Certain);
        assert_eq!(d.provenance, PROVENANCE_ARGV);
    }

    #[test]
    fn snapshot_fd_strong_for_bare_muse() {
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID]);
        let base = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}");
        let procs = vec![proc(Some(99), "muse", &["muse"])];
        let d = detect_from_snapshot(
            &procs,
            &|_| {
                sv(&[
                    format!("{base}/session.jsonl").as_str(),
                    format!("{base}/.session.lock").as_str(),
                ])
            },
            Some(&dir),
        )
        .unwrap();
        assert_eq!(d.id, UUID);
        assert_eq!(d.confidence, Confidence::Strong);
        assert_eq!(d.provenance, PROVENANCE_FD);
    }

    #[test]
    fn snapshot_lock_strong_for_bare_muse() {
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID]);
        let lock = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let procs = vec![proc(Some(99), "muse", &["muse"])];
        let d = detect_from_snapshot(&procs, &|_| sv(&["/dev/pts/3", lock.as_str()]), Some(&dir))
            .unwrap();
        assert_eq!(d.id, UUID);
        assert_eq!(d.confidence, Confidence::Strong);
        assert_eq!(d.provenance, PROVENANCE_LOCK);
    }

    #[test]
    fn snapshot_lock_own_plus_subagent_is_own() {
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID, UUID2]);
        let own = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let sub = format!(
            "/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/subagent/abc/.session.lock"
        );
        let procs = vec![proc(Some(99), "muse", &["muse"])];
        let d = detect_from_snapshot(&procs, &|_| sv(&[own.as_str(), sub.as_str()]), Some(&dir))
            .unwrap();
        assert_eq!(d.id, UUID);
        assert_eq!(d.provenance, PROVENANCE_LOCK);
    }

    #[test]
    fn snapshot_lock_ambiguous_invalid_or_subagent_only_is_none() {
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID, UUID2]);
        let lock_a = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let lock_b = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/.session.lock");
        let procs = vec![proc(Some(99), "muse", &["muse"])];
        // Two distinct session locks: ambiguous, the picker decides.
        assert_eq!(
            detect_from_snapshot(
                &procs,
                &|_| sv(&[lock_a.as_str(), lock_b.as_str()]),
                Some(&dir)
            ),
            None
        );
        // Subagent locks alone are no signal.
        let sub = format!(
            "/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/subagent/abc/.session.lock"
        );
        assert_eq!(
            detect_from_snapshot(&procs, &|_| sv(&[sub.as_str()]), Some(&dir)),
            None
        );
        // A lock whose session is gone from the store is no signal.
        let (_g2, empty) = crate::testutil::tempdir();
        muse_store(&empty, &[UUID2]);
        assert_eq!(
            detect_from_snapshot(&procs, &|_| sv(&[lock_a.as_str()]), Some(&empty)),
            None
        );
    }

    #[test]
    fn snapshot_jsonl_beats_lock() {
        // Both legs hold validated sessions: the jsonl leg wins and the
        // reported provenance is `detect:fd`, never `detect:lock`.
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID, UUID2]);
        let log = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/session.jsonl");
        let lock = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let procs = vec![proc(Some(99), "muse", &["muse"])];
        let d = detect_from_snapshot(&procs, &|_| sv(&[log.as_str(), lock.as_str()]), Some(&dir))
            .unwrap();
        assert_eq!(d.id, UUID2);
        assert_eq!(d.confidence, Confidence::Strong);
        assert_eq!(d.provenance, PROVENANCE_FD);
    }

    #[test]
    fn snapshot_invalid_jsonl_falls_through_to_lock() {
        // A jsonl naming a session that is gone does not block the lock leg.
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID]);
        let log = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/session.jsonl");
        let lock = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let procs = vec![proc(Some(99), "muse", &["muse"])];
        let d = detect_from_snapshot(&procs, &|_| sv(&[log.as_str(), lock.as_str()]), Some(&dir))
            .unwrap();
        assert_eq!(d.id, UUID);
        assert_eq!(d.provenance, PROVENANCE_LOCK);
    }

    #[test]
    fn snapshot_never_guesses() {
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID]);
        let no_fds = |_: u32| Vec::new();
        // argv id not in the store, no fds: None (falls through to the picker)
        let procs = vec![proc(Some(99), "muse", &["muse", "resume", UUID2])];
        assert_eq!(detect_from_snapshot(&procs, &no_fds, Some(&dir)), None);
        // fd id not in the store: None
        let stray = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/session.jsonl");
        let procs = vec![proc(Some(99), "muse", &["muse"])];
        assert_eq!(
            detect_from_snapshot(&procs, &|_| sv(&[stray.as_str()]), Some(&dir)),
            None
        );
        // `resume` argv on a non-muse process is not ours
        let procs = vec![proc(Some(99), "codex", &["codex", "resume", UUID])];
        assert_eq!(detect_from_snapshot(&procs, &no_fds, Some(&dir)), None);
        // no processes at all
        assert_eq!(detect_from_snapshot(&[], &no_fds, Some(&dir)), None);
        // versioned 1.4.3 binary shape matches too
        let procs = vec![proc(
            Some(99),
            "muse-bin-1.4.3-R5400.9",
            &["/home/u/.local/bin/muse-bin-1.4.3-R5400.9", "resume", UUID],
        )];
        let d = detect_from_snapshot(&procs, &no_fds, Some(&dir)).unwrap();
        assert_eq!(d.provenance, PROVENANCE_ARGV);
    }

    #[test]
    fn snapshot_invalid_argv_falls_through_to_fd() {
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID]);
        // argv names a session that is gone, but the process holds this one open
        let log = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/session.jsonl");
        let procs = vec![proc(Some(7), "muse", &["muse", "resume", UUID2])];
        let d = detect_from_snapshot(&procs, &|_| sv(&[log.as_str()]), Some(&dir)).unwrap();
        assert_eq!(d.id, UUID);
        assert_eq!(d.confidence, Confidence::Strong);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn fd_reader_handles_live_and_missing_pids() {
        // Our own pid has fds; a near-impossible pid has none. Neither raises.
        let own = read_fd_targets(std::process::id());
        assert!(!own.is_empty());
        assert!(own.iter().all(|t| !t.is_empty()));
        assert!(read_fd_targets(u32::MAX).is_empty());
    }

    #[test]
    fn preview_hit_matches_detect_on_every_leg() {
        // The popup preview and the resolve-time authority agree: Hit ⟺ Some.
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID, UUID2]);
        let log = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/session.jsonl");
        let lock = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let cases: Vec<(Vec<ForegroundProc>, Vec<String>)> = vec![
            // argv Certain
            (
                vec![proc(Some(7), "muse", &["muse", "resume", UUID])],
                vec![],
            ),
            // fd Strong
            (vec![proc(Some(7), "muse", &["muse"])], vec![log.clone()]),
            // lock Strong
            (vec![proc(Some(7), "muse", &["muse"])], vec![lock.clone()]),
            // miss: nothing at all
            (vec![proc(Some(7), "muse", &["muse"])], vec![]),
        ];
        for (procs, targets) in &cases {
            let preview = preview_from_snapshot(procs, &|_| targets.clone(), Some(&dir));
            let detected = detect_from_snapshot(procs, &|_| targets.clone(), Some(&dir));
            match (&preview, &detected) {
                (Preview::Hit(p), Some(d)) => assert_eq!(p, d),
                (Preview::Miss, None) => {}
                (p, d) => panic!("preview/detect disagree: {p:?} vs {d:?}"),
            }
        }
    }

    #[test]
    fn preview_reports_lock_ambiguity_with_count() {
        // Two distinct locks: resolve-time None, preview Ambiguous{2}.
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID, UUID2]);
        let a = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let b = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/.session.lock");
        let procs = vec![proc(Some(7), "muse", &["muse"])];
        assert_eq!(
            preview_from_snapshot(&procs, &|_| sv(&[a.as_str(), b.as_str()]), Some(&dir)),
            Preview::Ambiguous { count: 2 }
        );
        assert_eq!(
            detect_from_snapshot(&procs, &|_| sv(&[a.as_str(), b.as_str()]), Some(&dir)),
            None,
            "ambiguity stays the picker's job at resolve time"
        );
        // Duplicate fds to the same two locks still count two sessions.
        assert_eq!(
            preview_from_snapshot(
                &procs,
                &|_| sv(&[a.as_str(), b.as_str(), a.as_str()]),
                Some(&dir)
            ),
            Preview::Ambiguous { count: 2 }
        );
        // Subagent locks never join the count: own lock + subagent locks of
        // another session is a single-candidate hit, not ambiguity.
        let sub = format!(
            "/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/subagent/abc/.session.lock"
        );
        assert!(matches!(
            preview_from_snapshot(&procs, &|_| sv(&[a.as_str(), sub.as_str()]), Some(&dir)),
            Preview::Hit(_)
        ));
    }

    #[test]
    fn preview_ambiguity_counts_validated_ids_only() {
        // The popup count must match the resolve-time picker's rows exactly,
        // so a lock whose session is gone from the store joins neither.
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID]);
        let a = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        let b = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID2}/.session.lock");
        let procs = vec![proc(Some(7), "muse", &["muse"])];
        // Two distinct locks, one validated: a miss (the scanner picker
        // runs unchanged), not ambiguity over a stale lock.
        assert_eq!(
            preview_from_snapshot(&procs, &|_| sv(&[a.as_str(), b.as_str()]), Some(&dir)),
            Preview::Miss
        );
        assert!(
            ambiguous_from_snapshot(&procs, &|_| sv(&[a.as_str(), b.as_str()]), Some(&dir))
                .is_empty()
        );
        // Both validated: ambiguity with the matching count and ids.
        muse_store(&dir, &[UUID2]);
        assert_eq!(
            preview_from_snapshot(&procs, &|_| sv(&[a.as_str(), b.as_str()]), Some(&dir)),
            Preview::Ambiguous { count: 2 }
        );
        assert_eq!(
            ambiguous_from_snapshot(&procs, &|_| sv(&[a.as_str(), b.as_str()]), Some(&dir)),
            vec![UUID.to_string(), UUID2.to_string()]
        );
        // Neither validated: a miss.
        let (_g2, empty) = crate::testutil::tempdir();
        assert_eq!(
            preview_from_snapshot(&procs, &|_| sv(&[a.as_str(), b.as_str()]), Some(&empty)),
            Preview::Miss
        );
    }

    #[test]
    fn ambiguous_ids_empty_on_hit_or_miss() {
        // A silent hit leaves no ambiguity, and so does no lock signal.
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[UUID, UUID2]);
        let log = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/session.jsonl");
        let lock = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID}/.session.lock");
        // argv Certain
        let procs = vec![proc(Some(7), "muse", &["muse", "resume", UUID])];
        assert!(ambiguous_from_snapshot(&procs, &|_| Vec::new(), Some(&dir)).is_empty());
        // fd Strong
        let procs = vec![proc(Some(7), "muse", &["muse"])];
        assert!(ambiguous_from_snapshot(&procs, &|_| sv(&[log.as_str()]), Some(&dir)).is_empty());
        // single-lock Strong
        assert!(ambiguous_from_snapshot(&procs, &|_| sv(&[lock.as_str()]), Some(&dir)).is_empty());
        // miss: nothing at all
        assert!(ambiguous_from_snapshot(&procs, &|_| Vec::new(), Some(&dir)).is_empty());
        // miss: non-muse processes never contribute locks
        let procs = vec![proc(Some(7), "codex", &["codex"])];
        assert!(ambiguous_from_snapshot(&procs, &|_| sv(&[lock.as_str()]), Some(&dir)).is_empty());
    }

    #[test]
    fn popup_activity_shows_detected_id_and_source() {
        let base = "Activity unknown — muse doesn't report sessions to herdr.";
        for (provenance, source) in [
            (PROVENANCE_ARGV, "argv"),
            (PROVENANCE_FD, "fd"),
            (PROVENANCE_LOCK, "lock"),
        ] {
            let line = popup_activity(
                base,
                &Preview::Hit(Detected {
                    id: UUID.to_string(),
                    confidence: Confidence::Certain,
                    provenance,
                }),
            );
            assert_eq!(
                line,
                format!("Session 01a0da74… (auto-detected via {source})"),
                "{provenance}"
            );
        }
    }

    #[test]
    fn popup_activity_shows_ambiguous_count() {
        let base = "Activity unknown — muse doesn't report sessions to herdr.";
        assert_eq!(
            popup_activity(base, &Preview::Ambiguous { count: 2 }),
            "2 candidate sessions — you'll pick one"
        );
        assert_eq!(
            popup_activity(base, &Preview::Ambiguous { count: 3 }),
            "3 candidate sessions — you'll pick one"
        );
    }

    #[test]
    fn popup_activity_leaves_miss_and_known_activity_unchanged() {
        // No signal: the existing Activity-unknown line is kept verbatim.
        let unknown = "Activity unknown — muse doesn't report sessions to herdr.";
        assert_eq!(popup_activity(unknown, &Preview::Miss), unknown);
        let unknown_plural = "Activity unknown — gemini, muse don't report sessions to herdr.";
        assert_eq!(
            popup_activity(unknown_plural, &Preview::Miss),
            unknown_plural
        );
        // A line that already names an activity is never replaced, even on a
        // hit — detection only clarifies the unknown case.
        let known = "Last activity 3 days ago.";
        let hit = Preview::Hit(Detected {
            id: UUID.to_string(),
            confidence: Confidence::Certain,
            provenance: PROVENANCE_ARGV,
        });
        assert_eq!(popup_activity(known, &hit), known);
        assert_eq!(
            popup_activity(known, &Preview::Ambiguous { count: 2 }),
            known
        );
        assert_eq!(popup_activity(known, &Preview::Miss), known);
        assert_eq!(
            popup_activity("No activity recorded for this tab yet.", &hit),
            "No activity recorded for this tab yet."
        );
    }
}
