//! Resolve agent sessions the archive-tab popup needs but herdr did not report.
//!
//! Port of `shelf/manual.py`, generalized from muse-only to every scanner
//! kind (DESIGN.md §7.3). `resolve_missing_sessions` returns
//! `{pane_id: session_value}` for panes the user confirmed, omitting panes
//! explicitly archived as plain shells — or None to abort the whole archive.

use crate::agents;
use crate::scan;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const MAX_ATTEMPTS: u32 = 3;

pub fn age(mtime_secs: f64) -> String {
    let secs = (scan::now_secs() - mtime_secs).max(0.0) as u64;
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 90 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 48 * 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Session,
    Shell,
    Abort,
}

/// Read one answer line. Returns Err on EOF/interrupt so the caller can
/// cancel quietly.
pub type InputFn<'a> = &'a mut dyn FnMut(&str) -> Result<String, ()>;
pub type PrintFn<'a> = &'a mut dyn FnMut(&str);

/// A ("session", value, provenance), ("shell", ..) or ("abort", ..) verdict.
type Pick = (Verdict, Option<String>, Option<String>);

/// Validate a pasted/typed session id: the generic format check first, then
/// strict existence in the kind's local store where one is verifiable
/// (muse also accepts Session Names). Number-picks from scanner lists bypass
/// this (valid by construction). `Ok(())` accepts; `Err(reason)` rejects
/// with the why (bad format vs not found) for the re-prompt.
fn validate_paste(agent: &str, answer: &str, store_base: Option<&Path>) -> Result<(), String> {
    if !agents::valid_session_value(agent, answer) {
        return Err(format!(
            "{answer:?} is not a usable {agent} session id (bad format)."
        ));
    }
    match scan::session_exists(agent, answer, store_base) {
        None | Some(true) => Ok(()),
        Some(false) => Err(format!(
            "No {agent} session {answer:?} found in the local session store."
        )),
    }
}

/// Numbered-list branch shared by the scanner picker and the
/// ambiguity-scoped picker: `header` names the list, `list_mark` is the
/// `resolved_by` marker for a default/digit pick.
fn pick_from_list(
    agent: &str,
    cands: Vec<scan::Candidate>,
    header: &str,
    list_mark: &str,
    print_fn: PrintFn<'_>,
    input_fn: InputFn<'_>,
    store_base: Option<&Path>,
) -> Result<Pick, ()> {
    print_fn(header);
    for (i, c) in cands.iter().enumerate() {
        print_fn(&format!(
            "  {}. {}{}  {} ago",
            i + 1,
            if c.matched { "*" } else { "~" },
            c.id.chars().take(8).collect::<String>(),
            age(c.mtime)
        ));
    }
    for _ in 0..MAX_ATTEMPTS {
        let answer = input_fn("Session [1], paste an id, 's' for shell, 'q' to cancel: ")?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok((
                Verdict::Session,
                Some(cands[0].id.clone()),
                Some(list_mark.to_string()),
            ));
        }
        if answer == "q" {
            return Ok((Verdict::Abort, None, None));
        }
        if answer == "s" {
            return Ok((Verdict::Shell, None, None));
        }
        if !answer.is_empty()
            && answer.bytes().all(|b| b.is_ascii_digit())
            && let Ok(n) = answer.parse::<usize>()
            && (1..=cands.len()).contains(&n)
        {
            return Ok((
                Verdict::Session,
                Some(cands[n - 1].id.clone()),
                Some(list_mark.to_string()),
            ));
        }
        match validate_paste(agent, answer, store_base) {
            Ok(()) => {
                return Ok((
                    Verdict::Session,
                    Some(answer.to_string()),
                    Some("manual-paste".to_string()),
                ));
            }
            Err(reason) => print_fn(&format!(
                "{reason} Pick 1-{}, paste an id, 's' or 'q'.",
                cands.len()
            )),
        }
    }
    print_fn("Too many invalid answers; cancelling.");
    Ok((Verdict::Abort, None, None))
}

