//! Capture a tab into an archive record, and store records on disk.
//!
//! Port of `shelf/archive.py`, plus the additive `resume_argv` /
//! `resolved_by` / `tool` fields (DESIGN.md §3.1).
//!
//! Discovery leg 2 (agent-reported `resume_argv` in pane payloads) stays
//! OUT: herdr 0.9.3 does not expose it in `pane.list`/`pane.get` (U6
//! resolved negative). Restore still replays a recorded `resume_argv`
//! verbatim when one is present (forward compatibility).

use crate::agents;
use crate::api::HerdrError;
use crate::history;
use crate::util::{Ts, atomic_write_json, iso, read_json};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// The tab cannot be archived right now. Nothing was changed.
#[derive(Debug, Clone)]
pub struct Skip(pub String);

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Skip {}

// herdr's own layout.apply limits. A layout past either limit could never
// be restored, so capture() refuses it rather than writing a record that
// layout.apply would reject on restore.
pub const MAX_LAYOUT_PANES: usize = 24;

/// Activity lookup shared by capture/archive paths.
pub type ActivityOf<'a> = dyn Fn(&str, &str, Option<&str>) -> Option<Ts> + 'a;
pub const MAX_LAYOUT_DEPTH: usize = 16;

/// Archive id shape: YYYYMMDDTHHMMSSZ-<6 hex>. Fullmatch: a crafted id must
/// never build a path outside the archive root.
pub fn valid_archive_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.len() != 8 + 1 + 6 + 1 + 1 + 6 {
        return false;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    let hex =
        |i: usize| b[i].is_ascii_hexdigit() && (b[i].is_ascii_digit() || b[i].is_ascii_lowercase());
    // Note: Python's `[0-9a-f]` excludes uppercase; match it.
    for i in 0..8 {
        if !digit(i) {
            return false;
        }
    }
    if b[8] != b'T' {
        return false;
    }
    for i in 9..15 {
        if !digit(i) {
            return false;
        }
    }
    if b[15] != b'Z' || b[16] != b'-' {
        return false;
    }
    for i in 17..23 {
        if !hex(i) {
            return false;
        }
    }
    true
}

fn fsync_dir(path: &Path) {
    if let Ok(dir) = std::fs::File::open(path) {
        let _ = dir.sync_all();
    }
}

/// Records live in `<state>/archive/<id>/record.json`, session copies under
/// `sessions/`.
pub struct Archive {
    pub root: PathBuf,
}

impl Archive {
    pub fn new(state_dir: &Path) -> Self {
        Archive {
            root: state_dir.join("archive"),
        }
    }

    fn dir(&self, archive_id: &str) -> Result<PathBuf, KeyError> {
        if !valid_archive_id(archive_id) {
            return Err(KeyError(archive_id.to_string()));
        }
        Ok(self.root.join(archive_id))
    }

    /// Save a record plus `(source path, rel path)` session files. On any
    /// failure the partial folder is removed.
    pub fn save(&self, record: &Value, session_files: &[(PathBuf, String)]) -> std::io::Result<()> {
        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
        let folder = self
            .dir(id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.0))?;
        let result = (|| -> std::io::Result<()> {
            for (src, rel) in session_files {
                let dest = folder.join("sessions").join(rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if src.is_dir() {
                    copy_dir_all(src, &dest)?;
                } else {
                    std::fs::copy(src, &dest)?;
                }
            }
            atomic_write_json(&folder.join("record.json"), record)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&folder);
        }
        result?;
        fsync_dir(&self.root);
        Ok(())
    }

