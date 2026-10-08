//! Manifest parity: herdr-plugin.toml shape + version sync with Cargo.toml.
//!
//! Port of `tests/test_manifest.py` (hand-rolled TOML scan: only the pinned
//! shape below is asserted, so no TOML parser dependency is needed).

use std::collections::HashMap;

fn manifest_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(name: &str) -> String {
    std::fs::read_to_string(manifest_dir().join(name)).unwrap()
}

/// Top-level `key = "value"` pairs (first occurrence wins).
fn top_pairs(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') || line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            out.entry(k.trim().to_string()).or_insert(v);
        }
    }
    out
}

fn section_blocks(text: &str, header: &str) -> Vec<HashMap<String, String>> {
    let mut blocks = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[[") {
            if let Some(c) = current.take() {
                blocks.push(c);
            }
            current = if line == header {
                Some(HashMap::new())
            } else {
                None
            };
            continue;
        }
        if line.starts_with('[') || line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let (Some(c), Some((k, v))) = (current.as_mut(), line.split_once('=')) {
            c.entry(k.trim().to_string())
                .or_insert(v.trim().to_string());
        }
    }
    if let Some(c) = current.take() {
        blocks.push(c);
    }
    blocks
}

#[test]
fn manifest_shape_and_commands() {
    let text = read("herdr-plugin.toml");
    let top = top_pairs(&text);
    assert_eq!(top["id"], "herdr-archive");
    assert_eq!(top["name"], "Archive");
    assert_eq!(top["min_herdr_version"], "0.9.0");
    assert_eq!(top["platforms"], r#"["linux", "macos"]"#);

    let build = section_blocks(&text, "[[build]]");
    assert_eq!(build.len(), 1);
    assert_eq!(
        build[0]["command"],
        r#"["cargo", "build", "--release", "--locked"]"#
    );

    let startups = section_blocks(&text, "[[startup]]");
    assert_eq!(startups.len(), 1);
    assert!(
        startups[0]["command"].contains("sweep"),
        "{}",
        startups[0]["command"]
    );

    let events = section_blocks(&text, "[[events]]");
    let ons: Vec<&str> = events.iter().map(|e| e["on"].trim_matches('"')).collect();
    assert_eq!(
        ons,
        vec![
            "pane.agent_status_changed",
            "pane.agent_detected",
            "workspace.focused"
        ]
    );

    let actions = section_blocks(&text, "[[actions]]");
    let ids: Vec<&str> = actions.iter().map(|a| a["id"].trim_matches('"')).collect();
    assert_eq!(ids, vec!["restore", "archive-tab", "sweep-now"]);
    for a in &actions {
        assert!(
            a["command"].starts_with(r#"["./target/release/herdr-archive""#),
            "{}",
            a["command"]
        );
    }

    let panes = section_blocks(&text, "[[panes]]");
    let ids: Vec<&str> = panes.iter().map(|p| p["id"].trim_matches('"')).collect();
    assert_eq!(ids, vec!["picker", "archive-confirm"]);
    for p in &panes {
        assert!(
            p["command"].starts_with(r#"["./target/release/herdr-archive""#),
            "{}",
            p["command"]
        );
    }
}

#[test]
fn manifest_version_matches_cargo_toml() {
    let manifest_version = top_pairs(&read("herdr-plugin.toml"))["version"].clone();
    let cargo_version = top_pairs(&read("Cargo.toml"))["version"].clone();
    assert_eq!(manifest_version, "0.1.0");
    assert_eq!(manifest_version, cargo_version);
    assert_eq!(manifest_version, env!("CARGO_PKG_VERSION"));
}
