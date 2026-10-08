//! End-to-end sweep → archive → restore against the fake herdr server.

#[path = "common/fakeherdr.rs"]
mod fakeherdr;

use fakeherdr::{FakeHerdr, err, ok};
use herdr_archive::activity::ActivityStore;
use herdr_archive::agents;
use herdr_archive::api::Client;
use herdr_archive::archive::{self, Archive};
use herdr_archive::config::Config;
use herdr_archive::restore;
use herdr_archive::sweep;
use herdr_archive::util::{Ts, iso, now};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

fn old_ts() -> Ts {
    Ts(now().0 - 30 * 86_400 * 1_000_000)
}

fn tabs_json() -> Value {
    json!({"tabs": [{"tab_id": "t1", "workspace_id": "w1", "label": "old-work", "focused": false}]})
}

fn panes_json(cwd: &str) -> Value {
    json!({"panes": [{
        "pane_id": "p1", "tab_id": "t1", "cwd": cwd, "agent": "claude",
        "agent_status": "idle", "terminal_id": "term-1",
        "agent_session": {"agent": "claude", "kind": "id", "value": "SESS-FAKE-1", "source": "herdr:claude"},
    }]})
}

fn layout_json() -> Value {
    json!({"layout": {"root": {"type": "pane", "pane_id": "p1"}, "focused_pane_id": "p1", "zoomed": false}})
}

fn seed_activity(state: &std::path::Path) {
    std::fs::create_dir_all(state).unwrap();
    let old = iso(old_ts());
    let data = json!({"claude:SESS-FAKE-1": {"first_seen": old, "last_active": old}});
    std::fs::write(
        state.join("activity.json"),
        serde_json::to_string(&data).unwrap(),
    )
    .unwrap();
}

fn setup_sweep_server(cwd: String) -> FakeHerdr {
    let fake = FakeHerdr::new();
    fake.on("tab.list", move |_| ok(tabs_json()));
    let cwd2 = cwd.clone();
    fake.on("pane.list", move |_| ok(panes_json(&cwd2)));
    fake.on("workspace.list", |_| {
        ok(json!({"workspaces": [{"workspace_id": "w1", "label": "ws1"}]}))
    });
    fake.on("layout.export", |_| ok(layout_json()));
    fake.on("pane.process_info", |_| {
        ok(json!({"process_info": {"foreground_processes": [{"name": "claude", "argv": ["/usr/bin/claude", "--model", "opus"]}]}}))
    });
    fake.on("tab.close", |_| ok(json!({})));
    fake.on("notification.show", |_| ok(json!({})));
    fake
}

fn live_config() -> Config {
    Config {
        mode: "live".to_string(),
        ..Config::default()
    }
}

#[test]
fn sweep_archives_an_idle_tab() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = dir.join("state");
    seed_activity(&state);
    let fake = setup_sweep_server(dir.to_string_lossy().into_owned());
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let cfg = live_config();
    let table = agents::table(Some(&cfg.agents));
    let report = sweep::run(&mut client, &cfg, &state, &table, false, now(), "default")
        .unwrap()
        .unwrap();
    assert_eq!(report.mode, "live");
    assert_eq!(report.archived, vec!["old-work".to_string()]);
    assert!(report.failed.is_empty());
    // record on disk
    let records = Archive::new(&state).list();
    assert_eq!(records.len(), 1);
    let rec = &records[0];
    assert!(archive::valid_archive_id(rec["id"].as_str().unwrap()));
    assert_eq!(rec["panes"]["p1"]["agent"], json!("claude"));
    assert_eq!(rec["panes"]["p1"]["session"]["value"], json!("SESS-FAKE-1"));
    assert_eq!(
        rec["panes"]["p1"]["launch_argv"],
        json!(["/usr/bin/claude", "--model", "opus"])
    );
    assert_eq!(rec["panes"]["p1"]["resolved_by"], json!("herdr"));
    assert_eq!(rec["tool"], json!("herdr-archive 0.1.0"));
    // herdr told to close the tab + notify
    let methods = fake.methods();
    assert!(methods.contains(&"tab.close".to_string()));
    assert!(methods.contains(&"notification.show".to_string()));
    let bodies: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|(m, _)| m == "notification.show")
        .filter_map(|(_, p)| p.get("body").and_then(Value::as_str).map(str::to_string))
        .collect();
    assert_eq!(
        bodies,
        vec!["herdr-archive: archived 1 tab: old-work".to_string()]
    );
    // last_sweep + installed_at written
    assert!(state.join("last_sweep").exists());
    assert!(state.join("installed_at").exists());
    fake.close();
}