    /// Newest-`archived_at`-first records; requires `rec.id == dirname`.
    pub fn list(&self) -> Vec<Value> {
        let mut records = Vec::new();
        if self.root.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&self.root) {
                for entry in rd.filter_map(|e| e.ok()) {
                    let path = entry.path().join("record.json");
                    let rec = read_json(&path, Value::Null).unwrap_or(Value::Null);
                    if let Some(o) = rec.as_object() {
                        let dirname = entry.file_name().to_string_lossy().into_owned();
                        if o.get("id").and_then(Value::as_str) == Some(dirname.as_str()) {
                            records.push(rec);
                        }
                    }
                }
            }
        }
        records.sort_by(|a, b| {
            let x = a.get("archived_at").and_then(Value::as_str).unwrap_or("");
            let y = b.get("archived_at").and_then(Value::as_str).unwrap_or("");
            y.cmp(x)
        });
        records
    }

    pub fn load(&self, archive_id: &str) -> Result<Value, KeyError> {
        let path = self.dir(archive_id)?.join("record.json");
        match read_json(&path, Value::Null) {
            Ok(v @ Value::Object(_)) => Ok(v),
            _ => Err(KeyError(archive_id.to_string())),
        }
    }

    pub fn delete(&self, archive_id: &str) -> Result<(), KeyError> {
        let folder = self.dir(archive_id)?;
        let _ = std::fs::remove_file(folder.join("record.json"));
        let _ = std::fs::remove_dir_all(&folder);
        Ok(())
    }

    /// Copy archived Claude session files back where Claude deleted them.
    pub fn put_back_sessions(&self, record: &Value) -> Vec<String> {
        let mut restored = Vec::new();
        let base = history::claude_home();
        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
        let Ok(folder) = self.dir(id) else {
            return restored;
        };
        let copies = record
            .get("session_copies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for rel in copies.iter().filter_map(Value::as_str) {
            if rel.is_empty()
                || Path::new(rel).is_absolute()
                || Path::new(rel).components().any(|c| c.as_os_str() == "..")
            {
                continue;
            }
            let target = base.join(rel);
            let src = folder.join("sessions").join(rel);
            if target.exists() || !src.exists() {
                continue;
            }
            if let Some(parent) = target.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    continue;
                }
            }
            let ok = if src.is_dir() {
                copy_dir_all(&src, &target).is_ok()
            } else {
                std::fs::copy(&src, &target).is_ok()
            };
            if ok {
                restored.push(rel.to_string());
            }
        }
        restored
    }
}

fn copy_dir_all(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dst)?;
        } else {
            std::fs::copy(entry.path(), &dst)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct KeyError(pub String);

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for KeyError {}

fn random_hex_3() -> String {
    let mut buf = [0u8; 3];
    if let Ok(f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let mut f = f;
        let _ = f.read_exact(&mut buf);
    } else {
        // Fallback: pid ^ nanos (still unique in practice for archive ids).
        let n = std::process::id() as u64
            ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
        buf = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
    }
    format!("{:02x}{:02x}{:02x}", buf[0], buf[1], buf[2])
}

pub fn new_id(now: Ts) -> String {
    // iso() gives YYYY-MM-DDTHH:MM:SSZ; the id drops the separators.
    let s = iso(now);
    format!(
        "{}{}{}T{}{}{}Z-{}",
        &s[0..4],
        &s[5..7],
        &s[8..10],
        &s[11..13],
        &s[14..16],
        &s[17..19],
        random_hex_3()
    )
}

/// The agent's command line as the user typed it (outermost-match rule).
fn launch_argv(
    client: &mut dyn crate::Herdr,
    pane_id: &str,
    entry: &agents::Entry,
) -> Result<Option<Vec<String>>, HerdrError> {
    let info = client
        .call("pane.process_info", serde_json::json!({"pane_id": pane_id}))?
        .get("process_info")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let mut matches: Vec<Vec<String>> = Vec::new();
    for proc in info
        .get("foreground_processes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let argv: Vec<String> = proc
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if argv.is_empty() {
            continue;
        }
        // The reported process name goes through the same matcher: versioned
        // binaries (muse-bin-1.4.x-…) report versioned names too. Exact for
        // every kind except muse (basename-prefix), so behavior is unchanged
        // outside muse.
        if proc
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| agents::matches_program(entry, n))
            || agents::matches_program(entry, &argv[0])
        {
            matches.push(argv);
        }
    }
    if matches.is_empty() {
        crate::log_warn!(
            "{pane_id}: no {} process found; it will restore with a plain resume",
            entry.program
        );
        return Ok(None);
    }
    let tails: Vec<HashSet<&str>> = matches
        .iter()
        .map(|argv| argv[1..].iter().map(String::as_str).collect())
        .collect();
    for (argv, tail) in matches.iter().zip(tails.iter()) {
        if tails.iter().all(|other| tail.is_subset(other)) {
            return Ok(Some(argv.clone()));
        }
    }
    crate::log_warn!(
        "{pane_id}: {} {} processes found but none looks like the outermost one; it will restore with a plain resume",
        matches.len(),
        entry.program
    );
    Ok(None)
}

