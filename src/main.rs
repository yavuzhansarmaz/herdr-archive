//! Command-line entry: herdr-archive <command>.
//!
//! Port of `shelf/__main__.py`. Same commands, same exit codes (0/1/2),
//! same messages (with the `herdr-archive` name).

use herdr_archive::activity::{self, ActivityStore};
use herdr_archive::agents;
use herdr_archive::api::{Client, HerdrError};
use herdr_archive::archive::{self, Archive, Skip};
use herdr_archive::config::{self, ConfigError};
use herdr_archive::confirm;
use herdr_archive::manual;
use herdr_archive::migrate;
use herdr_archive::picker;
use herdr_archive::picker_tty;
use herdr_archive::restore;
use herdr_archive::scan;
use herdr_archive::session;
use herdr_archive::sweep;
use herdr_archive::util::{FileLock, LockError, Ts, now};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

const PLUGIN_ID: &str = "herdr-archive";
const ALWAYS_HOOKS: &[&str] = &["track", "open-picker", "open-archive"];
const USAGE: &str = "usage: herdr-archive {track | sweep [--if-due] | archive <tab-id> | open-archive | confirm-archive | open-picker | pick | list | restore <archive-id>}";
const CONFIG_ERROR_NOTIFY_INTERVAL_SECONDS: f64 = 3600.0;
const UNKNOWN_SESSION_DISPLAY: &str = "unknown";
const MIGRATION_MARKERS: &[&str] = &["activity.json", "installed_at", "last_sweep"];
const MIGRATING_MESSAGE: &str =
    "herdr-archive: migrating state from an older version; try again in a moment";

fn is_hook(command: &str, args: &[String]) -> bool {
    if ALWAYS_HOOKS.contains(&command) {
        return true;
    }
    if command == "sweep" {
        return args.iter().any(|a| a == "--if-due");
    }
    false
}

fn state_dir() -> PathBuf {
    if let Some(env) = std::env::var("HERDR_PLUGIN_STATE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(env);
    }
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            herdr_archive::history::home_dir()
                .join(".local")
                .join("state")
        });
    base.join("herdr").join("plugins").join(PLUGIN_ID)
}

fn config_dir() -> PathBuf {
    if let Some(env) = std::env::var("HERDR_PLUGIN_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(env);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| herdr_archive::history::home_dir().join(".config"));
    base.join("herdr")
        .join("plugins")
        .join("config")
        .join(PLUGIN_ID)
}

fn session_name() -> Option<String> {
    session::herdr_session_name(std::env::var("HERDR_SOCKET_PATH").ok().as_deref())
}

fn display_session_name(name: &Option<String>) -> &str {
    name.as_deref().unwrap_or(UNKNOWN_SESSION_DISPLAY)
}

fn session_allowed(name: &Option<String>, config_dir: &Path) -> bool {
    match name {
        None => false,
        Some(n) => config::session_enabled(&config::sessions_for_gate(Some(config_dir)), n),
    }
}

fn migration_incomplete(root: &Path) -> bool {
    MIGRATION_MARKERS.iter().any(|m| root.join(m).exists())
}

fn disabled_message(name: &Option<String>) -> String {
    format!(
        "herdr-archive is not enabled for herdr session '{}'; add it to \"sessions\" in config.json",
        display_session_name(name)
    )
}

fn notify(client: &mut dyn herdr_archive::Herdr, body: &str) {
    if let Err(e) = client.call(
        "notification.show",
        serde_json::json!({"title": "herdr-archive", "body": body}),
    ) {
        herdr_archive::log_warn!("notification failed: {e}");
    }
}

fn notify_disabled(client: &mut dyn herdr_archive::Herdr, name: &Option<String>) {
    notify(
        client,
        &format!(
            "herdr-archive is not enabled for herdr session {}",
            display_session_name(name)
        ),
    );
}

fn notify_config_error(state: &Path, message: &str) {
    let Ok(_guard) = FileLock::new(&state.join("config-error.lock"), 0.0).lock() else {
        return; // another process is already handling this config error
    };
    let marker = state.join("config-error-notified");
    if let Ok(md) = std::fs::metadata(&marker) {
        if let Ok(mtime) = md.modified() {
            if let Ok(age) = std::time::SystemTime::now().duration_since(mtime) {
                if age.as_secs_f64() < CONFIG_ERROR_NOTIFY_INTERVAL_SECONDS {
                    return;
                }
            }
        }
    }
    let mut client = match Client::from_env() {
        Ok(c) => c,
        Err(e) => {
            herdr_archive::log_warn!("notification failed: {e}");
            return;
        }
    };
    if let Err(e) = client.call(
        "notification.show",
        serde_json::json!({"title": "herdr-archive", "body": format!("herdr-archive: config.json is invalid: {message}")}),
    ) {
        herdr_archive::log_warn!("notification failed: {e}");
        return;
    }
    let _ = std::fs::create_dir_all(state);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&marker);
}

