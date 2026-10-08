//! Cross-compatibility with the Python shelf's archive records.
//!
//! Fixtures in `tests/fixtures/` are real `record.json` files written by the
//! Python shelf's own `capture()` (see `gen_fixtures.py`), plus the Python
//! `build_tree()` output for each. Rust must load them and produce identical
//! trees.

use herdr_archive::agents;
use herdr_archive::archive::Archive;
use herdr_archive::picker;
use herdr_archive::restore;
use std::collections::BTreeMap;

fn fixture(name: &str) -> serde_json::Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn python_records_load_and_rebuild_identically() {
    let table = agents::table(None);
    for name in ["python-claude-tab", "python-manual-muse"] {
        let record: serde_json::Value = fixture(&format!("{name}.json"));
        let expected: serde_json::Value = fixture(&format!("{name}.tree.json"));
        let panes = record["panes"].as_object().unwrap().clone();
        let mut argv_log = BTreeMap::new();
        let tree =
            restore::build_tree(&record["layout"]["root"], &panes, &table, &mut argv_log).unwrap();
        assert_eq!(tree, expected, "{name}: build_tree differs from Python's");
    }
}

#[test]
fn python_records_list_from_a_copied_archive_dir() {
    // A Python archive directory copied into the herdr-archive tree just works.
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let record: serde_json::Value = fixture("python-claude-tab.json");
    let id = record["id"].as_str().unwrap().to_string();
    let entry = dir.join("archive").join(&id);
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(
        entry.join("record.json"),
        serde_json::to_string(&record).unwrap(),
    )
    .unwrap();
    let listed = Archive::new(&dir).list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], serde_json::json!(id));
    assert_eq!(
        listed[0]["panes"]["p1"]["agent"],
        serde_json::json!("claude")
    );
    // Unknown-to-Rust keys would survive a round trip untouched (additive-only).
    assert!(listed[0].get("tool").is_none());
}

#[test]
fn nameless_records_fall_back_to_tab_label() {
    // Python-written records have no `name`: the picker and restore path
    // use the tab label exactly as before.
    let record: serde_json::Value = fixture("python-claude-tab.json");
    assert!(record.get("name").is_none());
    let label = record["tab"]["label"].as_str().unwrap().to_string();
    assert!(!label.is_empty());
    assert_eq!(picker::record_label(&record), label);
}

#[test]
fn named_records_restore_like_nameless_ones() {
    // The additive `name` is display-only: restore ignores it.
    let table = agents::table(None);
    let expected: serde_json::Value = fixture("python-claude-tab.tree.json");
    let mut record: serde_json::Value = fixture("python-claude-tab.json");
    record["name"] = serde_json::json!("my custom name");
    assert_eq!(picker::record_label(&record), "my custom name");
    let panes = record["panes"].as_object().unwrap().clone();
    let mut argv_log = BTreeMap::new();
    let tree =
        restore::build_tree(&record["layout"]["root"], &panes, &table, &mut argv_log).unwrap();
    assert_eq!(tree, expected);
}
