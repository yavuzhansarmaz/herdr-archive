//! Self-healing merge of pre-0.3.0 root-level state into sessions/default/.
//!
//! Port of `shelf/migrate.py`, scoped to the `herdr-archive` tree.

use crate::util::{FileLock, LockError, atomic_write_json, parse_iso, read_json};
use serde_json::{Map, Value};
use std::path::Path;

pub const LOCK_WAIT_SECONDS: f64 = 10.0;

pub fn is_archive_entry(entry: &Path) -> bool {
    crate::archive::valid_archive_id(entry.file_name().and_then(|n| n.to_str()).unwrap_or(""))
        && entry.is_dir()
}

/// Cheap, lock-free check: is there any pre-0.3.0 root-level state left?
pub fn root_legacy_present(root: &Path) -> bool {
    if root.join("activity.json").exists()
        || root.join("last_sweep").exists()
        || root.join("installed_at").exists()
    {
        return true;
    }
    let archive_dir = root.join("archive");
    if !archive_dir.is_dir() {
        return false;
    }
    match std::fs::read_dir(&archive_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .any(|e| is_archive_entry(&e.path())),
        Err(_) => false,
    }
}

#[derive(Debug)]
enum MergeError {
    Lock(LockError),
    Io(std::io::Error),
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Lock(e) => write!(f, "{e}"),
            MergeError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<LockError> for MergeError {
    fn from(e: LockError) -> Self {
        MergeError::Lock(e)
    }
}

impl From<std::io::Error> for MergeError {
    fn from(e: std::io::Error) -> Self {
        MergeError::Io(e)
    }
}

/// Merge root-level legacy state into sessions/default/, if any exists.
/// Never raises: lock contention or failure is logged for a later retry.
pub fn merge_into_default_session(root: &Path) {
    if !root_legacy_present(root) {
        return;
    }
    let result = (|| -> Result<(), MergeError> {
        let _m = FileLock::new(&root.join("migrate.lock"), LOCK_WAIT_SECONDS).lock()?;
        let _s = FileLock::new(&root.join("sweep.lock"), LOCK_WAIT_SECONDS).lock()?;
        let _a = FileLock::new(&root.join("activity.lock"), LOCK_WAIT_SECONDS).lock()?;
        merge(root)
    })();
    match result {
        Ok(()) => {}
        Err(MergeError::Lock(LockError::Busy(_))) => {
            crate::log_warn!(
                "could not migrate legacy state into sessions/default: a lock is held elsewhere; will retry"
            );
        }
        Err(MergeError::Lock(LockError::Io(e))) | Err(MergeError::Io(e)) => {
            crate::log_error!(
                "failed to migrate legacy state into sessions/default; will retry: {e}"
            );
        }
    }
}

fn merge(root: &Path) -> Result<(), MergeError> {
    let default_dir = root.join("sessions").join("default");
    std::fs::create_dir_all(&default_dir)?;
    let mut moved: Vec<&str> = Vec::new();
    match merge_archive(root, &default_dir) {
        Ok(true) => moved.push("archive"),
        Ok(false) => {}
        Err(e) => {
            crate::log_error!("failed to migrate archive into sessions/default: {e}");
        }
    }
    match merge_activity_json(root, &default_dir) {
        Ok(true) => moved.push("activity.json"),
        Ok(false) => {}
        Err(MergeError::Lock(e)) => return Err(MergeError::Lock(e)),
        Err(MergeError::Io(e)) => {
            crate::log_error!("failed to migrate activity.json into sessions/default: {e}");
        }
    }
    for (name, keep) in [("last_sweep", "destination"), ("installed_at", "earlier")] {
        match merge_marker(root, &default_dir, name, keep) {
            Ok(true) => moved.push(name),
            Ok(false) => {}
            Err(e) => {
                crate::log_error!("failed to migrate {name} into sessions/default: {e}");
            }
        }
    }
    if !moved.is_empty() {
        crate::log_info!(
            "migrated legacy state into sessions/default: {}",
            moved.join(", ")
        );
    }
    if root_legacy_present(root) {
        crate::log_error!(
            "some legacy state could not be migrated into sessions/default (see warnings above); will retry on the next command"
        );
    }
    Ok(())
}

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

use std::path::PathBuf;

fn files_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn archive_entries_identical(a: &Path, b: &Path) -> bool {
    let rel_a: Vec<PathBuf> = all_files(a)
        .iter()
        .filter_map(|p| p.strip_prefix(a).ok().map(Path::to_path_buf))
        .collect();
    let rel_b: Vec<PathBuf> = all_files(b)
        .iter()
        .filter_map(|p| p.strip_prefix(b).ok().map(Path::to_path_buf))
        .collect();
    if rel_a != rel_b {
        return false;
    }
    rel_a
        .iter()
        .all(|rel| files_equal(&a.join(rel), &b.join(rel)))
}

fn resolve_archive_collision(
    root: &Path,
    entry: &Path,
    dest: &Path,
) -> Result<bool, std::io::Error> {
    let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    if archive_entries_identical(entry, dest) {
        std::fs::remove_dir_all(entry)?;
        return Ok(true);
    }
    let conflict_dir = root.join("archive.conflict");
    let conflict_dest = conflict_dir.join(name);
    if conflict_dest.exists() {
        crate::log_warn!(
            "archive {name} already has a conflicting copy at archive.conflict/{name}; leaving the root copy in place"
        );
        return Ok(false);
    }
    std::fs::create_dir_all(&conflict_dir)?;
    std::fs::rename(entry, &conflict_dest)?;
    crate::log_warn!(
        "archive {name} exists at both the root and in sessions/default, and differs; moved the root copy to archive.conflict/{name}"
    );
    Ok(true)
}

fn merge_archive(root: &Path, default_dir: &Path) -> Result<bool, std::io::Error> {
    let src_root = root.join("archive");
    if !src_root.is_dir() {
        return Ok(false);
    }
    let dest_root = default_dir.join("archive");
    let mut moved_any = false;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&src_root)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for entry in entries {
        if !is_archive_entry(&entry) {
            continue;
        }
        let dest = dest_root.join(entry.file_name().unwrap());
        if dest.exists() {
            match resolve_archive_collision(root, &entry, &dest) {
                Ok(true) => moved_any = true,
                Ok(false) => {}
                Err(e) => {
                    let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                    crate::log_error!(
                        "failed to resolve archive {name} collision during migration: {e}"
                    );
                }
            }
            continue;
        }
        match (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dest_root)?;
            std::fs::rename(&entry, &dest)?;
            Ok(())
        })() {
            Ok(()) => moved_any = true,
            Err(e) => {
                let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                crate::log_error!("failed to migrate archive {name} into sessions/default: {e}");
            }
        }
    }
    let empty = std::fs::read_dir(&src_root)
        .map(|mut rd| rd.next().is_none())
        .unwrap_or(false);
    if empty {
        let _ = std::fs::remove_dir(&src_root);
    }
    Ok(moved_any)
}