fn describe(rec: &Value, moment: Ts) -> String {
    let tab = rec
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            rec.get("tab")
                .and_then(|t| t.get("label"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("(unnamed)");
    let workspace = rec
        .get("workspace")
        .and_then(|w| w.get("label"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let names = picker::agent_names(rec);
    let idle = picker::days_idle(rec, moment)
        .map(|d| format!("{d}d idle"))
        .unwrap_or_default();
    let mut parts = vec![tab.to_string()];
    for p in [workspace, names.as_str(), idle.as_str()] {
        if !p.is_empty() {
            parts.push(p.to_string());
        }
    }
    parts.join("  ")
}

/// Expected-failure bucket shared by every command handler.
#[derive(Debug)]
enum MainError {
    Config(ConfigError),
    LockBusy(String),
    Herdr(HerdrError),
    Skip(Skip),
    /// KeyError/ValueError/OSError bucket: log + exit 1 (0 for hooks).
    Expected(String),
}

impl std::fmt::Display for MainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MainError::Config(e) => write!(f, "{e}"),
            MainError::LockBusy(e) => write!(f, "{e}"),
            MainError::Herdr(e) => write!(f, "{e}"),
            MainError::Skip(e) => write!(f, "{e}"),
            MainError::Expected(e) => write!(f, "{e}"),
        }
    }
}

impl From<ConfigError> for MainError {
    fn from(e: ConfigError) -> Self {
        MainError::Config(e)
    }
}

impl From<HerdrError> for MainError {
    fn from(e: HerdrError) -> Self {
        MainError::Herdr(e)
    }
}

impl From<Skip> for MainError {
    fn from(e: Skip) -> Self {
        MainError::Skip(e)
    }
}

impl From<std::io::Error> for MainError {
    fn from(e: std::io::Error) -> Self {
        MainError::Expected(e.to_string())
    }
}

impl From<LockError> for MainError {
    fn from(e: LockError) -> Self {
        match e {
            LockError::Busy(p) => MainError::LockBusy(p.display().to_string()),
            LockError::Io(e) => MainError::Expected(e.to_string()),
        }
    }
}

impl From<activity::StoreError> for MainError {
    fn from(e: activity::StoreError) -> Self {
        match e {
            activity::StoreError::Lock(l) => l.into(),
            activity::StoreError::Io(i) => MainError::Expected(i.to_string()),
        }
    }
}

impl From<sweep::SweepError> for MainError {
    fn from(e: sweep::SweepError) -> Self {
        match e {
            sweep::SweepError::Herdr(h) => MainError::Herdr(h),
            sweep::SweepError::Io(i) => MainError::Expected(i.to_string()),
            sweep::SweepError::Lock(l) => l.into(),
            sweep::SweepError::Store(s) => s.into(),
            sweep::SweepError::Skip(s) => MainError::Skip(s),
        }
    }
}

impl From<restore::RestoreError> for MainError {
    fn from(e: restore::RestoreError) -> Self {
        match e {
            restore::RestoreError::Herdr(h) => MainError::Herdr(h),
            restore::RestoreError::Io(i) => MainError::Expected(i.to_string()),
            restore::RestoreError::Lock(l) => l.into(),
            restore::RestoreError::Skip(s) => MainError::Skip(s),
            restore::RestoreError::NotFound(id) => MainError::Expected(id),
            restore::RestoreError::Invalid(m) => MainError::Expected(m),
        }
    }
}

impl From<activity::TrackError> for MainError {
    fn from(e: activity::TrackError) -> Self {
        match e {
            activity::TrackError::Herdr(h) => MainError::Herdr(h),
            activity::TrackError::Store(s) => s.into(),
        }
    }
}

impl From<archive::KeyError> for MainError {
    fn from(e: archive::KeyError) -> Self {
        MainError::Expected(e.0)
    }
}

/// Keep log lines off a popup's screen while it runs.
struct LogsOffStderr;

impl LogsOffStderr {
    fn new() -> Self {
        herdr_archive::log::set_stderr(false);
        LogsOffStderr
    }
}

impl Drop for LogsOffStderr {
    fn drop(&mut self) {
        herdr_archive::log::set_stderr(true);
    }
}

fn open_archive(
    client: &mut dyn herdr_archive::Herdr,
    session_state: &Path,
) -> Result<(), MainError> {
    let Some(tab_id) = std::env::var("HERDR_TAB_ID").ok().filter(|s| !s.is_empty()) else {
        notify(client, "herdr-archive: no tab to archive");
        return Ok(());
    };
    let cfg = config::load(Some(config_dir().as_path()), true)?;
    let table = agents::table(Some(&cfg.agents));
    let found = sweep::preview(client, session_state, &table, &tab_id, now())?;
    if let Some(first) = found.blocks.first() {
        notify(
            client,
            &format!("herdr-archive: can't archive \"{}\": {first}", found.label),
        );
        return Ok(());
    }
    let live: Option<Vec<Map<String, Value>>> = client
        .call("pane.list", Value::Object(Map::new()))
        .ok()
        .and_then(|v| {
            v.get("panes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|p| p.as_object().cloned()).collect())
        });
    // Detection-aware question: when the tab holds exactly one muse pane
    // without a reported session, preview what confirm-time detection will
    // find so the popup names the session instead of crying unknown.
    // Informational only — confirm re-runs detection and never trusts this.
    let activity = match &live {
        Some(panes) => {
            let mut candidates = panes.iter().filter(|p| {
                p.get("tab_id").and_then(Value::as_str) == Some(found.tab_id.as_str())
                    && p.get("agent").and_then(Value::as_str) == Some("muse")
                    && table.contains_key("muse")
                    && !p
                        .get("agent_session")
                        .and_then(Value::as_object)
                        .and_then(|s| s.get("value"))
                        .and_then(Value::as_str)
                        .is_some_and(|v| !v.is_empty())
            });
            let (first, second) = (candidates.next(), candidates.next());
            match (first, second) {
                (Some(pane), None) => match pane.get("pane_id").and_then(Value::as_str) {
                    Some(pane_id) => {
                        let preview = herdr_archive::detect::preview_live_session(pane_id, client);
                        herdr_archive::detect::popup_activity(&found.activity, &preview)
                    }
                    None => found.activity.clone(),
                },
                _ => found.activity.clone(),
            }
        }
        None => found.activity.clone(),
    };
    let lines = confirm::lines(&found.label, &activity, &found.warnings);
    // Size the popup to its actual content: scan the same directories the
    // confirm step will ask about, so the list fits without a giant box.
    // (Scans run again at confirm time; a store changing in between only
    // costs a row or two of slack.) Amp's scan shells out with a 5s cap;
    // this path is user-invoked, so the bounded wait is acceptable.
    let slack = match &live {
        None => 17,
        Some(panes) => {
            let mut needs = Vec::new();
            for p in panes
                .iter()
                .filter(|p| p.get("tab_id").and_then(Value::as_str) == Some(found.tab_id.as_str()))
            {
                let agent = p.get("agent").and_then(Value::as_str).unwrap_or("");
                if agent.is_empty() {
                    continue;
                }
                let has_value = p
                    .get("agent_session")
                    .and_then(Value::as_object)
                    .and_then(|s| s.get("value"))
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.is_empty());
                if has_value {
                    continue;
                }
                // Mirror manual::resolve_missing_sessions: scanner list when
                // the kind is resumable and scanned, shell question otherwise.
                if table.contains_key(agent) {
                    match scan::candidates_for(agent, manual::pane_cwd(p)) {
                        Some(cands) => needs.push((true, cands.len())),
                        None => needs.push((false, 0)),
                    }
                } else {
                    needs.push((false, 0));
                }
            }
            confirm::flow_extra_rows(&needs)
        }
    };
    let plugin_id = std::env::var("HERDR_PLUGIN_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| PLUGIN_ID.to_string());
    let height = (lines.len() + 3 + slack).min(32);
    match client.call(
        "plugin.pane.open",
        serde_json::json!({
            "plugin_id": plugin_id, "entrypoint": "archive-confirm",
            "height": height,
            "env": {"ARCHIVE_TAB_ID": found.tab_id, "ARCHIVE_TAB_LABEL": found.label,
                    "ARCHIVE_TAB_TERMINALS": found.terminals.join(","), "ARCHIVE_CONFIRM_TEXT": lines.join("\n")},
        }),
    ) {
        Ok(_) => Ok(()),
        Err(e) if e.code == "ui_busy" => {
            notify(client, "herdr-archive: close the open popup or dialog first");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// One answer line from the popup's stdin. Returns Err on EOF/interrupt
/// so the caller can cancel quietly.
#[allow(clippy::result_unit_err)]
fn popup_input(prompt: &str) -> Result<String, ()> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => Err(()),
        Ok(_) => Ok(line),
        Err(_) => Err(()),
    }
}

fn popup_print(s: &str) {
    println!("{s}");
}

fn read_confirm_key() -> Result<u8, String> {
    if let Ok(text) = std::env::var("ARCHIVE_CONFIRM_TEXT") {
        println!("{text}");
        let _ = std::io::stdout().flush();
    }
    confirm::read_key().map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            "eof".to_string()
        } else {
            format!("read_key: {e}")
        }
    })
}