#[test]
fn rust_record_has_only_additive_keys() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = dir.join("state");
    seed_activity(&state);
    let fake = setup_sweep_server(dir.to_string_lossy().into_owned());
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let cfg = live_config();
    let table = agents::table(Some(&cfg.agents));
    sweep::run(&mut client, &cfg, &state, &table, false, now(), "default")
        .unwrap()
        .unwrap();
    let rec = &Archive::new(&state).list()[0];
    let top: Vec<&str> = rec
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let python_top = [
        "version",
        "id",
        "archived_at",
        "workspace",
        "tab",
        "layout",
        "panes",
        "session_copies",
        "herdr_session",
    ];
    for k in &top {
        assert!(
            python_top.contains(k) || ["tool", "name"].contains(k),
            "unexpected top-level key {k}"
        );
    }
    let pane: Vec<&str> = rec["panes"]["p1"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let python_pane = ["cwd", "agent", "session", "launch_argv", "last_activity"];
    for k in &pane {
        assert!(
            python_pane.contains(k) || ["resume_argv", "resolved_by"].contains(k),
            "unexpected pane key {k}"
        );
    }
    // workspace object: Python reads label/cwd; workspace_id is additive.
    let ws: Vec<&str> = rec["workspace"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for k in &ws {
        assert!(
            ["label", "cwd"].contains(k) || *k == "workspace_id",
            "unexpected workspace key {k}"
        );
    }
    assert_eq!(rec["workspace"]["workspace_id"], json!("w1"));
    assert_eq!(rec["workspace"]["label"], json!("ws1"));
    fake.close();
}

#[test]
fn named_archive_stores_name_and_keeps_label() {
    // The confirm flow's name lands in the additive `name` field while
    // `tab.label` stays untouched for Python compat.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = dir.join("state");
    seed_activity(&state);
    let fake = setup_sweep_server(dir.to_string_lossy().into_owned());
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let cfg = live_config();
    let table = agents::table(Some(&cfg.agents));
    let id = sweep::archive_now(
        &mut client,
        &cfg,
        &state,
        &table,
        "t1",
        now(),
        "default",
        None,
        true,
        None,
        None,
        Some("sprint 9"),
    )
    .unwrap();
    let rec = Archive::new(&state).load(&id).unwrap();
    assert_eq!(rec["name"], json!("sprint 9"));
    assert_eq!(rec["tab"]["label"], json!("old-work"));
    // Still within the additive key allowance (Python ignores both).
    let top: Vec<&str> = rec
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let python_top = [
        "version",
        "id",
        "archived_at",
        "workspace",
        "tab",
        "layout",
        "panes",
        "session_copies",
        "herdr_session",
    ];
    for k in &top {
        assert!(
            python_top.contains(k) || ["tool", "name"].contains(k),
            "unexpected top-level key {k}"
        );
    }
    // Nameless sweep archives omit `name` entirely.
    let fake = setup_sweep_server(dir.to_string_lossy().into_owned());
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    sweep::run(&mut client, &cfg, &state, &table, false, now(), "default")
        .unwrap()
        .unwrap();
    let records = Archive::new(&state).list();
    assert_eq!(records.len(), 2);
    let swept = records.iter().find(|r| r["id"] != json!(id)).unwrap();
    assert!(swept.get("name").is_none());
    assert_eq!(swept["tab"]["label"], json!("old-work"));
    fake.close();
}

#[test]
fn dry_run_archives_nothing() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = dir.join("state");
    seed_activity(&state);
    let fake = setup_sweep_server(dir.to_string_lossy().into_owned());
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let cfg = Config::default(); // dry-run
    let table = agents::table(Some(&cfg.agents));
    let report = sweep::run(&mut client, &cfg, &state, &table, false, now(), "default")
        .unwrap()
        .unwrap();
    assert_eq!(report.eligible, vec!["old-work".to_string()]);
    assert!(Archive::new(&state).list().is_empty());
    assert!(!fake.methods().contains(&"tab.close".to_string()));
    assert!(state.join("last_sweep").exists()); // gather succeeded
    fake.close();
}