fn merge_activity_json(root: &Path, default_dir: &Path) -> Result<bool, MergeError> {
    let src = root.join("activity.json");
    if !src.exists() {
        return Ok(false);
    }
    let dest = default_dir.join("activity.json");
    std::fs::create_dir_all(default_dir)?;
    {
        let _guard = FileLock::new(&default_dir.join("activity.lock"), LOCK_WAIT_SECONDS).lock()?;
        if !dest.exists() {
            std::fs::rename(&src, &dest)?;
            return Ok(true);
        }
        let merged = merge_activity_data(
            read_json(&dest, Value::Object(Map::new()))?,
            read_json(&src, Value::Object(Map::new()))?,
        );
        atomic_write_json(&dest, &merged)?;
    }
    std::fs::remove_file(&src)?;
    Ok(true)
}

fn later_iso_value(a: Option<&Value>, b: Option<&Value>) -> Option<Value> {
    let pa = a.and_then(Value::as_str).and_then(|s| parse_iso(Some(s)));
    let pb = b.and_then(Value::as_str).and_then(|s| parse_iso(Some(s)));
    match (pa, pb) {
        (None, None) => a.or(b).cloned(),
        (None, Some(_)) => b.cloned(),
        (Some(_), None) => a.cloned(),
        (Some(x), Some(y)) => {
            if x >= y {
                a.cloned()
            } else {
                b.cloned()
            }
        }
    }
}