fn confirm_archive(
    client: &mut dyn herdr_archive::Herdr,
    session_state: &Path,
    session_name: &Option<String>,
) {
    let label = std::env::var("ARCHIVE_TAB_LABEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tab".to_string());
    let _quiet = LogsOffStderr::new();
    let key = read_confirm_key();
    let key = match key {
        Ok(k) => k,
        Err(which) if which == "eof" => return,
        Err(e) => {
            herdr_archive::log_error!("confirm-archive failed: {e}");
            notify(
                client,
                &format!("herdr-archive: can't archive \"{label}\": {e}"),
            );
            return;
        }
    };
    if key != b'y' && key != b'Y' {
        return;
    }
    let terminals: Vec<String> = std::env::var("ARCHIVE_TAB_TERMINALS")
        .unwrap_or_default()
        .split(',')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    let live: Option<Vec<Map<String, Value>>> = client
        .call("pane.list", Value::Object(Map::new()))
        .ok()
        .and_then(|v| {
            v.get("panes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|p| p.as_object().cloned()).collect())
        });
    let wanted: HashSet<String> = terminals.iter().cloned().collect();
    let panes: Vec<Map<String, Value>> = live
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            p.get("terminal_id")
                .and_then(Value::as_str)
                .is_some_and(|t| wanted.contains(t))
        })
        .collect();
    let mut overrides = BTreeMap::new();
    let mut provenance = BTreeMap::new();
    let panes_terms: HashSet<&str> = panes
        .iter()
        .filter_map(|p| p.get("terminal_id").and_then(Value::as_str))
        .collect();
    let wanted_terms: HashSet<&str> = wanted.iter().map(String::as_str).collect();
    // A stray Enter typed after the single-key confirm (habit, or while
    // detection runs) sits buffered on stdin, and the first line prompt
    // would consume it as "accept default" — the user never chooses. Drain
    // once before that first prompt (shared across the session picker and
    // the archive-name question, whichever runs first) so only keys typed
    // AT the prompt count.
    let mut first_prompt = true;
    let mut input = |prompt: &str| -> Result<String, ()> {
        if first_prompt {
            first_prompt = false;
            confirm::drain_stdin();
        }
        popup_input(prompt)
    };
    if live.is_some() && panes_terms == wanted_terms {
        let resolved = (|| -> Result<Option<manual::Resolved>, String> {
            let cfg =
                config::load(Some(config_dir().as_path()), true).map_err(|e| e.to_string())?;
            let mut print = popup_print;
            manual::resolve_missing_sessions_with_client(
                &panes,
                &agents::table(Some(&cfg.agents)),
                &mut input,
                &mut print,
                &mut *client,
            )
            .map_err(|_| "eof".to_string())
        })();
        match resolved {
            Err(which) if which == "eof" => return,
            Err(e) => {
                herdr_archive::log_error!("confirm-archive failed: {e}");
                notify(
                    client,
                    &format!("herdr-archive: can't archive \"{label}\": {e}"),
                );
                return;
            }
            Ok(None) => {
                notify(client, "herdr-archive: archive cancelled");
                return;
            }
            Ok(Some(r)) => {
                overrides = r.overrides;
                provenance = r.provenance;
            }
        }
    }
    let mut print = popup_print;
    let archive_name = match confirm::ask_name(&mut input, &mut print, &label) {
        Err(_) => return, // EOF/interrupt: cancel quietly
        Ok(None) => {
            notify(client, "herdr-archive: archive cancelled");
            return;
        }
        Ok(Some(name)) => name,
    };
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    print!(" Archiving...");
    let _ = std::io::stdout().flush();
    match (|| -> Result<String, MainError> {
        let cfg = config::load(Some(config_dir().as_path()), true)?;
        let table = agents::table(Some(&cfg.agents));
        let tab_id = std::env::var("ARCHIVE_TAB_ID").unwrap_or_default();
        let terms: HashSet<String> = terminals.into_iter().collect();
        Ok(sweep::archive_now(
            client,
            &cfg,
            session_state,
            &table,
            &tab_id,
            now(),
            display_session_name(session_name),
            Some(&terms),
            true,
            Some(&overrides),
            Some(&provenance),
            Some(&archive_name),
        )?)
    })() {
        Ok(_) => notify(
            client,
            &format!("herdr-archive: archived \"{archive_name}\""),
        ),
        Err(MainError::LockBusy(_)) => notify(
            client,
            "herdr-archive: a sweep is running; try again in a moment",
        ),
        Err(MainError::Skip(s)) => {
            herdr_archive::log_info!("confirm-archive: can't archive {label}: {s}");
            notify(
                client,
                &format!("herdr-archive: can't archive \"{label}\": {s}"),
            );
        }
        Err(e) => {
            herdr_archive::log_error!("confirm-archive failed: {e}");
            notify(
                client,
                &format!("herdr-archive: can't archive \"{label}\": {e}"),
            );
        }
    }
}