fn workspace_label(
    client: &mut dyn crate::Herdr,
    workspace_id: &str,
) -> Result<Option<String>, HerdrError> {
    let workspaces = client
        .call("workspace.list", Value::Object(Map::new()))?
        .get("workspaces")
        .cloned()
        .unwrap_or(Value::Null);
    for ws in workspaces.as_array().cloned().unwrap_or_default() {
        if ws.get("workspace_id").and_then(Value::as_str) == Some(workspace_id) {
            return Ok(ws.get("label").and_then(Value::as_str).map(str::to_string));
        }
    }
    Ok(None)
}

fn pane_ids(node: &Value) -> HashSet<String> {
    let empty = HashSet::new();
    let Some(o) = node.as_object() else {
        return empty;
    };
    if o.get("type").and_then(Value::as_str) == Some("split") {
        let mut out = pane_ids(o.get("first").unwrap_or(&Value::Null));
        out.extend(pane_ids(o.get("second").unwrap_or(&Value::Null)));
        return out;
    }
    o.get("pane_id")
        .and_then(Value::as_str)
        .map(|s| HashSet::from([s.to_string()]))
        .unwrap_or_default()
}

/// (pane count, max depth), root at depth 1, like herdr's layout.apply.
fn layout_stats(node: &Value, depth: usize) -> (usize, usize) {
    let Some(o) = node.as_object() else {
        return (1, depth);
    };
    if o.get("type").and_then(Value::as_str) == Some("split") {
        let (p1, d1) = layout_stats(o.get("first").unwrap_or(&Value::Null), depth + 1);
        let (p2, d2) = layout_stats(o.get("second").unwrap_or(&Value::Null), depth + 1);
        return (p1 + p2, d1.max(d2));
    }
    (1, depth)
}

