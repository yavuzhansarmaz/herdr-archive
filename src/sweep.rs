//! Decide which tabs to archive, and archive them.
//!
//! Port of `shelf/sweep.py`.

use crate::activity::{self, ActivityStore};
use crate::agents;
use crate::api::HerdrError;
use crate::archive::{self, Archive, Skip};
use crate::config::Config;
use crate::history;
use crate::scan::SCANNER_KINDS;
use crate::util::{FileLock, LockError, Ts, iso, parse_iso};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const ARCHIVE_NOW_LOCK_WAIT_SECONDS: f64 = 10.0;

pub use crate::archive::ActivityOf;

/// One tab with its panes, as gathered.
pub type TabPanes = (Map<String, Value>, Vec<Map<String, Value>>);
/// Borrowed view of one gathered tab.
pub type TabPanesRef<'a> = (&'a Map<String, Value>, &'a Vec<Map<String, Value>>);

/// None when the tab should be archived, otherwise the reason it should not.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    tab: &Map<String, Value>,
    panes: &[Map<String, Value>],
    table: &BTreeMap<String, agents::Entry>,
    activity_of: &ActivityOf<'_>,
    idle_micros: i64,
    now: Ts,
    open_in: Option<&HashMap<String, HashSet<String>>>,
) -> Option<String> {
    if tab.get("focused").and_then(Value::as_bool) == Some(true) {
        return Some("focused".to_string());
    }
    if panes
        .iter()
        .any(|p| p.get("agent_status").and_then(Value::as_str) == Some("working"))
    {
        return Some("working".to_string());
    }
    let agent_panes: Vec<&Map<String, Value>> = panes
        .iter()
        .filter(|p| {
            p.get("agent")
                .and_then(Value::as_str)
                .is_some_and(|a| !a.is_empty())
        })
        .collect();
    if agent_panes.is_empty() {
        return Some("no agent pane".to_string());
    }
    let mut seen_keys = HashSet::new();
    for p in agent_panes {
        let pane_id = p.get("pane_id").and_then(Value::as_str).unwrap_or("?");
        let agent = p.get("agent").and_then(Value::as_str).unwrap_or("");
        let session = p.get("agent_session").and_then(Value::as_object);
        let value = session
            .and_then(|s| s.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if session.is_none() || value.is_empty() {
            return Some(format!("{pane_id}: no session id"));
        }
        let session = session.unwrap();
        let sagent = session.get("agent").and_then(Value::as_str).unwrap_or("");
        if sagent != agent {
            return Some(format!("{pane_id}: agent does not match its session"));
        }
        if !table.contains_key(sagent) {
            return Some(format!(
                "{pane_id}: agent {sagent:?} is not in the agent table"
            ));
        }
        if !agents::valid_session_value(sagent, value) {
            return Some(format!("{pane_id}: invalid session id"));
        }
        let key = activity::session_key(sagent, value);
        if seen_keys.contains(&key) {
            return Some(format!(
                "{pane_id}: conversation {} is also open in another pane",
                crate::util::short8(value)
            ));
        }
        seen_keys.insert(key.clone());
        if let Some(open) = open_in {
            let tab_id = tab.get("tab_id").and_then(Value::as_str).unwrap_or("");
            let others: HashSet<&str> = open
                .get(&key)
                .map(|s| {
                    s.iter()
                        .map(String::as_str)
                        .filter(|t| *t != tab_id)
                        .collect()
                })
                .unwrap_or_default();
            if !others.is_empty() {
                return Some(format!(
                    "{pane_id}: conversation {} is also open in another tab",
                    crate::util::short8(value)
                ));
            }
        }
        let last = activity_of(sagent, value, p.get("terminal_id").and_then(Value::as_str));
        match last {
            None => return Some(format!("{pane_id}: activity unknown")),
            Some(l) if now.0 - l.0 < idle_micros => {
                let days = (now.0 - l.0).div_euclid(86_400 * 1_000_000);
                return Some(format!("{pane_id}: active {days}d ago"));
            }
            _ => {}
        }
    }
    None
}

fn activity_line(stamps: &[Ts], now: Ts) -> String {
    match stamps.iter().max() {
        None => "No activity recorded for this tab yet.".to_string(),
        Some(max) => {
            let days = (now.0 - max.0).div_euclid(86_400 * 1_000_000).max(0);
            if days == 0 {
                "Last activity today.".to_string()
            } else {
                format!(
                    "Last activity {days} day{} ago.",
                    if days == 1 { "" } else { "s" }
                )
            }
        }
    }
}

/// (blocks, warnings, activity_line) for archiving one tab on request.
pub fn assess(
    tab: &Map<String, Value>,
    panes: &[Map<String, Value>],
    table: &BTreeMap<String, agents::Entry>,
    activity_of: &ActivityOf<'_>,
    now: Ts,
    open_in: &HashMap<String, HashSet<String>>,
) -> (Vec<String>, Vec<String>, String) {
    let mut blocks = Vec::new();
    let mut warnings = Vec::new();
    let mut stamps = Vec::new();
    let mut seen = HashSet::new();
    if panes
        .iter()
        .any(|p| p.get("agent_status").and_then(Value::as_str) == Some("working"))
    {
        warnings.push("A pane is still working; archiving stops it.".to_string());
    }
    let agent_panes: Vec<&Map<String, Value>> = panes
        .iter()
        .filter(|p| {
            p.get("agent")
                .and_then(Value::as_str)
                .is_some_and(|a| !a.is_empty())
        })
        .collect();
    if agent_panes.is_empty() {
        warnings.push("No agent in this tab; it comes back as shells.".to_string());
    }
    for p in agent_panes {
        let pane_id = p.get("pane_id").and_then(Value::as_str).unwrap_or("?");
        let agent = p.get("agent").and_then(Value::as_str).unwrap_or("");
        let session = p.get("agent_session").and_then(Value::as_object);
        let (sagent, value) = match session {
            Some(s) => (
                s.get("agent").and_then(Value::as_str).unwrap_or(""),
                s.get("value").and_then(Value::as_str).unwrap_or(""),
            ),
            None => ("", ""),
        };
        if value.is_empty() {
            warnings.push(format!(
                "Pane {pane_id} has no session id; it comes back as a shell."
            ));
            continue;
        }
        if sagent != agent {
            blocks.push(format!("{pane_id}: agent does not match its session"));
            continue;
        }
        if !table.contains_key(sagent) {
            warnings.push(format!(
                "Pane {pane_id} runs {sagent}, which herdr-archive cannot resume; it comes back as a shell."
            ));
            continue;
        }
        if !agents::valid_session_value(sagent, value) {
            blocks.push(format!("{pane_id}: invalid session id"));
            continue;
        }
        let key = activity::session_key(sagent, value);
        if seen.contains(&key) {
            warnings.push(format!(
                "Conversation {} is open in two panes here; both come back resuming it.",
                crate::util::short8(value)
            ));
        } else {
            let tab_id = tab.get("tab_id").and_then(Value::as_str).unwrap_or("");
            let elsewhere = open_in
                .get(&key)
                .is_some_and(|s| s.iter().any(|t| t != tab_id));
            if elsewhere {
                warnings.push(format!(
                    "Conversation {} is also open in another tab; restore waits until that copy is closed.",
                    crate::util::short8(value)
                ));
            }
        }
        seen.insert(key);
        if let Some(last) = activity_of(sagent, value, p.get("terminal_id").and_then(Value::as_str))
        {
            stamps.push(last);
        }
    }
    // Dedup preserving order.
    let mut deduped = Vec::new();
    for w in warnings {
        if !deduped.contains(&w) {
            deduped.push(w);
        }
    }
    // No stamps + never-reporting agents means activity is unknowable,
    // not merely unrecorded — say so explicitly instead of the generic line.
    let line = if stamps.is_empty() {
        let mut kinds: Vec<&str> = panes
            .iter()
            .filter_map(|p| p.get("agent").and_then(Value::as_str))
            .filter(|a| SCANNER_KINDS.contains(a))
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        if kinds.is_empty() {
            activity_line(&stamps, now)
        } else {
            let verb = if kinds.len() == 1 { "doesn't" } else { "don't" };
            format!(
                "Activity unknown — {} {} report sessions to herdr.",
                kinds.join(", "),
                verb
            )
        }
    } else {
        activity_line(&stamps, now)
    };
    (blocks, deduped, line)
}

/// Session key -> set of tab_ids carrying it, across the whole sweep.
pub fn open_sessions(tabs: &[TabPanes]) -> HashMap<String, HashSet<String>> {
    let mut result: HashMap<String, HashSet<String>> = HashMap::new();
    for (tab, panes) in tabs {
        for p in panes {
            let agent = p.get("agent").and_then(Value::as_str).unwrap_or("");
            if agent.is_empty() {
                continue;
            }
            let session = p.get("agent_session").and_then(Value::as_object);
            let value = session
                .and_then(|s| s.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if session.is_none() || value.is_empty() {
                continue;
            }
            let sagent = session
                .unwrap()
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("");
            let key = activity::session_key(sagent, value);
            result.entry(key).or_default().insert(
                tab.get("tab_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    result
}

/// [(tab, [panes in that tab])] from one tab.list and one pane.list.
pub fn gather(client: &mut dyn crate::Herdr) -> Result<Vec<TabPanes>, HerdrError> {
    let tabs = client
        .call("tab.list", Value::Object(Map::new()))?
        .get("tabs")
        .cloned()
        .unwrap_or(Value::Null);
    let panes = client
        .call("pane.list", Value::Object(Map::new()))?
        .get("panes")
        .cloned()
        .unwrap_or(Value::Null);
    let mut by_tab: HashMap<String, Vec<Map<String, Value>>> = HashMap::new();
    for p in panes.as_array().cloned().unwrap_or_default() {
        if let Some(o) = p.as_object() {
            by_tab
                .entry(
                    o.get("tab_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
                .or_default()
                .push(o.clone());
        }
    }
    let mut out = Vec::new();
    for t in tabs.as_array().cloned().unwrap_or_default() {
        if let Some(o) = t.as_object() {
            let tab_id = o.get("tab_id").and_then(Value::as_str).unwrap_or("");
            out.push((o.clone(), by_tab.remove(tab_id).unwrap_or_default()));
        }
    }
    Ok(out)
}

fn find<'a>(tabs: &'a [TabPanes], terminals: &HashSet<String>) -> Option<TabPanesRef<'a>> {
    if terminals.is_empty() {
        return None;
    }
    tabs.iter()
        .find(|(_, panes)| archive::pane_terminals(panes) == *terminals)
        .map(|(t, p)| (t, p))
}

/// first_seen for every agent session; active-now only for working panes.
/// Also copies terminal-recorded agent_started_at onto session records and
/// prunes stale terminal entries (see Python `_record_presence`).
pub fn record_presence(data: &mut Map<String, Value>, tabs: &[TabPanes], now: Ts) -> bool {
    let mut changed = false;
    match data.get(activity::TERMINALS_KEY) {
        None => {
            // Transient empty: nothing to copy or prune, and nothing stored.
        }
        Some(Value::Object(_)) => {}
        _ => {
            data.insert(
                activity::TERMINALS_KEY.to_string(),
                Value::Object(Map::new()),
            );
            changed = true;
        }
    }
    let current_terminals: HashSet<String> = tabs
        .iter()
        .flat_map(|(_, panes)| panes.iter())
        .filter_map(|p| p.get("terminal_id").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    // Copy terminal agent_started_at onto session records (max wins).
    let copies: Vec<(String, String)> = {
        let terminals = data.get(activity::TERMINALS_KEY).and_then(Value::as_object);
        let mut out = Vec::new();
        for (_, panes) in tabs {
            for p in panes {
                let tid = p.get("terminal_id").and_then(Value::as_str).unwrap_or("");
                if tid.is_empty() {
                    continue;
                }
                let entry = terminals
                    .and_then(|t| t.get(tid))
                    .and_then(Value::as_object);
                let Some(entry) = entry else {
                    continue;
                };
                let started = entry
                    .get("agent_started_at")
                    .and_then(Value::as_str)
                    .and_then(|s| parse_iso(Some(s)));
                let session = p.get("agent_session").and_then(Value::as_object);
                let valid = session.is_some_and(|s| {
                    s.get("agent")
                        .and_then(Value::as_str)
                        .is_some_and(|a| !a.is_empty())
                        && s.get("value")
                            .and_then(Value::as_str)
                            .is_some_and(|v| !v.is_empty())
                });
                if started.is_none() || !valid {
                    continue;
                }
                let session = session.unwrap();
                let key = activity::session_key(
                    session.get("agent").and_then(Value::as_str).unwrap(),
                    session.get("value").and_then(Value::as_str).unwrap(),
                );
                out.push((
                    key,
                    entry
                        .get("agent_started_at")
                        .and_then(Value::as_str)
                        .unwrap()
                        .to_string(),
                ));
            }
        }
        out
    };
    for (key, started_str) in copies {
        if !data.get(&key).is_some_and(Value::is_object) {
            data.insert(key.clone(), Value::Object(Map::new()));
        }
        let rec = data.get_mut(&key).unwrap().as_object_mut().unwrap();
        let existing = rec
            .get("agent_started_at")
            .and_then(Value::as_str)
            .and_then(|s| parse_iso(Some(s)));
        let started = parse_iso(Some(started_str.as_str())).unwrap();
        if existing.is_none_or(|e| started > e) {
            rec.insert("agent_started_at".to_string(), Value::String(started_str));
            changed = true;
        }
    }

    // Prune terminals not in this gather (except concurrent-track entries).
    let prune: Vec<String> = match data.get(activity::TERMINALS_KEY).and_then(Value::as_object) {
        Some(terminals) => terminals
            .keys()
            .filter(|t| !current_terminals.contains(*t))
            .filter(|t| {
                let started = terminals
                    .get(*t)
                    .and_then(Value::as_object)
                    .and_then(|e| e.get("agent_started_at"))
                    .and_then(Value::as_str)
                    .and_then(|s| parse_iso(Some(s)));
                // Keep entries started at/after now (concurrent track race).
                started.is_none_or(|s| s < now)
            })
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    if !prune.is_empty() {
        if let Some(terminals) = data
            .get_mut(activity::TERMINALS_KEY)
            .and_then(Value::as_object_mut)
        {
            for t in prune {
                terminals.remove(&t);
                changed = true;
            }
        }
    }

    for (_, panes) in tabs {
        for p in panes {
            let agent = p.get("agent").and_then(Value::as_str).unwrap_or("");
            if agent.is_empty() {
                continue;
            }
            let session = p.get("agent_session").and_then(Value::as_object);
            let value = session
                .and_then(|s| s.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if session.is_none() || value.is_empty() {
                continue;
            }
            let session = session.unwrap();
            let key = activity::session_key(
                session.get("agent").and_then(Value::as_str).unwrap_or(""),
                value,
            );
            if p.get("agent_status").and_then(Value::as_str) == Some("working") {
                let touched = activity::touch(data, &key, now);
                let rec = data.get_mut(&key).unwrap().as_object_mut().unwrap();
                let mut t = touched;
                if rec.get("last_status").and_then(Value::as_str) != Some("working") {
                    rec.insert(
                        "last_status".to_string(),
                        Value::String("working".to_string()),
                    );
                    t = true;
                }
                changed = t || changed;
            } else {
                changed = activity::see(data, &key, now) || changed;
            }
        }
    }
    changed
}

/// Cached activity lookup: max(session effective, terminal started).
pub struct ActivityLookup {
    records: Map<String, Value>,
    installed_at: Option<Ts>,
    cache: RefCell<HashMap<String, Option<Ts>>>,
}

impl ActivityLookup {
    pub fn new(records: Map<String, Value>, installed_at: Option<Ts>) -> Self {
        ActivityLookup {
            records,
            installed_at,
            cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn activity_of(&self, agent: &str, value: &str, terminal_id: Option<&str>) -> Option<Ts> {
        let key = activity::session_key(agent, value);
        let session_activity = if let Some(cached) = self.cache.borrow().get(&key) {
            *cached
        } else {
            let rec = self
                .records
                .get(&key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let eff = activity::effective(
                &rec,
                history::last_activity(agent, value),
                self.installed_at,
            );
            self.cache.borrow_mut().insert(key.clone(), eff);
            eff
        };
        let mut started = None;
        if let Some(tid) = terminal_id.filter(|s| !s.is_empty()) {
            started = self
                .records
                .get(activity::TERMINALS_KEY)
                .and_then(Value::as_object)
                .and_then(|t| t.get(tid))
                .and_then(Value::as_object)
                .and_then(|e| e.get("agent_started_at"))
                .and_then(Value::as_str)
                .and_then(|s| parse_iso(Some(s)));
        }
        [session_activity, started].into_iter().flatten().max()
    }
}

/// Whether sweep_interval_minutes has passed since last_sweep.
pub fn is_due(state: &Path, interval_minutes: f64, now: Ts) -> std::io::Result<bool> {
    match std::fs::read_to_string(state.join("last_sweep")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
        Ok(text) => {
            let last = parse_iso(Some(text.trim()));
            Ok(last.is_none_or(|l| now.0 - l.0 >= (interval_minutes * 60.0 * 1_000_000.0) as i64))
        }
    }
}

/// The install time, recorded once on the first sweep of any kind.
pub fn installed_at(state: &Path, now: Ts) -> std::io::Result<Ts> {
    let path = state.join("installed_at");
    let ts = match std::fs::read_to_string(&path) {
        Ok(text) => parse_iso(Some(text.trim())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    match ts {
        Some(t) => Ok(t),
        None => {
            std::fs::create_dir_all(state)?;
            std::fs::write(&path, iso(now) + "\n")?;
            Ok(now)
        }
    }
}

fn workspace_labels(client: &mut dyn crate::Herdr) -> HashMap<String, String> {
    match client.call("workspace.list", Value::Object(Map::new())) {
        Ok(v) => v
            .get("workspaces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|ws| {
                Some((
                    ws.get("workspace_id").and_then(Value::as_str)?.to_string(),
                    ws.get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ))
            })
            .collect(),
        Err(e) => {
            crate::log_warn!(
                "workspace.list failed; tab labels will not include a workspace name: {e}"
            );
            HashMap::new()
        }
    }
}

fn display_label(tab: &Map<String, Value>, workspace_labels: &HashMap<String, String>) -> String {
    let label = tab.get("label").and_then(Value::as_str).unwrap_or("");
    let tab_id = tab.get("tab_id").and_then(Value::as_str).unwrap_or("");
    let fallback = if label.is_empty() { tab_id } else { label };
    if !label.is_empty() && !label.bytes().all(|b| b.is_ascii_digit()) {
        return label.to_string();
    }
    let ws_id = tab
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    match workspace_labels.get(ws_id).filter(|s| !s.is_empty()) {
        Some(ws) => format!("{ws}/{fallback}"),
        None => fallback.to_string(),
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub mode: String,
    pub eligible: Vec<String>,
    pub archived: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
}

fn plural(n: usize) -> String {
    if n == 1 {
        "1 tab".to_string()
    } else {
        format!("{n} tabs")
    }
}

pub fn summary(report: &Report) -> Option<String> {
    if report.mode != "live" {
        if report.eligible.is_empty() {
            return None;
        }
        return Some(format!(
            "herdr-archive (dry-run): would archive {}: {}",
            plural(report.eligible.len()),
            report.eligible.join(", ")
        ));
    }
    let mut parts = Vec::new();
    if !report.archived.is_empty() {
        parts.push(format!(
            "archived {}: {}",
            plural(report.archived.len()),
            report.archived.join(", ")
        ));
    }
    if !report.failed.is_empty() {
        parts.push(format!(
            "{} failed, see herdr plugin log",
            report.failed.len()
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("herdr-archive: {}", parts.join("; ")))
    }
}

fn notify(client: &mut dyn crate::Herdr, report: &Report) {
    let Some(text) = summary(report) else {
        return;
    };
    if let Err(e) = client.call(
        "notification.show",
        serde_json::json!({"title": "herdr-archive", "body": text}),
    ) {
        crate::log_warn!("notification failed: {e}");
    }
}

#[derive(Debug)]
pub enum SweepError {
    Herdr(HerdrError),
    Io(std::io::Error),
    Lock(LockError),
    Store(activity::StoreError),
    Skip(Skip),
}

impl std::fmt::Display for SweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SweepError::Herdr(e) => write!(f, "{e}"),
            SweepError::Io(e) => write!(f, "{e}"),
            SweepError::Lock(e) => write!(f, "{e}"),
            SweepError::Store(e) => write!(f, "{e}"),
            SweepError::Skip(e) => write!(f, "{e}"),
        }
    }
}

impl SweepError {
    /// Split into a Skip (expected refusal) vs anything else.
    pub fn into_skip(self) -> Result<Skip, SweepError> {
        match self {
            SweepError::Skip(s) => Ok(s),
            e => Err(e),
        }
    }
}

impl std::error::Error for SweepError {}

impl From<HerdrError> for SweepError {
    fn from(e: HerdrError) -> Self {
        SweepError::Herdr(e)
    }
}

impl From<std::io::Error> for SweepError {
    fn from(e: std::io::Error) -> Self {
        SweepError::Io(e)
    }
}

impl From<LockError> for SweepError {
    fn from(e: LockError) -> Self {
        SweepError::Lock(e)
    }
}

impl From<activity::StoreError> for SweepError {
    fn from(e: activity::StoreError) -> Self {
        SweepError::Store(e)
    }
}

impl From<archive::CaptureError> for SweepError {
    fn from(e: archive::CaptureError) -> Self {
        match e {
            archive::CaptureError::Herdr(h) => SweepError::Herdr(h),
            archive::CaptureError::Io(i) => SweepError::Io(i),
            archive::CaptureError::Skip(s) => SweepError::Skip(s),
        }
    }
}

impl From<Skip> for SweepError {
    fn from(e: Skip) -> Self {
        SweepError::Skip(e)
    }
}

/// One sweep. Returns the report, or None when not due or another sweep
/// holds the lock.
pub fn run(
    client: &mut dyn crate::Herdr,
    cfg: &Config,
    state_dir: &Path,
    table: &BTreeMap<String, agents::Entry>,
    if_due: bool,
    now: Ts,
    herdr_session: &str,
) -> Result<Option<Report>, SweepError> {
    if if_due && !is_due(state_dir, cfg.sweep_interval_minutes, now)? {
        return Ok(None);
    }
    let _guard = match FileLock::new(&state_dir.join("sweep.lock"), 0.0).lock() {
        Ok(g) => g,
        Err(LockError::Busy(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if if_due && !is_due(state_dir, cfg.sweep_interval_minutes, now)? {
        return Ok(None);
    }
    let mut report = Report {
        mode: cfg.mode.clone(),
        ..Report::default()
    };
    let mut gathered = false;
    let result = sweep_into(
        client,
        cfg,
        state_dir,
        table,
        now,
        &mut report,
        &mut gathered,
        herdr_session,
    );
    notify(client, &report);
    if gathered {
        std::fs::write(state_dir.join("last_sweep"), iso(now) + "\n")?;
    }
    result?;
    Ok(Some(report))
}

#[allow(clippy::too_many_arguments)]
fn sweep_into(
    client: &mut dyn crate::Herdr,
    cfg: &Config,
    state: &Path,
    table: &BTreeMap<String, agents::Entry>,
    now: Ts,
    report: &mut Report,
    gathered: &mut bool,
    herdr_session: &str,
) -> Result<(), SweepError> {
    let tabs = gather(client)?;
    *gathered = true;
    let installed = installed_at(state, now)?;
    let labels = workspace_labels(client);
    let store = ActivityStore::new(state);
    store.update(|d| record_presence(d, &tabs, now))?;
    let lookup = ActivityLookup::new(store.load()?, Some(installed));
    let activity_of = |a: &str, v: &str, t: Option<&str>| lookup.activity_of(a, v, t);
    let open_in = open_sessions(&tabs);
    let idle = (cfg.idle_days * 86_400.0 * 1_000_000.0) as i64;
    let mut targets: Vec<(String, HashSet<String>)> = Vec::new();
    for (tab, panes) in &tabs {
        let label = display_label(tab, &labels);
        match decide(tab, panes, table, &activity_of, idle, now, Some(&open_in)) {
            Some(reason) => {
                report.skipped.push((label.clone(), reason.clone()));
                crate::log_info!("skip {label}: {reason}");
            }
            None => {
                report.eligible.push(label.clone());
                targets.push((label, archive::pane_terminals(panes)));
            }
        }
    }
    if cfg.mode != "live" {
        for label in &report.eligible {
            crate::log_info!("dry-run: would archive {label}");
        }
        return Ok(());
    }
    let arch = Archive::new(state);
    for (label, terminals) in targets {
        let outcome: Result<String, archive::CaptureError> = (|| {
            let lookup = ActivityLookup::new(store.load()?, Some(installed));
            let activity_of = |a: &str, v: &str, t: Option<&str>| lookup.activity_of(a, v, t);
            let fresh = gather(client)?;
            let Some((tab, panes)) = find(&fresh, &terminals) else {
                report
                    .skipped
                    .push((label.clone(), "tab changed during the sweep".to_string()));
                crate::log_info!("skip {label}: tab changed during the sweep");
                return Ok(String::new());
            };
            let idle = (cfg.idle_days * 86_400.0 * 1_000_000.0) as i64;
            if let Some(reason) = decide(
                tab,
                panes,
                table,
                &activity_of,
                idle,
                now,
                Some(&open_sessions(&fresh)),
            ) {
                report.skipped.push((label.clone(), reason.clone()));
                crate::log_info!("skip {label}: {reason}");
                return Ok(String::new());
            }
            let id = archive::archive_tab(
                client,
                &arch,
                tab,
                panes,
                table,
                &activity_of,
                cfg.keep_transcripts,
                now,
                herdr_session,
                None,
                None,
                None,
            )?;
            Ok(id)
        })();
        match outcome {
            Ok(id) if id.is_empty() => {}
            Ok(id) => {
                report.archived.push(label.clone());
                crate::log_info!("archived {label} as {id}");
            }
            Err(archive::CaptureError::Skip(s)) => {
                report.skipped.push((label.clone(), s.0.clone()));
                crate::log_info!("skip {label}: {s}");
            }
            Err(e) => {
                report.failed.push((label.clone(), e.to_string()));
                crate::log_error!("failed to archive {label}: {e}");
            }
        }
    }
    Ok(())
}

fn warn_if_open_elsewhere(
    tab_id: &str,
    panes: &[Map<String, Value>],
    open_in: &HashMap<String, HashSet<String>>,
) {
    for p in panes {
        if p.get("agent")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            continue;
        }
        let session = p.get("agent_session").and_then(Value::as_object);
        let value = session
            .and_then(|s| s.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if session.is_none() || value.is_empty() {
            continue;
        }
        let key = activity::session_key(
            session
                .unwrap()
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or(""),
            value,
        );
        if open_in
            .get(&key)
            .is_some_and(|s| s.iter().any(|t| t != tab_id))
        {
            crate::log_warn!(
                "{tab_id}: conversation {} is also open in another tab",
                crate::util::short8(value)
            );
        }
    }
}

fn snapshot(
    client: &mut dyn crate::Herdr,
    state: &Path,
    now: Ts,
) -> Result<(Vec<TabPanes>, ActivityLookup), SweepError> {
    let tabs = gather(client)?;
    let installed = installed_at(state, now)?;
    let store = ActivityStore::new(state);
    store.update(|d| record_presence(d, &tabs, now))?;
    Ok((tabs, ActivityLookup::new(store.load()?, Some(installed))))
}

fn match_tab<'a>(
    tabs: &'a [TabPanes],
    tab_id: &str,
    terminals: Option<&HashSet<String>>,
) -> Result<TabPanesRef<'a>, Skip> {
    if let Some(t) = terminals {
        match find(tabs, t) {
            Some(found) => Ok(found),
            None => Err(Skip("tab changed".to_string())),
        }
    } else {
        tabs.iter()
            .find(|(t, _)| t.get("tab_id").and_then(Value::as_str) == Some(tab_id))
            .map(|(t, p)| (t, p))
            .ok_or_else(|| Skip(format!("no tab {tab_id}")))
    }
}

fn pane_identities(panes: &[Map<String, Value>]) -> HashSet<(String, String, String)> {
    panes
        .iter()
        .map(|p| {
            (
                p.get("pane_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                p.get("tab_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                p.get("terminal_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .collect()
}

pub struct Preview {
    pub tab_id: String,
    pub label: String,
    pub terminals: Vec<String>,
    pub blocks: Vec<String>,
    pub warnings: Vec<String>,
    pub activity: String,
}

/// What the archive-tab popup shows for one tab. Takes no sweep lock; a
/// pane.list read before gather() that matches gather()'s own proves
/// nothing shifted in between.
pub fn preview(
    client: &mut dyn crate::Herdr,
    state_dir: &Path,
    table: &BTreeMap<String, agents::Entry>,
    tab_id: &str,
    now: Ts,
) -> Result<Preview, SweepError> {
    let before: HashSet<(String, String, String)> = {
        let panes = client
            .call("pane.list", Value::Object(Map::new()))?
            .get("panes")
            .cloned()
            .unwrap_or(Value::Null);
        pane_identities(
            &panes
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|p| p.as_object().cloned())
                .collect::<Vec<_>>(),
        )
    };
    let (tabs, lookup) = snapshot(client, state_dir, now)?;
    let activity_of = |a: &str, v: &str, t: Option<&str>| lookup.activity_of(a, v, t);
    let after: HashSet<(String, String, String)> = pane_identities(
        &tabs
            .iter()
            .flat_map(|(_, panes)| panes.iter().cloned())
            .collect::<Vec<_>>(),
    );
    if after != before {
        return Err(SweepError::Skip(Skip(
            "tabs changed while being read; try again".to_string(),
        )));
    }
    let (tab, panes) = match_tab(&tabs, tab_id, None)?;
    let (blocks, warnings, activity) =
        assess(tab, panes, table, &activity_of, now, &open_sessions(&tabs));
    let mut terminals: Vec<String> = archive::pane_terminals(panes).into_iter().collect();
    terminals.sort();
    Ok(Preview {
        tab_id: tab
            .get("tab_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        label: display_label(tab, &workspace_labels(client)),
        terminals,
        blocks,
        warnings,
        activity,
    })
}

/// Archive one tab immediately, ignoring idle_days and mode.
/// `archive_name`: user-given name for the record (confirm flow); None
/// omits the `name` field.
#[allow(clippy::too_many_arguments)]
pub fn archive_now(
    client: &mut dyn crate::Herdr,
    cfg: &Config,
    state_dir: &Path,
    table: &BTreeMap<String, agents::Entry>,
    tab_id: &str,
    now: Ts,
    herdr_session: &str,
    terminals: Option<&HashSet<String>>,
    confirmed: bool,
    overrides: Option<&BTreeMap<String, String>>,
    provenance: Option<&BTreeMap<String, String>>,
    archive_name: Option<&str>,
) -> Result<String, SweepError> {
    let _guard =
        FileLock::new(&state_dir.join("sweep.lock"), ARCHIVE_NOW_LOCK_WAIT_SECONDS).lock()?;
    let (tabs, lookup) = snapshot(client, state_dir, now)?;
    let activity_of = |a: &str, v: &str, t: Option<&str>| lookup.activity_of(a, v, t);
    let (tab, panes) = match_tab(&tabs, tab_id, terminals)?;
    let open_in = open_sessions(&tabs);
    let reason = if confirmed {
        assess(tab, panes, table, &activity_of, now, &open_in)
            .0
            .into_iter()
            .next()
    } else {
        let mut unfocused = tab.clone();
        unfocused.insert("focused".to_string(), Value::Bool(false));
        decide(&unfocused, panes, table, &activity_of, 0, now, None)
    };
    if let Some(r) = reason {
        return Err(SweepError::Skip(Skip(r)));
    }
    warn_if_open_elsewhere(
        tab.get("tab_id").and_then(Value::as_str).unwrap_or(""),
        panes,
        &open_in,
    );
    let arch = Archive::new(state_dir);
    Ok(archive::archive_tab(
        client,
        &arch,
        tab,
        panes,
        table,
        &activity_of,
        cfg.keep_transcripts,
        now,
        herdr_session,
        overrides,
        provenance,
        archive_name,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: &str, focused: bool) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("tab_id".to_string(), Value::String(id.to_string()));
        m.insert("workspace_id".to_string(), Value::String("w1".to_string()));
        m.insert("focused".to_string(), Value::Bool(focused));
        m
    }

    fn pane(pid: &str, agent: &str, value: &str, status: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("pane_id".to_string(), Value::String(pid.to_string()));
        m.insert("tab_id".to_string(), Value::String("t1".to_string()));
        m.insert(
            "terminal_id".to_string(),
            Value::String(format!("term-{pid}")),
        );
        m.insert("agent".to_string(), Value::String(agent.to_string()));
        m.insert(
            "agent_status".to_string(),
            Value::String(status.to_string()),
        );
        if !value.is_empty() {
            let mut s = Map::new();
            s.insert("agent".to_string(), Value::String(agent.to_string()));
            s.insert("kind".to_string(), Value::String("id".to_string()));
            s.insert("value".to_string(), Value::String(value.to_string()));
            m.insert("agent_session".to_string(), Value::Object(s));
        }
        m
    }

    fn table() -> BTreeMap<String, agents::Entry> {
        agents::table(None)
    }

    fn now() -> Ts {
        parse_iso(Some("2026-02-01T00:00:00Z")).unwrap()
    }

    fn old_activity(_a: &str, _v: &str, _t: Option<&str>) -> Option<Ts> {
        parse_iso(Some("2026-01-01T00:00:00Z"))
    }

    const IDLE_7D: i64 = 7 * 86_400 * 1_000_000;

    #[test]
    fn decide_matrix() {
        let t = table();
        let n = now();
        // eligible
        let tab1 = tab("t1", false);
        assert_eq!(
            decide(
                &tab1,
                &[pane("p1", "claude", "v1", "idle")],
                &t,
                &old_activity,
                IDLE_7D,
                n,
                None
            ),
            None
        );
        // focused / working / no agent
        assert_eq!(
            decide(
                &tab("t1", true),
                &[pane("p1", "claude", "v1", "idle")],
                &t,
                &old_activity,
                IDLE_7D,
                n,
                None
            ),
            Some("focused".to_string())
        );
        assert_eq!(
            decide(
                &tab1,
                &[pane("p1", "claude", "v1", "working")],
                &t,
                &old_activity,
                IDLE_7D,
                n,
                None
            ),
            Some("working".to_string())
        );
        let mut shell = pane("p1", "", "", "idle");
        shell.remove("agent");
        assert_eq!(
            decide(&tab1, &[shell], &t, &old_activity, IDLE_7D, n, None),
            Some("no agent pane".to_string())
        );
        // missing session
        assert_eq!(
            decide(
                &tab1,
                &[pane("p1", "claude", "", "idle")],
                &t,
                &old_activity,
                IDLE_7D,
                n,
                None
            ),
            Some("p1: no session id".to_string())
        );
        // stale agent
        let mut stale = pane("p1", "claude", "v1", "idle");
        stale
            .get_mut("agent_session")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("agent".to_string(), Value::String("codex".to_string()));
        assert_eq!(
            decide(&tab1, &[stale], &t, &old_activity, IDLE_7D, n, None),
            Some("p1: agent does not match its session".to_string())
        );
        // unknown agent
        let mut unk = pane("p1", "weird", "v1", "idle");
        unk.get_mut("agent_session")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("agent".to_string(), Value::String("weird".to_string()));
        assert_eq!(
            decide(&tab1, &[unk], &t, &old_activity, IDLE_7D, n, None),
            Some("p1: agent \"weird\" is not in the agent table".to_string())
        );
        // invalid session
        assert_eq!(
            decide(
                &tab1,
                &[pane("p1", "claude", "-bad", "idle")],
                &t,
                &old_activity,
                IDLE_7D,
                n,
                None
            ),
            Some("p1: invalid session id".to_string())
        );
        // dup pane
        assert_eq!(
            decide(
                &tab1,
                &[
                    pane("p1", "claude", "v12345678", "idle"),
                    pane("p2", "claude", "v12345678", "idle")
                ],
                &t,
                &old_activity,
                IDLE_7D,
                n,
                None
            ),
            Some("p2: conversation v1234567 is also open in another pane".to_string())
        );
        // dup tab via open_in
        let mut open = HashMap::new();
        open.insert(
            "claude:v1".to_string(),
            HashSet::from(["t1".to_string(), "t2".to_string()]),
        );
        assert_eq!(
            decide(
                &tab1,
                &[pane("p1", "claude", "v1", "idle")],
                &t,
                &old_activity,
                IDLE_7D,
                n,
                Some(&open)
            ),
            Some("p1: conversation v1 is also open in another tab".to_string())
        );
        // activity unknown
        let none = |_: &str, _: &str, _: Option<&str>| None;
        assert_eq!(
            decide(
                &tab1,
                &[pane("p1", "claude", "v1", "idle")],
                &t,
                &none,
                IDLE_7D,
                n,
                None
            ),
            Some("p1: activity unknown".to_string())
        );
        // recent activity
        let fresh = |_: &str, _: &str, _: Option<&str>| parse_iso(Some("2026-01-31T00:00:00Z"));
        assert_eq!(
            decide(
                &tab1,
                &[pane("p1", "claude", "v1", "idle")],
                &t,
                &fresh,
                IDLE_7D,
                n,
                None
            ),
            Some("p1: active 1d ago".to_string())
        );
    }

    #[test]
    fn assess_blocks_warnings() {
        let t = table();
        let n = now();
        let tab1 = tab("t1", false);
        let open = HashMap::new();
        // working -> warning, missing session -> warning shell
        let (b, w, a) = assess(
            &tab1,
            &[pane("p1", "claude", "", "working")],
            &t,
            &old_activity,
            n,
            &open,
        );
        assert!(b.is_empty());
        assert_eq!(w.len(), 2);
        assert_eq!(a, "No activity recorded for this tab yet.");
        // stale -> block
        let mut stale = pane("p1", "claude", "v1", "idle");
        stale
            .get_mut("agent_session")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("agent".to_string(), Value::String("codex".to_string()));
        let (b, _, _) = assess(&tab1, &[stale], &t, &old_activity, n, &open);
        assert_eq!(b, vec!["p1: agent does not match its session".to_string()]);
        // invalid -> block
        let (b, _, _) = assess(
            &tab1,
            &[pane("p1", "claude", "-bad", "idle")],
            &t,
            &old_activity,
            n,
            &open,
        );
        assert_eq!(b, vec!["p1: invalid session id".to_string()]);
        // unknown agent -> warning
        let mut unk = pane("p1", "weird", "v1", "idle");
        unk.get_mut("agent_session")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("agent".to_string(), Value::String("weird".to_string()));
        let (b, w, _) = assess(&tab1, &[unk], &t, &old_activity, n, &open);
        assert!(b.is_empty());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("herdr-archive cannot resume"), "{w:?}");
        // activity line with stamps
        let (_, _, a) = assess(
            &tab1,
            &[pane("p1", "claude", "v1", "idle")],
            &t,
            &old_activity,
            n,
            &open,
        );
        assert_eq!(a, "Last activity 31 days ago.");
    }

    #[test]
    fn assess_activity_line_unknown_for_never_reporting_agents() {
        let t = table();
        let n = now();
        let tab1 = tab("t1", false);
        let open = HashMap::new();
        // muse pane, no session, no stamps -> explicit unknown line.
        let (_, _, a) = assess(
            &tab1,
            &[pane("p1", "muse", "", "idle")],
            &t,
            &old_activity,
            n,
            &open,
        );
        assert_eq!(
            a,
            "Activity unknown — muse doesn't report sessions to herdr."
        );
        // multiple never-reporting kinds are listed, plural verb.
        let (_, _, a) = assess(
            &tab1,
            &[
                pane("p1", "muse", "", "idle"),
                pane("p2", "gemini", "", "idle"),
            ],
            &t,
            &old_activity,
            n,
            &open,
        );
        assert_eq!(
            a,
            "Activity unknown — gemini, muse don't report sessions to herdr."
        );
        // shell-only tab keeps the generic line.
        let (_, _, a) = assess(
            &tab1,
            &[pane("p1", "", "", "idle")],
            &t,
            &old_activity,
            n,
            &open,
        );
        assert_eq!(a, "No activity recorded for this tab yet.");
    }

    #[test]
    fn summary_formats() {
        let dry = Report {
            mode: "dry-run".to_string(),
            eligible: vec!["a".into(), "b".into()],
            ..Report::default()
        };
        assert_eq!(
            summary(&dry).unwrap(),
            "herdr-archive (dry-run): would archive 2 tabs: a, b"
        );
        let empty = Report {
            mode: "dry-run".to_string(),
            ..Report::default()
        };
        assert_eq!(summary(&empty), None);
        let live = Report {
            mode: "live".to_string(),
            archived: vec!["a".into()],
            failed: vec![("b".into(), "x".into())],
            ..Report::default()
        };
        assert_eq!(
            summary(&live).unwrap(),
            "herdr-archive: archived 1 tab: a; 1 failed, see herdr plugin log"
        );
        let none = Report {
            mode: "live".to_string(),
            ..Report::default()
        };
        assert_eq!(summary(&none), None);
    }
}