fn pick(state: &Path) -> i32 {
    let result = (|| -> Result<(), String> {
        let arch = Archive::new(state);
        let store = ActivityStore::new(state);
        let client = std::cell::RefCell::new(Client::from_env().map_err(|e| e.to_string())?);
        let do_restore =
            |archive_id: &str, target: restore::RestoreTarget| -> picker::RestoreOutcome {
                let cfg = match config::load(Some(config_dir().as_path()), true) {
                    Ok(c) => c,
                    Err(e) => return picker::RestoreOutcome::Failed(e.to_string()),
                };
                let table = agents::table(Some(&cfg.agents));
                let mut borrowed = client.borrow_mut();
                match restore::restore(
                    &mut *borrowed,
                    &arch,
                    &store,
                    archive_id,
                    &table,
                    now(),
                    target,
                ) {
                    Ok(r) => picker::RestoreOutcome::Ok(r.warnings),
                    Err(restore::RestoreError::Lock(LockError::Busy(_))) => {
                        picker::RestoreOutcome::Busy
                    }
                    Err(e) => picker::RestoreOutcome::Failed(e.to_string()),
                }
            };
        let workspace_live = |id: &str| -> bool {
            let mut borrowed = client.borrow_mut();
            restore::workspace_is_live(&mut *borrowed, id).unwrap_or(false)
        };
        let notify_fn = |title: &str, body: &str| {
            let mut borrowed = client.borrow_mut();
            if let Err(e) = borrowed.call(
                "notification.show",
                serde_json::json!({"title": title, "body": body}),
            ) {
                herdr_archive::log_warn!("notification failed: {e}");
            }
        };
        let _quiet = LogsOffStderr::new();
        picker_tty::run(&arch, &do_restore, &workspace_live, &now, &notify_fn).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                "eof".to_string()
            } else {
                e.to_string()
            }
        })
    })();
    match result {
        Ok(()) => 0,
        Err(which) if which == "eof" => 0,
        Err(e) => {
            herdr_archive::log_error!("pick failed: {e}");
            eprintln!("herdr-archive: {e}");
            let mut line = String::new();
            print!("Press Enter to close. ");
            let _ = std::io::stdout().flush();
            let _ = std::io::stdin().read_line(&mut line);
            0
        }
    }
}