fn merge_session_record(
    dest_rec: &Map<String, Value>,
    src_rec: &Map<String, Value>,
) -> Map<String, Value> {
    let mut merged = dest_rec.clone();
    for field in ["last_active", "restored_at", "agent_started_at"] {
        match later_iso_value(dest_rec.get(field), src_rec.get(field)) {
            None => {
                merged.remove(field);
            }
            Some(v) => {
                merged.insert(field.to_string(), v);
            }
        }
    }
    for (key, value) in src_rec {
        if !merged.contains_key(key) {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn merge_terminal_entry(dest_entry: Option<&Value>, src_entry: Option<&Value>) -> Option<Value> {
    let dest_obj = dest_entry.and_then(Value::as_object);
    let src_obj = src_entry.and_then(Value::as_object);
    match (dest_obj, src_obj) {
        (None, _) => src_entry.cloned(),
        (_, None) => dest_entry.cloned(),
        (Some(d), Some(s)) => {
            let started = later_iso_value(d.get("agent_started_at"), s.get("agent_started_at"));
            match started {
                Some(v) => {
                    let mut m = Map::new();
                    m.insert("agent_started_at".to_string(), v);
                    Some(Value::Object(m))
                }
                None => Some(Value::Object(d.clone())),
            }
        }
    }
}

fn merge_activity_data(dest_data: Value, src_data: Value) -> Value {
    let dest = dest_data.as_object().cloned().unwrap_or_default();
    let src = src_data.as_object().cloned().unwrap_or_default();
    let mut merged = dest.clone();
    for (key, rec) in &src {
        if key == "terminals" || !rec.is_object() {
            continue;
        }
        let rec = rec.as_object().unwrap();
        match merged.get(key).and_then(Value::as_object).cloned() {
            Some(d) => {
                merged.insert(key.clone(), Value::Object(merge_session_record(&d, rec)));
            }
            None => {
                merged.insert(key.clone(), Value::Object(rec.clone()));
            }
        }
    }
    let dest_t = dest
        .get("terminals")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let src_t = src
        .get("terminals")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut terminals = dest_t;
    for (term_id, entry) in &src_t {
        if let Some(v) = merge_terminal_entry(terminals.get(term_id), Some(entry)) {
            terminals.insert(term_id.clone(), v);
        }
    }
    if !terminals.is_empty() {
        merged.insert("terminals".to_string(), Value::Object(terminals));
    }
    Value::Object(merged)
}

fn merge_marker(
    root: &Path,
    default_dir: &Path,
    name: &str,
    keep: &str,
) -> Result<bool, std::io::Error> {
    let src = root.join(name);
    if !src.exists() {
        return Ok(false);
    }
    let dest = default_dir.join(name);
    if !dest.exists() {
        std::fs::rename(&src, &dest)?;
        return Ok(true);
    }
    if keep == "earlier" {
        let src_ts = parse_iso(Some(std::fs::read_to_string(&src)?.trim()));
        let dest_ts = parse_iso(Some(std::fs::read_to_string(&dest)?.trim()));
        if src_ts.is_some() && dest_ts.is_none_or(|d| src_ts.unwrap() < d) {
            std::fs::rename(&src, &dest)?;
            return Ok(true);
        }
    }
    std::fs::remove_file(&src)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use serde_json::json;

    #[test]
    fn noop_when_fresh() {
        let (_g, dir) = testutil::tempdir();
        merge_into_default_session(&dir);
        assert!(!dir.join("sessions").exists());
    }

    #[test]
    fn moves_legacy_state() {
        let (_g, dir) = testutil::tempdir();
        std::fs::write(
            dir.join("activity.json"),
            r#"{"a:b":{"first_seen":"2026-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("last_sweep"), "2026-01-01T00:00:00Z\n").unwrap();
        std::fs::write(dir.join("installed_at"), "2025-01-01T00:00:00Z\n").unwrap();
        let entry = dir.join("archive").join("20260101T000000Z-abcdef");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("record.json"), r#"{"id":"x"}"#).unwrap();
        merge_into_default_session(&dir);
        let def = dir.join("sessions").join("default");
        assert!(def.join("activity.json").exists());
        assert!(def.join("last_sweep").exists());
        assert!(def.join("installed_at").exists());
        assert!(
            def.join("archive")
                .join("20260101T000000Z-abcdef")
                .join("record.json")
                .exists()
        );
        assert!(!root_legacy_present(&dir));
    }

    #[test]
    fn identical_collision_deleted_differing_moved() {
        let (_g, dir) = testutil::tempdir();
        let id = "20260101T000000Z-abcdef";
        let src = dir.join("archive").join(id);
        let dst = dir
            .join("sessions")
            .join("default")
            .join("archive")
            .join(id);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("record.json"), r#"{"same":true}"#).unwrap();
        std::fs::write(dst.join("record.json"), r#"{"same":true}"#).unwrap();
        merge_into_default_session(&dir);
        assert!(!src.exists());
        // differing
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("record.json"), r#"{"same":false}"#).unwrap();
        merge_into_default_session(&dir);
        assert!(!src.exists());
        assert!(
            dir.join("archive.conflict")
                .join(id)
                .join("record.json")
                .exists()
        );
    }

    #[test]
    fn activity_merge_later_wins() {
        let dest = json!({"a:b": {"last_active": "2026-01-02T00:00:00Z", "first_seen": "2026-01-01T00:00:00Z"}});
        let src = json!({"a:b": {"last_active": "2026-01-03T00:00:00Z", "restored_at": "2026-01-01T00:00:00Z"}});
        let merged = merge_activity_data(dest, src);
        assert_eq!(merged["a:b"]["last_active"], json!("2026-01-03T00:00:00Z"));
        assert_eq!(merged["a:b"]["first_seen"], json!("2026-01-01T00:00:00Z"));
        assert_eq!(merged["a:b"]["restored_at"], json!("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn stray_files_untouched_and_never_legacy() {
        let (_g, dir) = testutil::tempdir();
        std::fs::create_dir_all(dir.join("archive")).unwrap();
        std::fs::write(dir.join("archive").join(".DS_Store"), "x").unwrap();
        std::fs::create_dir_all(dir.join("archive").join("odd-dir")).unwrap();
        assert!(!root_legacy_present(&dir));
        merge_into_default_session(&dir);
        assert!(dir.join("archive").join(".DS_Store").exists());
    }
}