/// The ambiguity-scoped picker: detection found 2+ validated live sessions
/// for this pane and could not choose silently, so the user picks from
/// exactly those — never the full scanner history. Same answers as the
/// scanner list (default/digit/paste/shell/cancel); a list pick records
/// [`crate::detect::PROVENANCE_AMBIGUOUS`].
fn pick_ambiguous_sessions(
    pane: &Map<String, Value>,
    agent: &str,
    cands: Vec<scan::Candidate>,
    print_fn: PrintFn<'_>,
    input_fn: InputFn<'_>,
    store_base: Option<&Path>,
) -> Result<Pick, ()> {
    let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("?");
    let header = format!(
        "Pane {pane_id} has {} candidate {agent} sessions; pick which one to resume ('*' matched this directory):",
        cands.len()
    );
    pick_from_list(
        agent,
        cands,
        &header,
        crate::detect::PROVENANCE_AMBIGUOUS,
        print_fn,
        input_fn,
        store_base,
    )
}

fn pick_scanned_session(
    pane: &Map<String, Value>,
    agent: &str,
    cands: Vec<scan::Candidate>,
    print_fn: PrintFn<'_>,
    input_fn: InputFn<'_>,
    store_base: Option<&Path>,
) -> Result<Pick, ()> {
    let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("?");
    let cwd = pane_cwd(pane);
    let scanner_mark = format!("scanner:{agent}");
    if cands.is_empty() {
        print_fn(&format!(
            "Pane {pane_id} runs {agent}, but no {agent} sessions were found{}.",
            if cwd.is_empty() {
                String::new()
            } else {
                format!(" for {cwd}")
            }
        ));
        for _ in 0..MAX_ATTEMPTS {
            let answer = input_fn("Paste a session id, 's' for a plain shell, 'q' to cancel: ")?;
            let answer = answer.trim();
            if answer == "q" || answer.is_empty() {
                return Ok((Verdict::Abort, None, None));
            }
            if answer == "s" {
                return Ok((Verdict::Shell, None, None));
            }
            match validate_paste(agent, answer, store_base) {
                Ok(()) => {
                    return Ok((
                        Verdict::Session,
                        Some(answer.to_string()),
                        Some("manual-paste".to_string()),
                    ));
                }
                Err(reason) => print_fn(&reason),
            }
        }
        print_fn("Too many invalid answers; cancelling.");
        return Ok((Verdict::Abort, None, None));
    }
    let header = format!(
        "Pane {pane_id} runs {agent} with no reported session; pick which one to resume ('*' matched this directory):"
    );
    pick_from_list(
        agent,
        cands,
        &header,
        &scanner_mark,
        print_fn,
        input_fn,
        store_base,
    )
}

fn shell_or_abort(
    pane: &Map<String, Value>,
    reason: &str,
    print_fn: PrintFn<'_>,
    input_fn: InputFn<'_>,
) -> Result<(Verdict, Option<String>), ()> {
    let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("?");
    print_fn(&format!("Pane {pane_id}: {reason}"));
    let answer = input_fn("Archive it as a plain shell? [y/N] ")?;
    if answer.trim().to_lowercase() == "y" {
        return Ok((Verdict::Shell, None));
    }
    Ok((Verdict::Abort, None))
}

/// Confirmed sessions plus their `resolved_by` provenance markers
/// (`scanner:<kind>` for a picked scanner candidate, `detect:ambiguous` for
/// a picked ambiguity-scoped candidate, `manual-paste` for a pasted id,
/// `detect:argv`/`detect:fd`/`detect:lock` for silent detections).
#[derive(Debug, Default)]
pub struct Resolved {
    pub overrides: BTreeMap<String, String>,
    pub provenance: BTreeMap<String, String>,
}

/// Confirmed panes, or None to abort. `Err(())` (EOF/interrupt) propagates
/// so the caller can cancel quietly.
#[allow(clippy::result_unit_err)]
pub fn resolve_missing_sessions(
    panes: &[Map<String, Value>],
    table: &BTreeMap<String, agents::Entry>,
    input_fn: InputFn<'_>,
    print_fn: PrintFn<'_>,
) -> Result<Option<Resolved>, ()> {
    resolve_inner(panes, table, input_fn, print_fn, None, None)
}

/// `resolve_missing_sessions` plus live-session detection: muse panes with
/// no reported session first try `detect::detect_live_session`; a
/// Certain/Strong hit is used silently (provenance recorded), anything else
/// falls through to the unchanged picker flow. Non-muse panes never touch
/// detection, and detection never prompts.
#[allow(clippy::result_unit_err)]
pub fn resolve_missing_sessions_with_client(
    panes: &[Map<String, Value>],
    table: &BTreeMap<String, agents::Entry>,
    input_fn: InputFn<'_>,
    print_fn: PrintFn<'_>,
    client: &mut dyn crate::Herdr,
) -> Result<Option<Resolved>, ()> {
    resolve_inner(panes, table, input_fn, print_fn, Some(client), None)
}

