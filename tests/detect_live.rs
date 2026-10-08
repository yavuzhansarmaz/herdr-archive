//! Live-session detection prefers a Certain/Strong hit over the picker.
//!
//! resolve_missing_sessions_with_store validates against a fixture muse
//! store (no dependency on the machine's real sessions, no /proc).

#[path = "common/fakeherdr.rs"]
mod fakeherdr;

use fakeherdr::{FakeHerdr, ok};
use herdr_archive::agents;
use herdr_archive::api::Client;
use herdr_archive::manual;
use serde_json::{Map, Value, json};

const UUID: &str = "01a0da74-9f52-7760-a301-60a6f87abcf5";

/// Fixture muse store: `<base>/2026/01/02/<id>/session.jsonl`.
fn muse_store(base: &std::path::Path) {
    let d = base.join("2026").join("01").join("02").join(UUID);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("session.jsonl"),
        "{\"workspace_root\":\"/work/a\"}\n",
    )
    .unwrap();
}

fn muse_pane() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("pane_id".to_string(), json!("p1"));
    m.insert("agent".to_string(), json!("muse"));
    m.insert("cwd".to_string(), json!("/work/a"));
    m
}

#[test]
fn resolve_prefers_detected_argv_session() {
    // Versioned muse binary carrying `resume <uuid>`; the uuid exists in
    // the fixture store → used silently, the picker never prompts.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    muse_store(&dir);
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", |_| {
        ok(json!({"process_info": {"foreground_processes": [{
            "pid": 424242,
            "name": "muse-bin-1.4.4-R5419.1",
            "argv": ["/home/u/.local/bin/muse-bin-1.4.4-R5419.1", "resume", UUID],
            "cwd": "/work/a"}]}}))
    });
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let table = agents::table(None);
    let panes = vec![muse_pane()];
    let mut asked = 0;
    let mut input = |_: &str| -> Result<String, ()> {
        asked += 1;
        Err(())
    };
    let mut printed = Vec::new();
    let mut print = |s: &str| printed.push(s.to_string());
    let out = manual::resolve_missing_sessions_with_store(
        &panes,
        &table,
        &mut input,
        &mut print,
        &mut client,
        Some(&dir),
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.overrides.get("p1").map(String::as_str), Some(UUID));
    assert_eq!(
        out.provenance.get("p1").map(String::as_str),
        Some("detect:argv")
    );
    assert_eq!(asked, 0, "the picker must not prompt when detection hits");
    assert!(
        printed.iter().any(|l| l.contains("detect:argv")),
        "{printed:?}"
    );
    assert!(
        fake.methods().contains(&"pane.process_info".to_string()),
        "detection queries this pane's processes"
    );
    fake.close();
}

#[test]
fn resolve_falls_back_to_picker_when_nothing_detected() {
    // Bare `muse` argv (no resume id) and a pid with no muse fds → the
    // unchanged picker flow. 's' is accepted in both picker branches, so
    // this holds regardless of the machine's real muse store contents.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    muse_store(&dir);
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", |_| {
        ok(json!({"process_info": {"foreground_processes": [{
            "pid": 4294967295u32,
            "name": "muse",
            "argv": ["muse"],
            "cwd": "/work/a"}]}}))
    });
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let table = agents::table(None);
    let panes = vec![muse_pane()];
    let mut answers = vec!["s".to_string()];
    let mut prompts = Vec::new();
    let mut input = |p: &str| -> Result<String, ()> {
        prompts.push(p.to_string());
        answers.pop().ok_or(())
    };
    let mut printed = Vec::new();
    let mut print = |s: &str| printed.push(s.to_string());
    let out = manual::resolve_missing_sessions_with_store(
        &panes,
        &table,
        &mut input,
        &mut print,
        &mut client,
        Some(&dir),
    )
    .unwrap()
    .unwrap();
    assert!(out.overrides.is_empty());
    assert!(out.provenance.is_empty());
    assert!(
        prompts.iter().any(|l| l.contains("shell")),
        "picker prompt shown: {prompts:?} / {printed:?}"
    );
    assert!(
        !printed.iter().any(|l| l.contains("detect:")),
        "no detection claimed: {printed:?}"
    );
    fake.close();
}

#[test]
fn non_muse_panes_never_detect() {
    // Even when process_info would hand muse a session, a claude pane takes
    // the existing shell question and detection makes no socket call.
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", |_| {
        ok(json!({"process_info": {"foreground_processes": [{
            "pid": 424242,
            "name": "muse-bin-1.4.4-R5419.1",
            "argv": ["muse-bin-1.4.4-R5419.1", "resume", UUID],
            "cwd": "/work/a"}]}}))
    });
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let table = agents::table(None);
    let mut pane = Map::new();
    pane.insert("pane_id".to_string(), json!("p1"));
    pane.insert("agent".to_string(), json!("claude"));
    pane.insert("cwd".to_string(), json!("/work/a"));
    let mut answers = vec!["y".to_string()];
    let mut input = |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
    let mut printed = Vec::new();
    let mut print = |s: &str| printed.push(s.to_string());
    let out = manual::resolve_missing_sessions_with_store(
        std::slice::from_ref(&pane),
        &table,
        &mut input,
        &mut print,
        &mut client,
        None,
    )
    .unwrap()
    .unwrap();
    assert!(out.overrides.is_empty());
    assert!(!fake.methods().contains(&"pane.process_info".to_string()));
    fake.close();
}
