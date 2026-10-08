//! Lock-leg ambiguity scopes the resolve-time picker to exactly the N
//! validated live sessions — never the full scanner history.
//!
//! Linux-only: ambiguity is derived from this process's own /proc fd table
//! (fixture `.session.lock` files held open, own pid reported via
//! `process_info`), so these tests serialize on a mutex — fds are
//! process-wide and a parallel test's locks would join the count.
#![cfg(target_os = "linux")]

#[path = "common/fakeherdr.rs"]
mod fakeherdr;

use fakeherdr::{FakeHerdr, ok};
use herdr_archive::agents;
use herdr_archive::api::Client;
use herdr_archive::detect::Preview;
use herdr_archive::manual;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

const UUID_A: &str = "01a0da74-9f52-7760-a301-60a6f87abcf5";
const UUID_B: &str = "00000000-0000-4000-8000-000000000001";
const UUID_C: &str = "11111111-1111-4111-8111-111111111111";

/// Fixture muse store: `<store>/2026/01/02/<id>/{session.jsonl,.session.lock}`.
/// Holds nothing open; [`hold_locks`] selects which locks this process keeps.
fn muse_store(base: &Path, ids: &[&str]) -> PathBuf {
    let store = base.join("sessions");
    for id in ids {
        let d = store.join("2026").join("01").join("02").join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("session.jsonl"),
            "{\"workspace_root\":\"/work/a\"}\n",
        )
        .unwrap();
        std::fs::write(d.join(".session.lock"), "").unwrap();
    }
    store
}

/// Keep these sessions' locks open: while the handles live, this process's
/// own /proc fd table names them.
fn hold_locks(store: &Path, ids: &[&str]) -> Vec<std::fs::File> {
    ids.iter()
        .map(|id| {
            std::fs::File::open(
                store
                    .join("2026")
                    .join("01")
                    .join("02")
                    .join(id)
                    .join(".session.lock"),
            )
            .unwrap()
        })
        .collect()
}

fn muse_pane() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("pane_id".to_string(), json!("p1"));
    m.insert("agent".to_string(), json!("muse"));
    m.insert("cwd".to_string(), json!("/work/a"));
    m
}

