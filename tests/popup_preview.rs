//! The open-archive popup question is detection-aware: before composing it,
//! the popup previews what confirm-time detection will find for the tab's
//! lone muse pane without a reported session. Informational only — resolve
//! re-runs detection at confirm and never trusts the preview.

#[path = "common/fakeherdr.rs"]
mod fakeherdr;

use fakeherdr::{FakeHerdr, ok};
use herdr_archive::agents;
use herdr_archive::api::Client;
use herdr_archive::confirm;
use herdr_archive::detect::{
    ForegroundProc, Preview, preview_from_snapshot, preview_live_session_with_store,
};
use herdr_archive::manual;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

const UUID_A: &str = "01a0da74-9f52-7760-a301-60a6f87abcf5";
const UUID_B: &str = "00000000-0000-4000-8000-000000000001";

const UNKNOWN_LINE: &str = "Activity unknown — muse doesn't report sessions to herdr.";

/// Fixture muse store: `<base>/2026/01/02/<id>/session.jsonl`.
fn muse_store(base: &std::path::Path, ids: &[&str]) {
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

fn muse_pane() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("pane_id".to_string(), json!("p1"));
    m.insert("agent".to_string(), json!("muse"));
    m.insert("cwd".to_string(), json!("/work/a"));
    m
}

fn argv_info(id: &str) -> Value {
    json!({"process_info": {"foreground_processes": [{
        "pid": 424242,
        "name": "muse",
        "argv": ["muse", "resume", id],
        "cwd": "/work/a"}]}})
}

#[test]
fn popup_shows_detected_id() {
    // The argv signal validates against the store → the popup question names
    // the short session id and its source instead of the unknown line.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    muse_store(&dir, &[UUID_A]);
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", |_| ok(argv_info(UUID_A)));
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let preview = preview_live_session_with_store("p1", &mut client, Some(&dir));
    let line = herdr_archive::detect::popup_activity(UNKNOWN_LINE, &preview);
    assert_eq!(line, "Session 01a0da74… (auto-detected via argv)");
    // ...and that line is what the composed popup question shows.
    let question = confirm::lines("mytab", &line, &[]);
    assert!(
        question
            .iter()
            .any(|l| l.contains("Session 01a0da74… (auto-detected via argv)")),
        "{question:?}"
    );
    assert!(
        !question.iter().any(|l| l.contains("Activity unknown")),
        "{question:?}"
    );
    fake.close();
}

#[test]
fn popup_shows_ambiguous_count() {
    // Two distinct session locks: the popup says how many candidates the
    // confirm-time picker will offer. The snapshot seam injects fd targets
    // (no /proc dependency); resolve-time detection still yields None.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    muse_store(&dir, &[UUID_A, UUID_B]);
    let a = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID_A}/.session.lock");
    let b = format!("/home/u/.local/share/muse/sessions/2026/10/08/{UUID_B}/.session.lock");
    let procs = vec![ForegroundProc {
        pid: Some(7),
        name: "muse".to_string(),
        argv: vec!["muse".to_string()],
    }];
    let preview = preview_from_snapshot(&procs, &|_| vec![a.clone(), b.clone()], Some(&dir));
    assert_eq!(preview, Preview::Ambiguous { count: 2 });
    assert_eq!(
        herdr_archive::detect::detect_from_snapshot(
            &procs,
            &|_| vec![a.clone(), b.clone()],
            Some(&dir)
        ),
        None,
        "ambiguity stays the picker's job at resolve time"
    );
    let line = herdr_archive::detect::popup_activity(UNKNOWN_LINE, &preview);
    assert_eq!(line, "2 candidate sessions — you'll pick one");
    let question = confirm::lines("mytab", &line, &[]);
    assert!(
        question.iter().any(|l| l.contains("2 candidate sessions")),
        "{question:?}"
    );
}

#[test]
fn popup_keeps_unknown_line_when_nothing_detected() {
    // Bare `muse` argv and a pid with no muse fds → the existing unknown
    // line passes through the popup composition verbatim.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    muse_store(&dir, &[UUID_A]);
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", |_| {
        ok(json!({"process_info": {"foreground_processes": [{
            "pid": 4294967295u32,
            "name": "muse",
            "argv": ["muse"],
            "cwd": "/work/a"}]}}))
    });
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let preview = preview_live_session_with_store("p1", &mut client, Some(&dir));
    assert_eq!(preview, Preview::Miss);
    let line = herdr_archive::detect::popup_activity(UNKNOWN_LINE, &preview);
    assert_eq!(line, UNKNOWN_LINE);
    let question = confirm::lines("mytab", &line, &[]);
    assert!(
        question.iter().any(|l| l.contains(UNKNOWN_LINE)),
        "{question:?}"
    );
    fake.close();
}

#[test]
fn resolve_reruns_detection_and_never_trusts_the_preview() {
    // The popup previews session A; argv changes to session B before
    // confirm; resolve must use the fresh detection (B), proving it
    // re-runs detection instead of trusting the popup-time value.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    muse_store(&dir, &[UUID_A, UUID_B]);
    let calls = Arc::new(Mutex::new(0u32));
    let fake = FakeHerdr::new();
    fake.on("pane.process_info", {
        let calls = calls.clone();
        move |_| {
            let mut n = calls.lock().unwrap();
            *n += 1;
            ok(argv_info(if *n == 1 { UUID_A } else { UUID_B }))
        }
    });
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    // Popup time: sees session A.
    let preview = preview_live_session_with_store("p1", &mut client, Some(&dir));
    assert!(
        matches!(&preview, Preview::Hit(d) if d.id == UUID_A),
        "{preview:?}"
    );
    assert_eq!(
        herdr_archive::detect::popup_activity(UNKNOWN_LINE, &preview),
        "Session 01a0da74… (auto-detected via argv)"
    );
    // Confirm time: re-runs detection, sees session B, uses it silently.
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
    assert_eq!(out.overrides.get("p1").map(String::as_str), Some(UUID_B));
    assert_eq!(
        out.provenance.get("p1").map(String::as_str),
        Some("detect:argv")
    );
    assert_eq!(asked, 0, "detection hit: the picker must not prompt");
    let info_calls = fake
        .methods()
        .into_iter()
        .filter(|m| m == "pane.process_info")
        .count();
    assert_eq!(
        info_calls, 2,
        "one process_info for the popup preview, one fresh one for resolve"
    );
    fake.close();
}