fn setup_restore_server(apply_params: Arc<Mutex<Vec<Value>>>, workspaces: Value) -> FakeHerdr {
    let fake = FakeHerdr::new();
    fake.on("pane.list", |_| ok(json!({"panes": []})));
    fake.on("workspace.list", move |_| ok(workspaces.clone()));
    fake.on("workspace.create", |_| {
        ok(json!({"workspace": {"workspace_id": "w-new"}, "tab": {"tab_id": "t-new"}}))
    });
    fake.on("layout.apply", move |p| {
        apply_params.lock().unwrap().push(p.clone());
        ok(json!({"layout": {"tab_id": "t-restored"}}))
    });
    fake.on("workspace.close", |_| ok(json!({})));
    fake
}

fn archived_state(dir: &std::path::Path) -> std::path::PathBuf {
    let state = dir.join("state");
    seed_activity(&state);
    let fake = setup_sweep_server(dir.to_string_lossy().into_owned());
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let cfg = live_config();
    let table = agents::table(Some(&cfg.agents));
    sweep::run(&mut client, &cfg, &state, &table, false, now(), "default")
        .unwrap()
        .unwrap();
    fake.close();
    state
}

#[test]
fn restore_cli_always_creates_a_new_workspace() {
    // Even with the recorded id live and duplicate labels around, the
    // default target is a brand-new workspace — never a label guess.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = archived_state(&dir);
    let arch = Archive::new(&state);
    let id = arch.list()[0]["id"].as_str().unwrap().to_string();
    assert_eq!(arch.list()[0]["workspace"]["workspace_id"], json!("w1"));
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let fake = setup_restore_server(
        seen.clone(),
        json!({"workspaces": [
            {"workspace_id": "w2", "label": "ws1"},
            {"workspace_id": "w1", "label": "ws1"},
        ]}),
    );
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let store = ActivityStore::new(&state);
    let table: BTreeMap<String, herdr_archive::agents::Entry> = agents::table(None);
    let result = restore::restore(
        &mut client,
        &arch,
        &store,
        &id,
        &table,
        now(),
        restore::RestoreTarget::New,
    )
    .unwrap();
    assert_eq!(result.tab_id.as_deref(), Some("t-restored"));
    assert_eq!(result.warnings.len(), 1);
    assert!(
        result.warnings[0].contains("was not found, so it may not resume"),
        "{:?}",
        result.warnings
    );
    // created new: apply targets the fresh tab, not any live workspace
    let params = seen.lock().unwrap();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].get("workspace_id"), None);
    assert_eq!(params[0]["tab_id"], json!("t-new"));
    // resume command rebuilt through the table
    let cmd = params[0]["root"]["command"][2].as_str().unwrap();
    assert!(
        cmd.contains("claude --model opus --resume SESS-FAKE-1"),
        "{cmd}"
    );
    assert!(cmd.starts_with("trap : INT; "));
    // entry deleted, activity marked
    assert!(arch.list().is_empty());
    let data = store.load().unwrap();
    assert!(data["claude:SESS-FAKE-1"].get("restored_at").is_some());
    let methods = fake.methods();
    assert!(methods.contains(&"workspace.create".to_string()));
    fake.close();
}

