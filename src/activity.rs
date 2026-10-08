//! Per-session activity: what the tracker records, and effective activity.
//!
//! Port of `shelf/activity.py`.

use crate::util::{FileLock, LockError, Ts, atomic_write_json, iso, parse_iso, read_json};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const TOUCH_SKIP_MICROS: i64 = 60 * 1_000_000;
pub const AGENT_SESSION_RETRY_ATTEMPTS: u32 = 10;
pub const AGENT_SESSION_RETRY_DELAY_MS: u64 = 500;
pub const TERMINALS_KEY: &str = "terminals";

pub fn is_active_status(status: &str) -> bool {
    matches!(status, "working" | "blocked" | "done")
}

pub fn session_key(agent: &str, value: &str) -> String {
    format!("{agent}:{value}")
}

pub struct ActivityStore {
    pub path: PathBuf,
    pub lock_path: PathBuf,
}

impl ActivityStore {
    pub fn new(state_dir: &Path) -> Self {
        ActivityStore {
            path: state_dir.join("activity.json"),
            lock_path: state_dir.join("activity.lock"),
        }
    }

    /// Missing/corrupt files read as empty; other I/O errors propagate.
    pub fn load(&self) -> Result<Map<String, Value>, std::io::Error> {
        match read_json(&self.path, Value::Object(Map::new())) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => Ok(Map::new()),
            Err(e) => Err(e),
        }
    }

    /// Apply `f(data)` under the lock; write unless `f` returns false.
    /// Returns whether the write happened.
    pub fn update(
        &self,
        f: impl FnOnce(&mut Map<String, Value>) -> bool,
    ) -> Result<bool, StoreError> {
        let _guard = FileLock::new(&self.lock_path, 5.0).lock()?;
        let mut data = self.load()?;
        let changed = f(&mut data);
        if changed {
            atomic_write_json(&self.path, &Value::Object(data))?;
        }
        Ok(changed)
    }
}