pub fn pane_terminals(panes: &[Map<String, Value>]) -> HashSet<String> {
    panes
        .iter()
        .filter_map(|p| {
            p.get("terminal_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn tab_terminals(
    client: &mut dyn crate::Herdr,
    tab_id: &str,
) -> Result<HashSet<String>, HerdrError> {
    let panes = client
        .call("pane.list", Value::Object(Map::new()))?
        .get("panes")
        .cloned()
        .unwrap_or(Value::Null);
    Ok(panes
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|p| p.get("tab_id").and_then(Value::as_str) == Some(tab_id))
        .filter_map(|p| {
            p.get("terminal_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect())
}

fn tab_with_terminals_exists(
    client: &mut dyn crate::Herdr,
    terminals: &HashSet<String>,
) -> Result<bool, HerdrError> {
    if terminals.is_empty() {
        return Ok(false);
    }
    let panes = client
        .call("pane.list", Value::Object(Map::new()))?
        .get("panes")
        .cloned()
        .unwrap_or(Value::Null);
    let mut by_tab: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for p in panes.as_array().cloned().unwrap_or_default() {
        if let Some(tid) = p.get("terminal_id").and_then(Value::as_str) {
            by_tab
                .entry(
                    p.get("tab_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
                .or_default()
                .insert(tid.to_string());
        }
    }
    Ok(by_tab.values().any(|v| v == terminals))
}

#[derive(Debug)]
pub enum CaptureError {
    Skip(Skip),
    Herdr(HerdrError),
    Io(std::io::Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Skip(e) => write!(f, "{e}"),
            CaptureError::Herdr(e) => write!(f, "{e}"),
            CaptureError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<HerdrError> for CaptureError {
    fn from(e: HerdrError) -> Self {
        CaptureError::Herdr(e)
    }
}

impl From<std::io::Error> for CaptureError {
    fn from(e: std::io::Error) -> Self {
        CaptureError::Io(e)
    }
}

impl From<Skip> for CaptureError {
    fn from(e: Skip) -> Self {
        CaptureError::Skip(e)
    }
}

/// Build the archive record for a tab. Returns (record, session_files).
/// `overrides`: pane_id -> user-confirmed session value. `provenance`:
/// pane_id -> resolved_by marker for overrides ("scanner:<kind>",
/// "detect:ambiguous", "manual-paste", or a silent "detect:*" hit;
/// defaults to "manual-paste" when absent).
/// `archive_name`: user-given archive name, stored as the additive `name`
/// field; None omits it (sweeps, old records).
#[allow(clippy::too_many_arguments)]
pub fn capture(
    client: &mut dyn crate::Herdr,
    tab: &Map<String, Value>,
    panes: &[Map<String, Value>],
    table: &BTreeMap<String, agents::Entry>,
    activity_of: &ActivityOf<'_>,
    keep_transcripts: bool,
    now: Ts,
    herdr_session: &str,
    overrides: Option<&BTreeMap<String, String>>,
    provenance: Option<&BTreeMap<String, String>>,
    archive_name: Option<&str>,
) -> Result<(Value, Vec<(PathBuf, String)>), CaptureError> {
    let tab_id = tab.get("tab_id").and_then(Value::as_str).unwrap_or("");
    let layout = client
        .call("layout.export", serde_json::json!({"tab_id": tab_id}))?
        .get("layout")
        .cloned()
        .unwrap_or(Value::Null);
    let root = layout.get("root").cloned().unwrap_or(Value::Null);
    if layout.as_object().is_none_or(|o| o.is_empty()) || root.is_null() {
        return Err(Skip("layout.export returned no layout".to_string()).into());
    }
    let gathered: HashSet<String> = panes
        .iter()
        .filter_map(|p| p.get("pane_id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if pane_ids(&root) != gathered {
        return Err(Skip("layout does not match the tab's panes".to_string()).into());
    }
    let (pane_count, max_depth) = layout_stats(&root, 1);
    if pane_count > MAX_LAYOUT_PANES {
        return Err(Skip(format!(
            "layout has {pane_count} panes; herdr's limit is {MAX_LAYOUT_PANES}"
        ))
        .into());
    }
    if max_depth > MAX_LAYOUT_DEPTH {
        return Err(Skip(format!(
            "layout depth is {max_depth}; herdr's limit is {MAX_LAYOUT_DEPTH}"
        ))
        .into());
    }
    let workspace_id = tab
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let workspace_label = workspace_label(client, workspace_id)?;
    let mut pane_meta = Map::new();
    let mut session_files: Vec<(PathBuf, String)> = Vec::new();
    let mut copies: Vec<String> = Vec::new();
    for pane in panes {
        let mut meta = Map::new();
        meta.insert(
            "cwd".to_string(),
            pane.get("cwd").cloned().unwrap_or(Value::Null),
        );
        let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("");
        let pane_agent = pane.get("agent").and_then(Value::as_str).unwrap_or("");
        let mut session = pane.get("agent_session").cloned().unwrap_or(Value::Null);
        let mut resolved_by: Option<String> = None;
        if let Some(ov) = overrides.and_then(|o| o.get(pane_id)) {
            if !pane_agent.is_empty() {
                if !agents::valid_session_value(pane_agent, ov) {
                    return Err(Skip(format!("{pane_id}: invalid manual session id")).into());
                }
                session = serde_json::json!({"agent": pane_agent, "kind": "id", "value": ov, "source": "manual"});
                resolved_by = Some(
                    provenance
                        .and_then(|p| p.get(pane_id))
                        .cloned()
                        .unwrap_or_else(|| "manual-paste".to_string()),
                );
            }
        }
        let usable = !pane_agent.is_empty()
            && session.as_object().is_some_and(|s| {
                s.get("value")
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.is_empty())
            })
            && session
                .get("agent")
                .and_then(Value::as_str)
                .is_some_and(|a| table.contains_key(a));
        if usable {
            let agent = session
                .get("agent")
                .and_then(Value::as_str)
                .unwrap()
                .to_string();
            let value = session
                .get("value")
                .and_then(Value::as_str)
                .unwrap()
                .to_string();
            let last = activity_of(
                &agent,
                &value,
                pane.get("terminal_id").and_then(Value::as_str),
            );
            let argv = launch_argv(client, pane_id, &table[&agent])?;
            meta.insert("agent".to_string(), Value::String(agent.clone()));
            meta.insert(
                "session".to_string(),
                serde_json::json!({
                    "kind": session.get("kind").cloned().unwrap_or(Value::Null),
                    "value": value,
                    "source": session.get("source").cloned().unwrap_or(Value::Null),
                }),
            );
            meta.insert(
                "launch_argv".to_string(),
                argv.map(|a| Value::Array(a.into_iter().map(Value::String).collect()))
                    .unwrap_or(Value::Null),
            );
            meta.insert(
                "last_activity".to_string(),
                last.map(iso).map(Value::String).unwrap_or(Value::Null),
            );
            meta.insert(
                "resolved_by".to_string(),
                Value::String(resolved_by.unwrap_or_else(|| "herdr".to_string())),
            );
            if keep_transcripts && agent == "claude" {
                let home = history::claude_home();
                for src in history::claude_session_paths(&value) {
                    if let Ok(rel) = src.strip_prefix(&home) {
                        let rel = rel.to_string_lossy().replace('\\', "/");
                        session_files.push((src, rel.clone()));
                        copies.push(rel);
                    }
                }
            }
        } else {
            // Recorded as a plain shell (no agent, or no usable session).
            meta.insert(
                "resolved_by".to_string(),
                Value::String("shell".to_string()),
            );
        }
        pane_meta.insert(pane_id.to_string(), Value::Object(meta));
    }
    let first_cwd = pane_meta
        .values()
        .filter_map(|m| m.get("cwd").and_then(Value::as_str))
        .next()
        .map(str::to_string);
    let mut label = tab.get("label").and_then(Value::as_str).map(str::to_string);
    if label
        .as_deref()
        .is_some_and(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
    {
        label = None; // herdr's default label is just the tab number
    }
    // Python's str.isdigit is broader (unicode digits); herdr labels are
    // ASCII tab numbers, so ASCII-only is equivalent in practice.
    // `workspace_id` is additive (Python ignores it): restore prefers the
    // same live workspace id, since labels need not be unique.
    let mut record = serde_json::json!({
        "version": 1,
        "id": new_id(now),
        "archived_at": iso(now),
        "workspace": {"label": workspace_label, "cwd": first_cwd, "workspace_id": workspace_id},
        "tab": {"label": label},
        "layout": {"root": root,
                   "focused_pane_id": layout.get("focused_pane_id").cloned().unwrap_or(Value::Null),
                   "zoomed": layout.get("zoomed").and_then(Value::as_bool).unwrap_or(false)},
        "panes": pane_meta,
        "session_copies": copies,
        "herdr_session": herdr_session,
        "tool": format!("herdr-archive {}", env!("CARGO_PKG_VERSION")),
    });
    if let (Some(name), Some(obj)) = (archive_name, record.as_object_mut()) {
        obj.insert("name".to_string(), Value::String(name.to_string()));
    }
    Ok((record, session_files))
}

/// Write the record, verify the tab is unchanged, then close it. Returns
/// the archive id.
#[allow(clippy::too_many_arguments)]
pub fn archive_tab(
    client: &mut dyn crate::Herdr,
    arch: &Archive,
    tab: &Map<String, Value>,
    panes: &[Map<String, Value>],
    table: &BTreeMap<String, agents::Entry>,
    activity_of: &ActivityOf<'_>,
    keep_transcripts: bool,
    now: Ts,
    herdr_session: &str,
    overrides: Option<&BTreeMap<String, String>>,
    provenance: Option<&BTreeMap<String, String>>,
    archive_name: Option<&str>,
) -> Result<String, CaptureError> {
    let tab_id = tab.get("tab_id").and_then(Value::as_str).unwrap_or("");
    let (record, session_files) = capture(
        client,
        tab,
        panes,
        table,
        activity_of,
        keep_transcripts,
        now,
        herdr_session,
        overrides,
        provenance,
        archive_name,
    )?;
    let archive_id = record
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    arch.save(&record, &session_files)?;
    let expected_terminals = pane_terminals(panes);
    let terminals_now = match tab_terminals(client, tab_id) {
        Ok(t) => t,
        Err(e) => {
            let _ = arch.delete(&archive_id);
            return Err(CaptureError::Herdr(e));
        }
    };
    // Python catches BaseException around the re-read; a non-Herdr panic
    // cannot be caught meaningfully here, and tab_terminals only fails
    // with HerdrError, so this is equivalent.
    if terminals_now != expected_terminals {
        let _ = arch.delete(&archive_id);
        return Err(Skip("tab changed before it could be closed".to_string()).into());
    }
    match client.call("tab.close", serde_json::json!({"tab_id": tab_id})) {
        Ok(_) => Ok(archive_id),
        Err(e) => {
            if e.definite {
                let _ = arch.delete(&archive_id);
            } else {
                // Outcome unknown: ask pane.list whether the tab is still
                // there before deciding.
                if tab_with_terminals_exists(client, &expected_terminals).unwrap_or(false) {
                    let _ = arch.delete(&archive_id);
                }
            }
            if e.code == "confirmation_required" {
                return Err(Skip("closing it would close a worktree group".to_string()).into());
            }
            Err(CaptureError::Herdr(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_validation() {
        assert!(valid_archive_id("20260101T000000Z-abcdef"));
        assert!(!valid_archive_id("../x"));
        assert!(!valid_archive_id("20260101T000000Z-ABCDEF")); // uppercase hex rejected like Python
        assert!(!valid_archive_id("20260101T000000Z-abcde"));
        assert!(!valid_archive_id("20260101T000000Z-abcdef\n"));
        assert!(!valid_archive_id(""));
    }

    #[test]
    fn new_id_shape() {
        let id = new_id(crate::util::parse_iso(Some("2026-01-02T03:04:05Z")).unwrap());
        assert!(valid_archive_id(&id), "{id}");
        assert!(id.starts_with("20260102T030405Z-"), "{id}");
    }

    #[test]
    fn layout_stats_counts_like_herdr() {
        let leaf = serde_json::json!({"type": "pane", "pane_id": "p1"});
        assert_eq!(layout_stats(&leaf, 1), (1, 1));
        let split = serde_json::json!({"type": "split", "direction": "h", "ratio": 0.5,
            "first": leaf, "second": {"type": "pane", "pane_id": "p2"}});
        assert_eq!(layout_stats(&split, 1), (2, 2));
    }
}