#[test]
fn restore_existing_target_reuses_a_live_workspace() {
    // The picker's confirmed choice: reuse the original workspace id.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = archived_state(&dir);
    let arch = Archive::new(&state);
    let id = arch.list()[0]["id"].as_str().unwrap().to_string();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let fake = setup_restore_server(
        seen.clone(),
        json!({"workspaces": [
            {"workspace_id": "w2", "label": "ws1"},
            {"workspace_id": "w1", "label": "ws1"},
        ]}),
    );
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let store = ActivityStore::new(&state);
    let table = agents::table(None);
    let result = restore::restore(
        &mut client,
        &arch,
        &store,
        &id,
        &table,
        now(),
        restore::RestoreTarget::Existing("w1".to_string()),
    )
    .unwrap();
    assert_eq!(result.tab_id.as_deref(), Some("t-restored"));
    let params = seen.lock().unwrap();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0]["workspace_id"], json!("w1"));
    assert!(params[0].get("tab_id").is_none());
    assert!(!fake.methods().contains(&"workspace.create".to_string()));
    assert!(arch.list().is_empty());
    fake.close();
}

#[test]
fn restore_existing_target_falls_back_when_original_gone() {
    // Chosen in the picker, closed since: new workspace + warning.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = archived_state(&dir);
    let arch = Archive::new(&state);
    let id = arch.list()[0]["id"].as_str().unwrap().to_string();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let fake = setup_restore_server(
        seen.clone(),
        json!({"workspaces": [{"workspace_id": "w2", "label": "ws1"}]}),
    );
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let store = ActivityStore::new(&state);
    let table = agents::table(None);
    let result = restore::restore(
        &mut client,
        &arch,
        &store,
        &id,
        &table,
        now(),
        restore::RestoreTarget::Existing("w-dead".to_string()),
    )
    .unwrap();
    assert_eq!(result.tab_id.as_deref(), Some("t-restored"));
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("original workspace is gone")),
        "{:?}",
        result.warnings
    );
    let params = seen.lock().unwrap();
    assert_eq!(params[0]["tab_id"], json!("t-new"));
    assert!(fake.methods().contains(&"workspace.create".to_string()));
    fake.close();
}

#[test]
fn restore_legacy_record_without_id_creates_new() {
    // Old/Python records carry no workspace_id: always a new workspace.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = archived_state(&dir);
    let arch = Archive::new(&state);
    let id = arch.list()[0]["id"].as_str().unwrap().to_string();
    let path = state.join("archive").join(&id).join("record.json");
    let mut rec: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    rec["workspace"]
        .as_object_mut()
        .unwrap()
        .remove("workspace_id");
    std::fs::write(&path, serde_json::to_string(&rec).unwrap()).unwrap();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let fake = setup_restore_server(
        seen.clone(),
        json!({"workspaces": [
            {"workspace_id": "wA", "label": "ws1"},
            {"workspace_id": "wB", "label": "ws1"},
        ]}),
    );
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let store = ActivityStore::new(&state);
    let table = agents::table(None);
    let result = restore::restore(
        &mut client,
        &arch,
        &store,
        &id,
        &table,
        now(),
        restore::RestoreTarget::New,
    )
    .unwrap();
    assert_eq!(result.tab_id.as_deref(), Some("t-restored"));
    let params = seen.lock().unwrap();
    assert_eq!(params[0]["tab_id"], json!("t-new"));
    assert!(fake.methods().contains(&"workspace.create".to_string()));
    fake.close();
}

#[test]
fn restore_recreates_a_missing_workspace() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = archived_state(&dir);
    let arch = Archive::new(&state);
    let id = arch.list()[0]["id"].as_str().unwrap().to_string();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let fake = setup_restore_server(seen.clone(), json!({"workspaces": []}));
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let store = ActivityStore::new(&state);
    let table = agents::table(None);
    let result = restore::restore(
        &mut client,
        &arch,
        &store,
        &id,
        &table,
        now(),
        restore::RestoreTarget::New,
    )
    .unwrap();
    assert_eq!(result.tab_id.as_deref(), Some("t-restored"));
    let params = seen.lock().unwrap();
    assert_eq!(params[0].get("workspace_id"), None);
    assert_eq!(params[0]["tab_id"], json!("t-new"));
    let methods = fake.methods();
    assert!(methods.contains(&"workspace.create".to_string()));
    fake.close();
}