/// Test seam: like `resolve_missing_sessions_with_client` but detection (and
/// paste validation) use `store_base` instead of the real muse store.
/// Production passes None.
#[doc(hidden)]
#[allow(clippy::result_unit_err)]
pub fn resolve_missing_sessions_with_store(
    panes: &[Map<String, Value>],
    table: &BTreeMap<String, agents::Entry>,
    input_fn: InputFn<'_>,
    print_fn: PrintFn<'_>,
    client: &mut dyn crate::Herdr,
    store_base: Option<&Path>,
) -> Result<Option<Resolved>, ()> {
    resolve_inner(panes, table, input_fn, print_fn, Some(client), store_base)
}

#[allow(clippy::result_unit_err)]
fn resolve_inner(
    panes: &[Map<String, Value>],
    table: &BTreeMap<String, agents::Entry>,
    input_fn: InputFn<'_>,
    print_fn: PrintFn<'_>,
    mut client: Option<&mut dyn crate::Herdr>,
    store_base: Option<&Path>,
) -> Result<Option<Resolved>, ()> {
    let mut resolved = Resolved::default();
    for pane in panes {
        let agent = pane.get("agent").and_then(Value::as_str).unwrap_or("");
        if agent.is_empty() {
            continue;
        }
        let session = pane.get("agent_session").and_then(Value::as_object);
        if session.is_some_and(|s| {
            s.get("value")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty())
        }) {
            // A reported value stays on the existing assess/capture path.
            // Only a missing value needs a question.
            continue;
        }
        // Live detection before the picker: muse-only (the signals and the
        // store are muse-specific), and only when the kind is resumable. A
        // hit is used silently; lock-leg ambiguity scopes the picker below
        // to exactly the validated live sessions; anything else falls
        // through to the scanner picker unchanged.
        let mut scoped: Option<Vec<scan::Candidate>> = None;
        if agent == "muse" && table.contains_key(agent) {
            if let (Some(pane_id), Some(c)) = (
                pane.get("pane_id").and_then(Value::as_str),
                client.as_deref_mut(),
            ) {
                let hit = match store_base {
                    Some(base) => {
                        crate::detect::detect_live_session_with_store(pane_id, c, Some(base))
                    }
                    None => crate::detect::detect_live_session(pane_id, c),
                };
                if let Some(d) = hit {
                    print_fn(&format!(
                        "Pane {pane_id}: using live muse session {} ({}).",
                        d.id.chars().take(8).collect::<String>(),
                        d.provenance
                    ));
                    resolved.overrides.insert(pane_id.to_string(), d.id);
                    resolved
                        .provenance
                        .insert(pane_id.to_string(), d.provenance.to_string());
                    continue;
                }
                // No silent hit: recompute ambiguity fresh at resolve time
                // (the popup preview is informational only — never trusted).
                let ids = match store_base {
                    Some(base) => {
                        crate::detect::ambiguous_live_sessions_with_store(pane_id, c, Some(base))
                    }
                    None => crate::detect::ambiguous_live_sessions(pane_id, c),
                };
                if ids.len() >= 2 {
                    let cands =
                        crate::scan::muse::candidates_for_ids(&ids, pane_cwd(pane), store_base);
                    if !cands.is_empty() {
                        scoped = Some(cands);
                    }
                }
            }
        }
        let (kind, value, provenance) = if table.contains_key(agent) {
            match scoped {
                Some(cands) => {
                    pick_ambiguous_sessions(pane, agent, cands, print_fn, input_fn, store_base)?
                }
                None => match scan::candidates_for(agent, pane_cwd(pane)) {
                    Some(cands) => {
                        pick_scanned_session(pane, agent, cands, print_fn, input_fn, store_base)?
                    }
                    None => {
                        let (k, v) =
                            shell_or_abort(pane, "has no session id.", print_fn, input_fn)?;
                        (k, v, None)
                    }
                },
            }
        } else {
            let (k, v) = shell_or_abort(
                pane,
                &format!("runs {agent}, which herdr-archive cannot resume."),
                print_fn,
                input_fn,
            )?;
            (k, v, None)
        };
        match kind {
            Verdict::Abort => return Ok(None),
            Verdict::Session => {
                if let Some(v) = value {
                    if let Some(pid) = pane.get("pane_id").and_then(Value::as_str) {
                        resolved.overrides.insert(pid.to_string(), v);
                        if let Some(p) = provenance {
                            resolved.provenance.insert(pid.to_string(), p);
                        }
                    }
                }
            }
            Verdict::Shell => {}
        }
    }
    Ok(Some(resolved))
}