/// `process_info` naming this test process's own pid as a bare muse
/// process: detection reads our real held-open fixture locks via /proc.
fn own_muse_info() -> Value {
    json!({"process_info": {"foreground_processes": [{
        "pid": std::process::id(),
        "name": "muse",
        "argv": ["muse"],
        "cwd": "/work/a"}]}})
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn rows(printed: &[String]) -> Vec<&String> {
    printed.iter().filter(|l| l.starts_with("  ")).collect()
}

#[test]
fn resolve_lists_exactly_the_ambiguous_live_sessions() {
    let _serial = SERIAL.lock().unwrap();
    let (_g, dir) = herdr_archive::testutil::tempdir();
    // A third validated session exists in the store but holds no lock: it
    // must NOT be listed (this is scoping, not the scanner).
    let store = muse_store(&dir, &[UUID_A, UUID_B, UUID_C]);
    let _held = hold_locks(&store, &[UUID_A, UUID_B]);
    // Newest-first order: A newest, B older.
    herdr_archive::testutil::backdate(
        &store
            .join("2026")
            .join("01")
            .join("02")
            .join(UUID_B)
            .join("session.jsonl"),
        3600,
    );
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", |_| ok(own_muse_info()));
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    // Popup time: the preview count promises two rows.
    let preview =
        herdr_archive::detect::preview_live_session_with_store("p1", &mut client, Some(&store));
    assert_eq!(preview, Preview::Ambiguous { count: 2 });
    // Resolve time, digit pick: exactly the two locked sessions listed.
    let table = agents::table(None);
    let panes = vec![muse_pane()];
    let mut answers = vec!["2".to_string()];
    let mut input = |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
    let mut printed = Vec::new();
    let mut print = |s: &str| printed.push(s.to_string());
    let out = manual::resolve_missing_sessions_with_store(
        &panes,
        &table,
        &mut input,
        &mut print,
        &mut client,
        Some(&store),
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.overrides.get("p1").map(String::as_str), Some(UUID_B));
    assert_eq!(
        out.provenance.get("p1").map(String::as_str),
        Some("detect:ambiguous")
    );
    assert!(
        printed
            .iter()
            .any(|l| l.contains("2 candidate muse sessions")),
        "{printed:?}"
    );
    assert_eq!(rows(&printed).len(), 2, "{printed:?}");
    assert!(
        printed.iter().any(|l| l.contains(&short(UUID_A))),
        "{printed:?}"
    );
    assert!(
        printed.iter().any(|l| l.contains(&short(UUID_B))),
        "{printed:?}"
    );
    assert!(
        !printed.iter().any(|l| l.contains(&short(UUID_C))),
        "unlocked store session must not be listed: {printed:?}"
    );
    // Resolve time, default pick: the newest of the two.
    let mut answers = vec![String::new()];
    let mut input = |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
    let mut printed = Vec::new();
    let mut print = |s: &str| printed.push(s.to_string());
    let out = manual::resolve_missing_sessions_with_store(
        &panes,
        &table,
        &mut input,
        &mut print,
        &mut client,
        Some(&store),
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.overrides.get("p1").map(String::as_str), Some(UUID_A));
    assert_eq!(
        out.provenance.get("p1").map(String::as_str),
        Some("detect:ambiguous")
    );
    fake.close();
}

#[test]
fn stale_lock_falls_back_to_the_scanner_picker() {
    // Two distinct locks but only one validated session: not ambiguity —
    // the unchanged scanner flow runs (either branch accepts 's').
    let _serial = SERIAL.lock().unwrap();
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let store = muse_store(&dir, &[UUID_A]);
    // B's lock exists on disk (held open) but B has no session.jsonl, so B
    // never validates.
    let b_dir = store.join("2026").join("01").join("02").join(UUID_B);
    std::fs::create_dir_all(&b_dir).unwrap();
    std::fs::write(b_dir.join(".session.lock"), "").unwrap();
    let _held = hold_locks(&store, &[UUID_A, UUID_B]);
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", |_| ok(own_muse_info()));
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let preview =
        herdr_archive::detect::preview_live_session_with_store("p1", &mut client, Some(&store));
    assert_eq!(preview, Preview::Miss);
    let table = agents::table(None);
    let panes = vec![muse_pane()];
    let mut answers = vec!["s".to_string()];
    let mut input = |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
    let mut printed = Vec::new();
    let mut print = |s: &str| printed.push(s.to_string());
    let out = manual::resolve_missing_sessions_with_store(
        &panes,
        &table,
        &mut input,
        &mut print,
        &mut client,
        Some(&store),
    )
    .unwrap()
    .unwrap();
    assert!(out.overrides.is_empty());
    assert!(
        !printed
            .iter()
            .any(|l| l.contains("candidate muse sessions")),
        "no ambiguity scoping on a stale lock: {printed:?}"
    );
    assert!(
        !printed.iter().any(|l| l.contains("detect:ambiguous")),
        "{printed:?}"
    );
    fake.close();
}

#[test]
fn resolve_recomputes_ambiguity_fresh() {
    // The popup previews {A, B}; the locks change to {A, C} before confirm;
    // resolve must list the fresh set, proving it never trusts the preview.
    let _serial = SERIAL.lock().unwrap();
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let store = muse_store(&dir, &[UUID_A, UUID_B, UUID_C]);
    let held = hold_locks(&store, &[UUID_A, UUID_B]);
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", |_| ok(own_muse_info()));
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let preview =
        herdr_archive::detect::preview_live_session_with_store("p1", &mut client, Some(&store));
    assert_eq!(preview, Preview::Ambiguous { count: 2 });
    // Locks change between popup and confirm: B released, C taken.
    drop(held);
    let held = hold_locks(&store, &[UUID_A, UUID_C]);
    let table = agents::table(None);
    let panes = vec![muse_pane()];
    let mut answers = vec!["2".to_string()];
    let mut input = |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
    let mut printed = Vec::new();
    let mut print = |s: &str| printed.push(s.to_string());
    let out = manual::resolve_missing_sessions_with_store(
        &panes,
        &table,
        &mut input,
        &mut print,
        &mut client,
        Some(&store),
    )
    .unwrap()
    .unwrap();
    assert_eq!(rows(&printed).len(), 2, "{printed:?}");
    assert!(
        printed.iter().any(|l| l.contains(&short(UUID_A))),
        "{printed:?}"
    );
    assert!(
        printed.iter().any(|l| l.contains(&short(UUID_C))),
        "{printed:?}"
    );
    assert!(
        !printed.iter().any(|l| l.contains(&short(UUID_B))),
        "stale preview member must not be listed: {printed:?}"
    );
    assert!(
        out.overrides.get("p1").map(String::as_str) == Some(UUID_A)
            || out.overrides.get("p1").map(String::as_str) == Some(UUID_C),
        "pick comes from the fresh set: {out:?}"
    );
    drop(held);
    fake.close();
}