#[test]
fn restore_refuses_a_live_duplicate_and_keeps_the_entry() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = archived_state(&dir);
    let arch = Archive::new(&state);
    let id = arch.list()[0]["id"].as_str().unwrap().to_string();
    let fake = FakeHerdr::new();
    fake.on("pane.list", |_| {
        ok(json!({"panes": [{"pane_id": "qx", "agent": "claude",
            "agent_session": {"agent": "claude", "kind": "id", "value": "SESS-FAKE-1"}}]}))
    });
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let store = ActivityStore::new(&state);
    let table = agents::table(None);
    let err = restore::restore(
        &mut client,
        &arch,
        &store,
        &id,
        &table,
        now(),
        restore::RestoreTarget::New,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("already open in another tab"),
        "{err}"
    );
    assert_eq!(arch.list().len(), 1); // kept
    fake.close();
}

#[test]
fn restore_cleans_up_a_created_workspace_when_apply_fails() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = archived_state(&dir);
    let arch = Archive::new(&state);
    let id = arch.list()[0]["id"].as_str().unwrap().to_string();
    let closed: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let fake = FakeHerdr::new();
    fake.on("pane.list", |_| ok(json!({"panes": []})));
    fake.on("workspace.list", |_| ok(json!({"workspaces": []})));
    fake.on("workspace.create", |_| {
        ok(json!({"workspace": {"workspace_id": "w-new"}, "tab": {"tab_id": "t-new"}}))
    });
    fake.on("layout.apply", |_| err("apply_rejected", "nope"));
    let closed2 = closed.clone();
    fake.on("workspace.close", move |p| {
        closed2.lock().unwrap().push(p.clone());
        ok(json!({}))
    });
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let store = ActivityStore::new(&state);
    let table = agents::table(None);
    let err = restore::restore(
        &mut client,
        &arch,
        &store,
        &id,
        &table,
        now(),
        restore::RestoreTarget::New,
    )
    .unwrap_err();
    assert!(err.to_string().contains("apply_rejected"), "{err}");
    assert_eq!(
        closed.lock().unwrap().as_slice(),
        &[json!({"workspace_id": "w-new"})]
    );
    assert_eq!(arch.list().len(), 1); // kept
    fake.close();
}