/// Working directory used for session matching: `cwd`, else
/// `foreground_cwd`, else empty. Shared with popup sizing so both sides
/// scan the same directory.
pub fn pane_cwd(pane: &Map<String, Value>) -> &str {
    pane.get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            pane.get("foreground_cwd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pane(agent: &str, pid: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("pane_id".to_string(), json!(pid));
        m.insert("agent".to_string(), json!(agent));
        m.insert("cwd".to_string(), json!("/work/a"));
        m
    }

    struct Script {
        answers: Vec<String>,
        printed: Vec<String>,
    }

    impl Script {
        fn new(answers: &[&str]) -> Self {
            Script {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                printed: Vec::new(),
            }
        }
        fn run(
            &mut self,
            panes: &[Map<String, Value>],
            table: &BTreeMap<String, agents::Entry>,
        ) -> Result<Option<Resolved>, ()> {
            let mut answers = std::mem::take(&mut self.answers);
            answers.reverse();
            let printed = &mut self.printed;
            let mut input = |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
            let mut print = |s: &str| printed.push(s.to_string());
            resolve_missing_sessions(panes, table, &mut input, &mut print)
        }
    }

    fn table() -> BTreeMap<String, agents::Entry> {
        agents::table(None)
    }

    #[test]
    fn skips_reported_and_shells() {
        let t = table();
        let mut reported = pane("claude", "p1");
        reported.insert(
            "agent_session".to_string(),
            json!({"agent": "claude", "value": "v"}),
        );
        let mut shell = Map::new();
        shell.insert("pane_id".to_string(), json!("p2"));
        let mut s = Script::new(&[]);
        let out = s.run(&[reported, shell], &t).unwrap().unwrap().overrides;
        assert!(out.is_empty());
        assert!(s.printed.is_empty());
    }

    #[test]
    fn unknown_agent_shell_or_abort() {
        let t = table();
        let p = pane("weird-agent", "p1");
        let mut s = Script::new(&["y"]);
        let out = s
            .run(std::slice::from_ref(&p), &t)
            .unwrap()
            .unwrap()
            .overrides;
        assert!(out.is_empty()); // shell: omitted
        let mut s = Script::new(&["n"]);
        assert!(s.run(&[p], &t).unwrap().is_none());
    }

    #[test]
    fn known_agent_without_scanner_shell_or_abort() {
        let t = table();
        let p = pane("claude", "p1");
        let mut s = Script::new(&["y"]);
        let out = s
            .run(std::slice::from_ref(&p), &t)
            .unwrap()
            .unwrap()
            .overrides;
        assert!(out.is_empty());
        let mut s = Script::new(&[""]);
        assert!(s.run(&[p], &t).unwrap().is_none());
    }

    /// Fixture muse store: session dirs with logs under `base/2026/01/02/`.
    fn muse_store(base: &Path, sessions: &[(&str, &[&str])]) {
        for (id, lines) in sessions {
            let d = base.join("2026").join("01").join("02").join(id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("session.jsonl"), lines.join("\n")).unwrap();
        }
    }

    fn named(id: &str, name: &str) -> String {
        format!(
            r#"{{"payload_type":"session.name.changed","payload":{{"new_name":"{name}","session_id":"{id}"}}}}"#
        )
    }

    fn scripted(answers: &[&str]) -> (Vec<String>, impl FnMut(&str) -> Result<String, ()> + use<>) {
        let mut answers: Vec<String> = answers.iter().map(|s| s.to_string()).collect();
        answers.reverse();
        let printed = Vec::new();
        let input = move |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
        (printed, input)
    }

    #[test]
    fn no_candidates_paste_shell_quit() {
        // Exercise pick_scanned_session directly with no candidates and a
        // fixture store (env overrides would be racy under parallel tests).
        let p = pane("muse", "p1");
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[("sess-1", &[r#"{"workspace_root":"/work/a"}"#])]);
        let mut printed = Vec::new();
        let mut print = |s: &str| printed.push(s.to_string());
        // paste of a stored id
        let mut input = |_: &str| -> Result<String, ()> { Ok("sess-1".to_string()) };
        let (k, v, prov) =
            pick_scanned_session(&p, "muse", vec![], &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("sess-1".to_string()),
                Some("manual-paste".to_string())
            )
        );
        // shell
        let mut input = |_: &str| -> Result<String, ()> { Ok("s".to_string()) };
        let (k, ..) =
            pick_scanned_session(&p, "muse", vec![], &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(k, Verdict::Shell);
        // quit
        let mut input = |_: &str| -> Result<String, ()> { Ok("q".to_string()) };
        let (k, ..) =
            pick_scanned_session(&p, "muse", vec![], &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(k, Verdict::Abort);
    }

    #[test]
    fn pasted_garbage_rejected_with_reason_then_shell() {
        let p = pane("muse", "p1");
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[("sess-1", &[r#"{"workspace_root":"/work/a"}"#])]);
        // the live failure ("uq") must reject with a not-found reason and
        // re-prompt instead of archiving garbage
        let (mut printed, mut input) = scripted(&["uq", "s"]);
        let mut print = |s: &str| printed.push(s.to_string());
        let (k, ..) =
            pick_scanned_session(&p, "muse", vec![], &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(k, Verdict::Shell);
        assert!(
            printed
                .iter()
                .any(|l| l.contains("No muse session") && l.contains("\"uq\"")),
            "{printed:?}"
        );
    }

    #[test]
    fn pasted_unknown_uuid_rejected_bad_format_says_so() {
        let p = pane("muse", "p1");
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[("sess-1", &[r#"{"x":1}"#])]);
        let missing = "00000000-0000-4000-8000-000000000000";
        let (mut printed, mut input) = scripted(&[missing, "-bad", "q"]);
        let mut print = |s: &str| printed.push(s.to_string());
        let (k, ..) =
            pick_scanned_session(&p, "muse", vec![], &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(k, Verdict::Abort);
        assert!(
            printed.iter().any(|l| l.contains("No muse session")
                && l.contains("00000000-0000-4000-8000-000000000000")),
            "{printed:?}"
        );
        assert!(
            printed
                .iter()
                .any(|l| l.contains("bad format") && l.contains("\"-bad\"")),
            "{printed:?}"
        );
    }

    #[test]
    fn pasted_valid_uuid_and_name_accepted() {
        let p = pane("muse", "p1");
        let (_g, dir) = crate::testutil::tempdir();
        let uuid = "01a0da74-9f52-7760-a301-60a6f87abcf5";
        let name_line = named(uuid, "gentle-pisces");
        muse_store(
            &dir,
            &[(uuid, &[name_line.as_str()]), ("sess-1", &[r#"{"x":1}"#])],
        );
        // uuid
        let (mut printed, mut input) = scripted(&[uuid]);
        let mut print = |s: &str| printed.push(s.to_string());
        let (k, v, prov) =
            pick_scanned_session(&p, "muse", vec![], &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some(uuid.to_string()),
                Some("manual-paste".to_string())
            )
        );
        // session name (stored verbatim: `muse resume` accepts names)
        let (mut printed, mut input) = scripted(&["gentle-pisces"]);
        let mut print = |s: &str| printed.push(s.to_string());
        let (k, v, prov) =
            pick_scanned_session(&p, "muse", vec![], &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("gentle-pisces".to_string()),
                Some("manual-paste".to_string())
            )
        );
    }

    #[test]
    fn no_candidates_three_strikes_abort() {
        let p = pane("muse", "p1");
        let (_g, dir) = crate::testutil::tempdir();
        let (mut printed, mut input) = scripted(&["uq", "also-nope", "-bad"]);
        let mut print = |s: &str| printed.push(s.to_string());
        let (k, ..) =
            pick_scanned_session(&p, "muse", vec![], &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(k, Verdict::Abort);
        assert!(
            printed
                .iter()
                .any(|l| l.contains("Too many invalid answers")),
            "{printed:?}"
        );
    }

    #[test]
    fn unverifiable_store_keeps_generic_check() {
        // amp threads are server-side: a well-formed paste is accepted.
        let p = pane("amp", "p1");
        let (mut printed, mut input) = scripted(&["T-xyz"]);
        let mut print = |s: &str| printed.push(s.to_string());
        let (k, v, prov) =
            pick_scanned_session(&p, "amp", vec![], &mut print, &mut input, None).unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("T-xyz".to_string()),
                Some("manual-paste".to_string())
            )
        );
        // ...but bad format still rejects and re-prompts.
        let (mut printed, mut input) = scripted(&["-bad", "s"]);
        let mut print = |s: &str| printed.push(s.to_string());
        let (k, ..) =
            pick_scanned_session(&p, "amp", vec![], &mut print, &mut input, None).unwrap();
        assert_eq!(k, Verdict::Shell);
        assert!(
            printed.iter().any(|l| l.contains("bad format")),
            "{printed:?}"
        );
    }

    #[test]
    fn candidate_list_default_number_reprompt_abort() {
        let p = pane("muse", "p1");
        let cands = || {
            vec![
                scan::Candidate {
                    id: "aaa".to_string(),
                    mtime: scan::now_secs(),
                    matched: true,
                },
                scan::Candidate {
                    id: "bbb".to_string(),
                    mtime: scan::now_secs() - 100.0,
                    matched: false,
                },
            ]
        };
        let mut printed = Vec::new();
        let mut print = |s: &str| printed.push(s.to_string());
        // Enter = newest
        let mut input = |_: &str| -> Result<String, ()> { Ok(String::new()) };
        let (k, v, prov) =
            pick_scanned_session(&p, "muse", cands(), &mut print, &mut input, None).unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("aaa".to_string()),
                Some("scanner:muse".to_string())
            )
        );
        // digit
        let mut input = |_: &str| -> Result<String, ()> { Ok("2".to_string()) };
        let (k, v, prov) =
            pick_scanned_session(&p, "muse", cands(), &mut print, &mut input, None).unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("bbb".to_string()),
                Some("scanner:muse".to_string())
            )
        );
        // reprompt then valid ("-bad" is not a usable id, "1" picks)
        let mut answers = vec!["-bad".to_string(), "1".to_string()];
        answers.reverse();
        let mut input = |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
        let (k, v, prov) =
            pick_scanned_session(&p, "muse", cands(), &mut print, &mut input, None).unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("aaa".to_string()),
                Some("scanner:muse".to_string())
            )
        );
        // 3 bad answers abort
        let mut n = 0;
        let mut input = |_: &str| -> Result<String, ()> {
            n += 1;
            Ok(format!("-bad-{n}"))
        };
        let (k, ..) =
            pick_scanned_session(&p, "muse", cands(), &mut print, &mut input, None).unwrap();
        assert_eq!(k, Verdict::Abort);
    }

    #[test]
    #[cfg(unix)]
    fn stale_enter_drained_before_picker_waits_for_real_answer() {
        // The live incident: a stray Enter buffered after the single-key
        // confirm must be drained before the picker prompt, or the picker
        // consumes it as default-accept and the user never chooses.
        use std::io::{BufRead, Write};
        use std::os::fd::AsRawFd;
        let cands = || {
            vec![
                scan::Candidate {
                    id: "aaa".to_string(),
                    mtime: scan::now_secs(),
                    matched: true,
                },
                scan::Candidate {
                    id: "bbb".to_string(),
                    mtime: scan::now_secs() - 100.0,
                    matched: false,
                },
            ]
        };
        fn pipe_input<'a>(
            rd: &'a std::fs::File,
        ) -> impl FnMut(&str) -> Result<String, ()> + use<'a> {
            let mut reader = std::io::BufReader::new(rd);
            move |_: &str| -> Result<String, ()> {
                let mut line = String::new();
                reader.read_line(&mut line).map_err(|_| ())?;
                if line.is_empty() {
                    return Err(());
                }
                Ok(line)
            }
        }
        let p = pane("muse", "p1");
        // Control (no drain): the stale newline is consumed as Enter, so
        // the default wins without the user choosing.
        let (rd, mut wr) = crate::testutil::pipe();
        wr.write_all(b"\n2\n").unwrap();
        drop(wr);
        let mut print = |_: &str| {};
        let (k, v, _) =
            pick_scanned_session(&p, "muse", cands(), &mut print, &mut pipe_input(&rd), None)
                .unwrap();
        assert_eq!((k, v.as_deref()), (Verdict::Session, Some("aaa")));
        // Fix: draining first makes the picker wait for the real answer.
        let (rd, mut wr) = crate::testutil::pipe();
        wr.write_all(b"\n").unwrap(); // stray Enter after the confirm key
        crate::confirm::drain_fd(rd.as_raw_fd());
        wr.write_all(b"2\n").unwrap(); // typed AT the picker
        drop(wr);
        let mut print = |_: &str| {};
        let (k, v, _) =
            pick_scanned_session(&p, "muse", cands(), &mut print, &mut pipe_input(&rd), None)
                .unwrap();
        assert_eq!((k, v.as_deref()), (Verdict::Session, Some("bbb")));
    }

    #[test]
    fn candidates_branch_paste_reprompts_with_reason() {
        let p = pane("muse", "p1");
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[("aaa", &[r#"{"x":1}"#])]);
        let cands = vec![
            scan::Candidate {
                id: "aaa".to_string(),
                mtime: scan::now_secs(),
                matched: true,
            },
            scan::Candidate {
                id: "bbb".to_string(),
                mtime: scan::now_secs() - 100.0,
                matched: false,
            },
        ];
        // garbage paste rejects with a reason and re-prompts; the digit pick
        // then bypasses validation (valid by construction, not in the store)
        let (mut printed, mut input) = scripted(&["uq", "2"]);
        let mut print = |s: &str| printed.push(s.to_string());
        let (k, v, prov) =
            pick_scanned_session(&p, "muse", cands, &mut print, &mut input, Some(&dir)).unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("bbb".to_string()),
                Some("scanner:muse".to_string())
            )
        );
        assert!(
            printed.iter().any(|l| l.contains("No muse session")),
            "{printed:?}"
        );
    }

    #[test]
    fn ambiguous_list_names_count_and_marks_picks() {
        // The ambiguity-scoped list: header names the exact count, list
        // picks record detect:ambiguous, and paste/shell/cancel still work.
        let p = pane("muse", "p1");
        let (_g, dir) = crate::testutil::tempdir();
        muse_store(&dir, &[("aaa", &[r#"{"x":1}"#])]);
        let cands = || {
            vec![
                scan::Candidate {
                    id: "aaa".to_string(),
                    mtime: scan::now_secs(),
                    matched: true,
                },
                scan::Candidate {
                    id: "bbb".to_string(),
                    mtime: scan::now_secs() - 100.0,
                    matched: false,
                },
            ]
        };
        // Default: newest with the ambiguity marker, header naming the count.
        let mut printed = Vec::new();
        let mut print = |s: &str| printed.push(s.to_string());
        let mut input = |_: &str| -> Result<String, ()> { Ok(String::new()) };
        let (k, v, prov) =
            pick_ambiguous_sessions(&p, "muse", cands(), &mut print, &mut input, Some(&dir))
                .unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("aaa".to_string()),
                Some("detect:ambiguous".to_string())
            )
        );
        assert!(
            printed
                .iter()
                .any(|l| l.contains("2 candidate muse sessions")),
            "{printed:?}"
        );
        let rows: Vec<_> = printed.iter().filter(|l| l.starts_with("  ")).collect();
        assert_eq!(rows.len(), 2, "{printed:?}");
        // Digit pick keeps the marker.
        let mut printed = Vec::new();
        let mut print = |s: &str| printed.push(s.to_string());
        let mut input = |_: &str| -> Result<String, ()> { Ok("2".to_string()) };
        let (k, v, prov) =
            pick_ambiguous_sessions(&p, "muse", cands(), &mut print, &mut input, Some(&dir))
                .unwrap();
        assert_eq!(
            (k, v, prov),
            (
                Verdict::Session,
                Some("bbb".to_string()),
                Some("detect:ambiguous".to_string())
            )
        );
        // Paste of a stored id, shell, and cancel round out the answers.
        for (answer, want) in [
            ("aaa", Verdict::Session),
            ("s", Verdict::Shell),
            ("q", Verdict::Abort),
        ] {
            let mut printed = Vec::new();
            let mut print = |s: &str| printed.push(s.to_string());
            let mut input = |_: &str| -> Result<String, ()> { Ok(answer.to_string()) };
            let (k, ..) =
                pick_ambiguous_sessions(&p, "muse", cands(), &mut print, &mut input, Some(&dir))
                    .unwrap();
            assert_eq!(k, want, "answer {answer:?}");
        }
    }
}