fn pick_disabled(name: &Option<String>) -> i32 {
    eprintln!("{}", disabled_message(name));
    let mut line = String::new();
    print!("Press Enter to close. ");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().read_line(&mut line);
    0
}

fn dispatch(
    command: &str,
    args: &[String],
    state: &Path,
    name: &Option<String>,
) -> Result<i32, MainError> {
    let session_state: Option<PathBuf> = name.as_ref().map(|n| state.join("sessions").join(n));
    let mut allowed_cache: Option<bool> = None;
    let mut allowed = || -> bool {
        if allowed_cache.is_none() {
            allowed_cache = Some(session_allowed(name, &config_dir()));
        }
        allowed_cache.unwrap()
    };

    if command == "track" {
        if !allowed() {
            return Ok(0);
        }
        let session_state = session_state.unwrap();
        let mut client = Client::from_env()?;
        activity::track(
            &mut client,
            &ActivityStore::new(&session_state),
            std::env::var("HERDR_PLUGIN_EVENT_JSON").ok().as_deref(),
            std::env::var("HERDR_PANE_ID").ok().as_deref(),
            now(),
            std::env::var("HERDR_PLUGIN_EVENT").ok().as_deref(),
        )?;
        return Ok(0);
    }
    if command == "sweep" {
        let if_due = args.iter().any(|a| a == "--if-due");
        if if_due {
            if let Some(ref ss) = session_state {
                if !sweep::is_due(ss, config::DEFAULT_SWEEP_INTERVAL_MINUTES, now())? {
                    return Ok(0);
                }
            }
        }
        if migration_incomplete(state) {
            if if_due {
                herdr_archive::log_info!(
                    "sweep: migrating state from an older version; skipping this sweep"
                );
                return Ok(0);
            }
            eprintln!("{MIGRATING_MESSAGE}");
            return Ok(1);
        }
        if !allowed() {
            if if_due {
                return Ok(0);
            }
            eprintln!("{}", disabled_message(name));
            if let Ok(mut c) = Client::from_env() {
                notify_disabled(&mut c, name);
            }
            return Ok(1);
        }
        let session_state = session_state.unwrap();
        let cfg = config::load(Some(config_dir().as_path()), true)?;
        let mut client = Client::from_env()?;
        let report = sweep::run(
            &mut client,
            &cfg,
            &session_state,
            &agents::table(Some(&cfg.agents)),
            if_due,
            now(),
            display_session_name(name),
        )?;
        match report {
            None => {
                if !if_due {
                    println!("herdr-archive: another sweep is running");
                }
                Ok(0)
            }
            Some(r) => {
                if !if_due {
                    match sweep::summary(&r) {
                        Some(text) => println!("{text}"),
                        None => {
                            println!("herdr-archive: nothing to archive");
                            if let Err(e) = client.call(
                                "notification.show",
                                serde_json::json!({"title": "herdr-archive", "body": "herdr-archive: nothing to archive"}),
                            ) {
                                herdr_archive::log_warn!("notification failed: {e}");
                            }
                        }
                    }
                }
                Ok(0)
            }
        }
    } else if command == "open-picker" {
        let mut client = Client::from_env()?;
        if !allowed() {
            notify_disabled(&mut client, name);
            return Ok(0);
        }
        let plugin_id = std::env::var("HERDR_PLUGIN_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| PLUGIN_ID.to_string());
        match client.call(
            "plugin.pane.open",
            serde_json::json!({"plugin_id": plugin_id, "entrypoint": "picker"}),
        ) {
            Ok(_) => Ok(0),
            Err(e) if e.code == "ui_busy" => {
                let _ = client.call(
                    "notification.show",
                    serde_json::json!({"title": "herdr-archive", "body": "herdr-archive: close the open popup or dialog first"}),
                );
                Ok(0)
            }
            Err(e) => Err(e.into()),
        }
    } else if command == "open-archive" {
        let mut client = Client::from_env()?;
        if !allowed() {
            notify_disabled(&mut client, name);
            return Ok(0);
        }
        if migration_incomplete(state) {
            notify(&mut client, MIGRATING_MESSAGE);
            return Ok(0);
        }
        let session_state = session_state.unwrap();
        if let Err(e) = open_archive(&mut client, &session_state) {
            herdr_archive::log_error!("open-archive failed: {e}");
            notify(
                &mut client,
                &format!("herdr-archive: can't archive this tab: {e}"),
            );
        }
        Ok(0)
    } else if command == "confirm-archive" {
        let mut client = Client::from_env()?;
        if !allowed() {
            notify_disabled(&mut client, name);
            return Ok(0);
        }
        if migration_incomplete(state) {
            notify(&mut client, MIGRATING_MESSAGE);
            return Ok(0);
        }
        let session_state = session_state.unwrap();
        confirm_archive(&mut client, &session_state, name);
        Ok(0)
    } else if (command == "archive" || command == "restore") && args.is_empty() {
        eprintln!("{USAGE}");
        Ok(2)
    } else if command == "list" {
        if !allowed() {
            eprintln!("{}", disabled_message(name));
            return Ok(1);
        }
        let session_state = session_state.unwrap();
        let records = Archive::new(&session_state).list();
        if records.is_empty() {
            println!("No archived tabs.");
        }
        let moment = now();
        for rec in &records {
            println!(
                "{}  {}",
                rec.get("id").and_then(Value::as_str).unwrap_or("?"),
                describe(rec, moment)
            );
        }
        Ok(0)
    } else if command == "archive" {
        if migration_incomplete(state) {
            eprintln!("{MIGRATING_MESSAGE}");
            return Ok(1);
        }
        if !allowed() {
            eprintln!("{}", disabled_message(name));
            return Ok(1);
        }
        let session_state = session_state.unwrap();
        let cfg = config::load(Some(config_dir().as_path()), true)?;
        let table = agents::table(Some(&cfg.agents));
        let mut client = Client::from_env()?;
        let id = sweep::archive_now(
            &mut client,
            &cfg,
            &session_state,
            &table,
            &args[0],
            now(),
            display_session_name(name),
            None,
            false,
            None,
            None,
            None,
        )?;
        println!("{id}");
        Ok(0)
    } else if command == "restore" {
        if !allowed() {
            eprintln!("{}", disabled_message(name));
            return Ok(1);
        }
        let session_state = session_state.unwrap();
        let arch = Archive::new(&session_state);
        if arch.load(&args[0]).is_err() {
            eprintln!("no archived tab '{}'; see `herdr-archive list`", args[0]);
            return Ok(1);
        }
        let cfg = config::load(Some(config_dir().as_path()), true)?;
        let table = agents::table(Some(&cfg.agents));
        let store = ActivityStore::new(&session_state);
        let mut client = Client::from_env()?;
        let result = restore::restore(
            &mut client,
            &arch,
            &store,
            &args[0],
            &table,
            now(),
            restore::RestoreTarget::New,
        )?;
        for w in &result.warnings {
            println!("{w}");
        }
        println!("{}", result.tab_id.as_deref().unwrap_or(""));
        Ok(0)
    } else if command == "pick" {
        if !allowed() {
            return Ok(pick_disabled(name));
        }
        let session_state = session_state.unwrap();
        Ok(pick(&session_state))
    } else {
        eprintln!("{USAGE}");
        Ok(2)
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let command = argv.first().cloned().unwrap_or_default();
    let args = if argv.is_empty() {
        vec![]
    } else {
        argv[1..].to_vec()
    };
    let state = state_dir();
    let name = session_name();
    let hook = is_hook(&command, &args);
    herdr_archive::log::setup(&state, display_session_name(&name));
    migrate::merge_into_default_session(&state);
    // Hooks never fail loudly inside herdr — not even on a panic.
    let result = if hook {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch(&command, &args, &state, &name)
        })) {
            Ok(r) => r,
            Err(_) => {
                herdr_archive::log_error!("{command} failed");
                Ok(0)
            }
        }
    } else {
        dispatch(&command, &args, &state, &name)
    };
    let code = match result {
        Ok(code) => code,
        Err(MainError::Config(e)) => {
            let cmd = if command.is_empty() {
                "herdr-archive".to_string()
            } else {
                command.clone()
            };
            herdr_archive::log_error!("{cmd}: {e}");
            if hook || command == "sweep" {
                notify_config_error(&state, &e.to_string());
            }
            if hook { 0 } else { 1 }
        }
        Err(MainError::LockBusy(e)) => {
            if command == "restore" || command == "archive" {
                eprintln!("herdr-archive: a sweep is running; try again in a moment");
            } else {
                let cmd = if command.is_empty() {
                    "herdr-archive".to_string()
                } else {
                    command.clone()
                };
                herdr_archive::log_error!("{cmd}: {e}");
            }
            if hook { 0 } else { 1 }
        }
        Err(MainError::Herdr(e)) => {
            let cmd = if command.is_empty() {
                "herdr-archive".to_string()
            } else {
                command.clone()
            };
            herdr_archive::log_error!("{cmd}: {e}");
            if hook { 0 } else { 1 }
        }
        Err(MainError::Skip(e)) => {
            let cmd = if command.is_empty() {
                "herdr-archive".to_string()
            } else {
                command.clone()
            };
            herdr_archive::log_error!("{cmd}: {e}");
            if hook { 0 } else { 1 }
        }
        Err(MainError::Expected(e)) => {
            let cmd = if command.is_empty() {
                "herdr-archive".to_string()
            } else {
                command.clone()
            };
            herdr_archive::log_error!("{cmd} failed: {e}");
            if hook { 0 } else { 1 }
        }
    };
    std::process::exit(code);
}