#[test]
fn archive_now_confirmed_path_with_overrides() {
    // Manual archive of a muse tab via confirmed=true + overrides.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = dir.join("state");
    let fake = FakeHerdr::new();
    fake.on("tab.list", |_| {
        ok(json!({"tabs": [{"tab_id": "t1", "workspace_id": "w1", "label": "m", "focused": true}]}))
    });
    let cwd = dir.to_string_lossy().into_owned();
    fake.on("pane.list", move |_| {
        ok(
            json!({"panes": [{"pane_id": "p1", "tab_id": "t1", "cwd": cwd, "agent": "muse",
            "agent_status": "idle", "terminal_id": "term-1"}]}),
        )
    });
    fake.on("workspace.list", |_| {
        ok(json!({"workspaces": [{"workspace_id": "w1", "label": "ws"}]}))
    });
    fake.on("layout.export", |_| ok(layout_json()));
    fake.on("pane.process_info", |_| {
        ok(json!({"process_info": {"foreground_processes": [{"name": "muse", "argv": ["muse"]}]}}))
    });
    fake.on("tab.close", |_| ok(json!({})));
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let cfg = Config::default();
    let table = agents::table(Some(&cfg.agents));
    let mut overrides = BTreeMap::new();
    overrides.insert("p1".to_string(), "MUSE-1".to_string());
    let mut provenance = BTreeMap::new();
    provenance.insert("p1".to_string(), "scanner:muse".to_string());
    let id = sweep::archive_now(
        &mut client,
        &cfg,
        &state,
        &table,
        "t1",
        now(),
        "default",
        None,
        true,
        Some(&overrides),
        Some(&provenance),
        None,
    )
    .unwrap();
    let rec = Archive::new(&state).load(&id).unwrap();
    assert_eq!(rec["panes"]["p1"]["agent"], json!("muse"));
    assert_eq!(rec["panes"]["p1"]["session"]["source"], json!("manual"));
    assert_eq!(rec["panes"]["p1"]["resolved_by"], json!("scanner:muse"));
    // unconfirmed archive_now refuses the missing session without overrides
    let err = sweep::archive_now(
        &mut client,
        &cfg,
        &state,
        &table,
        "t1",
        now(),
        "default",
        None,
        false,
        None,
        None,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no session id"), "{err}");
    fake.close();
}

#[test]
fn track_records_status_and_detected_events() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = dir.join("state");
    let fake = FakeHerdr::new();
    fake.on("pane.get", |_| {
        ok(json!({"pane": {"pane_id": "p1", "terminal_id": "term-9",
            "agent_session": {"agent": "claude", "kind": "id", "value": "SESS-T"}} }))
    });
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let store = ActivityStore::new(&state);
    // status event
    let changed = herdr_archive::activity::track_with_retry(
        &mut client,
        &store,
        Some(r#"{"event": "pane_agent_status_changed", "data": {"pane_id": "p1", "agent_status": "done"}}"#),
        None,
        now(),
        Some("pane.agent_status_changed"),
        0,
    )
    .unwrap();
    assert!(changed);
    // duplicate done is not a change
    let changed = herdr_archive::activity::track_with_retry(
        &mut client,
        &store,
        Some(r#"{"event": "pane_agent_status_changed", "data": {"pane_id": "p1", "agent_status": "done"}}"#),
        None,
        now(),
        Some("pane.agent_status_changed"),
        0,
    )
    .unwrap();
    assert!(!changed);
    // detected event records the terminal + session
    let changed = herdr_archive::activity::track_with_retry(
        &mut client,
        &store,
        Some(r#"{"event": "pane_agent_detected", "data": {"pane_id": "p1", "agent": "claude"}}"#),
        None,
        now(),
        Some("pane.agent_detected"),
        0,
    )
    .unwrap();
    assert!(changed);
    let data = store.load().unwrap();
    assert!(
        data["terminals"]["term-9"]
            .get("agent_started_at")
            .is_some()
    );
    assert!(data["claude:SESS-T"].get("agent_started_at").is_some());
    // garbage + idle are ignored
    assert!(
        !herdr_archive::activity::track(&mut client, &store, Some("{"), None, now(), None).unwrap()
    );
    assert!(
        !herdr_archive::activity::track(
            &mut client,
            &store,
            Some(r#"{"data": {"pane_id": "p1", "agent_status": "idle"}}"#),
            None,
            now(),
            None,
        )
        .unwrap()
    );
    fake.close();
}

#[test]
fn preview_reports_blocks_and_activity() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = dir.join("state");
    seed_activity(&state);
    let fake = setup_sweep_server(dir.to_string_lossy().into_owned());
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let cfg = Config::default();
    let table = agents::table(Some(&cfg.agents));
    let pv = sweep::preview(&mut client, &state, &table, "t1", now()).unwrap();
    assert_eq!(pv.tab_id, "t1");
    assert_eq!(pv.label, "old-work");
    assert_eq!(pv.terminals, vec!["term-1".to_string()]);
    assert!(pv.blocks.is_empty());
    assert!(pv.warnings.is_empty());
    assert!(pv.activity.contains("30 days ago"), "{}", pv.activity);
    fake.close();
}

#[test]
fn api_client_error_shapes() {
    // unknown method -> definite herdr error
    let fake = FakeHerdr::new();
    let mut client = Client::new(Some(fake.path.to_str().unwrap()), 10.0).unwrap();
    let e = client.call("nope", Value::Object(Map::new())).unwrap_err();
    assert_eq!(e.code, "unknown_method");
    assert!(e.definite);
    // handler error -> definite with code
    fake.on("boom", |_| err("confirmation_required", "worktree"));
    let e = client.call("boom", Value::Object(Map::new())).unwrap_err();
    assert_eq!(e.code, "confirmation_required");
    assert!(e.definite);
    // missing socket file -> unavailable/definite
    let mut client2 = Client::new(Some("/nonexistent-xyz/herdr.sock"), 1.0).unwrap();
    let e2 = client2
        .call("tab.list", Value::Object(Map::new()))
        .unwrap_err();
    assert_eq!(e2.code, "unavailable");
    assert!(e2.definite);
    fake.close();
}