#[derive(Debug)]
pub enum StoreError {
    Lock(LockError),
    Io(std::io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Lock(e) => write!(f, "{e}"),
            StoreError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<LockError> for StoreError {
    fn from(e: LockError) -> Self {
        StoreError::Lock(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

fn get_rec<'a>(data: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !data.get(key).is_some_and(Value::is_object) {
        data.insert(key.to_string(), Value::Object(Map::new()));
    }
    data.get_mut(key).unwrap().as_object_mut().unwrap()
}

/// Record activity now. Skips the write when the last record is under 60 s
/// old. A last_active in the future (clock skew) is always overwritten.
pub fn touch(data: &mut Map<String, Value>, key: &str, now: Ts) -> bool {
    let rec = get_rec(data, key);
    let mut changed = !rec.contains_key("first_seen");
    rec.entry("first_seen".to_string())
        .or_insert_with(|| Value::String(iso(now)));
    let last = rec
        .get("last_active")
        .and_then(Value::as_str)
        .and_then(|s| parse_iso(Some(s)));
    if last.is_none_or(|l| l > now || now.0 - l.0 >= TOUCH_SKIP_MICROS) {
        rec.insert("last_active".to_string(), Value::String(iso(now)));
        changed = true;
    }
    changed
}

/// Record activity for one pane.agent_status_changed event. "working" always
/// counts (subject to touch's own 60 s skip); "blocked"/"done" count only
/// on a status change.
pub fn record_status(data: &mut Map<String, Value>, key: &str, status: &str, now: Ts) -> bool {
    let prev = get_rec(data, key)
        .get("last_status")
        .and_then(Value::as_str)
        .map(str::to_string);
    let status_changed = prev.as_deref() != Some(status);
    if status != "working" && !status_changed {
        return false;
    }
    let touched = touch(data, key, now);
    get_rec(data, key).insert("last_status".to_string(), Value::String(status.to_string()));
    touched || status_changed
}

pub fn see(data: &mut Map<String, Value>, key: &str, now: Ts) -> bool {
    let rec = get_rec(data, key);
    if rec.contains_key("first_seen") {
        return false;
    }
    rec.insert("first_seen".to_string(), Value::String(iso(now)));
    true
}

pub fn mark_restored(data: &mut Map<String, Value>, key: &str, now: Ts) -> bool {
    let rec = get_rec(data, key);
    rec.entry("first_seen".to_string())
        .or_insert_with(|| Value::String(iso(now)));
    rec.insert("restored_at".to_string(), Value::String(iso(now)));
    true
}

/// Latest of last_active, restored_at, agent_started_at, and either history
/// or first_seen (see Python `effective` for the installed_at rule).
pub fn effective(
    rec: &Map<String, Value>,
    history_ts: Option<Ts>,
    installed_at: Option<Ts>,
) -> Option<Ts> {
    let mut candidates: Vec<Ts> = ["last_active", "restored_at", "agent_started_at"]
        .iter()
        .filter_map(|k| {
            rec.get(*k)
                .and_then(Value::as_str)
                .and_then(|s| parse_iso(Some(s)))
        })
        .collect();
    let first_seen = rec
        .get("first_seen")
        .and_then(Value::as_str)
        .and_then(|s| parse_iso(Some(s)));
    match history_ts {
        Some(h) => {
            candidates.push(h);
            if let (Some(f), Some(i)) = (first_seen, installed_at) {
                if f > i {
                    candidates.push(f);
                }
            }
        }
        None => {
            if let Some(f) = first_seen {
                candidates.push(f);
            }
        }
    }
    candidates.into_iter().max()
}

fn terminals_dict(data: &mut Map<String, Value>) -> &mut Map<String, Value> {
    if !data.get(TERMINALS_KEY).is_some_and(Value::is_object) {
        data.insert(TERMINALS_KEY.to_string(), Value::Object(Map::new()));
    }
    data.get_mut(TERMINALS_KEY)
        .unwrap()
        .as_object_mut()
        .unwrap()
}

/// Record that an agent (re)started in this pane's terminal.
pub fn record_terminal_started(data: &mut Map<String, Value>, terminal_id: &str, now: Ts) -> bool {
    let mut entry = Map::new();
    entry.insert("agent_started_at".to_string(), Value::String(iso(now)));
    terminals_dict(data).insert(terminal_id.to_string(), Value::Object(entry));
    true
}

fn mark_session_started(rec: &mut Map<String, Value>, now: Ts) -> bool {
    let existing = rec
        .get("agent_started_at")
        .and_then(Value::as_str)
        .and_then(|s| parse_iso(Some(s)));
    if existing.is_some_and(|e| e >= now) {
        return false;
    }
    rec.insert("agent_started_at".to_string(), Value::String(iso(now)));
    true
}

/// The pane's agent_session key, but only when that session's own agent
/// equals `agent`.
fn matching_session_key(pane: &Map<String, Value>, agent: &str) -> Option<String> {
    let session = pane.get("agent_session")?.as_object()?;
    if session.get("agent").and_then(Value::as_str) != Some(agent) {
        return None;
    }
    let value = session.get("value").and_then(Value::as_str)?;
    if value.is_empty() {
        return None;
    }
    Some(session_key(
        session.get("agent").and_then(Value::as_str).unwrap(),
        value,
    ))
}

#[derive(Debug)]
pub enum TrackError {
    Herdr(crate::api::HerdrError),
    Store(StoreError),
}

impl std::fmt::Display for TrackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrackError::Herdr(e) => write!(f, "{e}"),
            TrackError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TrackError {}

impl From<crate::api::HerdrError> for TrackError {
    fn from(e: crate::api::HerdrError) -> Self {
        TrackError::Herdr(e)
    }
}

impl From<StoreError> for TrackError {
    fn from(e: StoreError) -> Self {
        TrackError::Store(e)
    }
}

fn track_agent_detected(
    client: &mut dyn crate::Herdr,
    store: &ActivityStore,
    data: &Map<String, Value>,
    pane_id: &str,
    now: Ts,
    retry_delay_ms: u64,
) -> Result<bool, TrackError> {
    let agent = data.get("agent").and_then(Value::as_str).unwrap_or("");
    if agent.is_empty()
        || data
            .get("released")
            .is_some_and(|v| v.as_bool() == Some(true))
    {
        return Ok(false);
    }
    // Note: Python checks `data.get("released")` truthiness on any JSON
    // value; a release omits "agent", so the empty-agent check above already
    // covers real releases. Non-bool truthy "released" values do not occur
    // from herdr; treat only explicit true as released.
    let mut pane = client.call("pane.get", serde_json::json!({"pane_id": pane_id}))?;
    let mut terminal_id = pane
        .get("pane")
        .and_then(Value::as_object)
        .and_then(|p| p.get("terminal_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if terminal_id.as_deref().is_none_or(str::is_empty) {
        return Ok(false);
    }
    let mut key = pane
        .get("pane")
        .and_then(Value::as_object)
        .and_then(|p| matching_session_key(p, agent));
    for _ in 0..AGENT_SESSION_RETRY_ATTEMPTS {
        if key.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(retry_delay_ms));
        pane = client.call("pane.get", serde_json::json!({"pane_id": pane_id}))?;
        key = pane
            .get("pane")
            .and_then(Value::as_object)
            .and_then(|p| matching_session_key(p, agent));
    }
    let tid = terminal_id.take().unwrap();
    Ok(store.update(|d| {
        let mut changed = record_terminal_started(d, &tid, now);
        if let Some(k) = &key {
            changed = mark_session_started(get_rec(d, k), now) || changed;
        }
        changed
    })?)
}

fn is_agent_detected_event(
    env_event: Option<&str>,
    json_event: Option<&str>,
    data: &Map<String, Value>,
) -> bool {
    if env_event == Some("pane.agent_detected") {
        return true;
    }
    if matches!(
        json_event,
        Some("pane_agent_detected") | Some("pane.agent_detected")
    ) {
        return true;
    }
    data.get("type").and_then(Value::as_str) == Some("pane_agent_detected")
}

/// Handle one pane.agent_status_changed or pane.agent_detected event.
/// Returns whether activity.json actually changed.
pub fn track(
    client: &mut dyn crate::Herdr,
    store: &ActivityStore,
    event_json: Option<&str>,
    env_pane_id: Option<&str>,
    now: Ts,
    env_event: Option<&str>,
) -> Result<bool, TrackError> {
    track_with_retry(
        client,
        store,
        event_json,
        env_pane_id,
        now,
        env_event,
        AGENT_SESSION_RETRY_DELAY_MS,
    )
}

pub fn track_with_retry(
    client: &mut dyn crate::Herdr,
    store: &ActivityStore,
    event_json: Option<&str>,
    env_pane_id: Option<&str>,
    now: Ts,
    env_event: Option<&str>,
    retry_delay_ms: u64,
) -> Result<bool, TrackError> {
    let parsed: Value = serde_json::from_str(event_json.unwrap_or("{}")).unwrap_or(Value::Null);
    let top = match parsed.as_object() {
        Some(o) => o,
        None => return Ok(false),
    };
    let data = match top.get("data").and_then(Value::as_object) {
        Some(d) => d,
        None => return Ok(false),
    };
    let pane_id = data
        .get("pane_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| env_pane_id.map(str::to_string));
    let Some(pane_id) = pane_id.filter(|s| !s.is_empty()) else {
        return Ok(false);
    };
    if is_agent_detected_event(env_event, top.get("event").and_then(Value::as_str), data) {
        return track_agent_detected(client, store, data, &pane_id, now, retry_delay_ms);
    }
    let status = data
        .get("agent_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !is_active_status(status) {
        return Ok(false);
    }
    let pane = client.call("pane.get", serde_json::json!({"pane_id": pane_id}))?;
    let session = pane
        .get("pane")
        .and_then(|p| p.get("agent_session"))
        .and_then(Value::as_object);
    let (agent, value) = match session {
        Some(s) => (
            s.get("agent").and_then(Value::as_str).unwrap_or(""),
            s.get("value").and_then(Value::as_str).unwrap_or(""),
        ),
        None => ("", ""),
    };
    if agent.is_empty() || value.is_empty() {
        return Ok(false);
    }
    let key = session_key(agent, value);
    Ok(store.update(|d| record_status(d, &key, status, now))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(s: &str) -> Ts {
        parse_iso(Some(s)).unwrap()
    }

    #[test]
    fn touch_skip_and_future() {
        let now = ts("2026-01-01T00:01:00Z");
        let mut d = Map::new();
        assert!(touch(&mut d, "a:1", now));
        assert!(!touch(&mut d, "a:1", now)); // within 60 s
        let later = Ts(now.0 + 61 * 1_000_000);
        assert!(touch(&mut d, "a:1", later));
        // future last_active is always overwritten
        d.get_mut("a:1").unwrap().as_object_mut().unwrap().insert(
            "last_active".to_string(),
            Value::String("2027-01-01T00:00:00Z".to_string()),
        );
        assert!(touch(&mut d, "a:1", now));
        assert_eq!(
            d["a:1"]["last_active"],
            Value::String("2026-01-01T00:01:00Z".to_string())
        );
    }

    #[test]
    fn record_status_change_rules() {
        let now = ts("2026-01-01T00:01:00Z");
        let mut d = Map::new();
        assert!(record_status(&mut d, "a:1", "working", now));
        assert!(record_status(&mut d, "a:1", "done", now)); // changed status counts
        let mut d = Map::new();
        assert!(record_status(&mut d, "a:1", "done", now));
        assert!(!record_status(&mut d, "a:1", "done", now)); // same status, no touch
    }

    #[test]
    fn effective_matrix() {
        let installed = ts("2026-01-01T00:00:00Z");
        let mut rec = Map::new();
        rec.insert("first_seen".to_string(), json!("2026-01-01T00:00:00Z"));
        // no history: first_seen counts
        assert_eq!(effective(&rec, None, Some(installed)), Some(installed));
        // history present + first_seen == installed: history alone
        let hist = ts("2025-06-01T00:00:00Z");
        assert_eq!(effective(&rec, Some(hist), Some(installed)), Some(hist));
        // first_seen strictly after install counts alongside history
        rec.insert("first_seen".to_string(), json!("2026-02-01T00:00:00Z"));
        assert_eq!(
            effective(&rec, Some(hist), Some(installed)),
            parse_iso(Some("2026-02-01T00:00:00Z"))
        );
        // without installed_at, first_seen is not counted alongside history
        assert_eq!(effective(&rec, Some(hist), None), Some(hist));
        // restored_at / agent_started_at always count
        rec.insert("restored_at".to_string(), json!("2026-03-01T00:00:00Z"));
        assert_eq!(
            effective(&rec, Some(hist), None),
            parse_iso(Some("2026-03-01T00:00:00Z"))
        );
    }
}
