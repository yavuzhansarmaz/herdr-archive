//! Load config.json from the plugin's config directory.
//!
//! Port of `shelf/config.py`. Same file format, same defaults, same
//! validation messages.

use serde_json::{Map, Value};
use std::fmt;
use std::path::Path;

pub const ALL_SESSIONS: &str = "*";
pub const MAX_IDLE_DAYS: f64 = 3650.0;
pub const MAX_SWEEP_INTERVAL_MINUTES: f64 = 10080.0;
pub const DEFAULT_IDLE_DAYS: f64 = 7.0;
pub const DEFAULT_SWEEP_INTERVAL_MINUTES: f64 = 60.0;

pub const AGENT_KEYS: &[&str] = &[
    "program",
    "resume",
    "strip",
    "strip_bare",
    "strip_subcommand",
    "relaunch",
];

#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone)]
pub struct Config {
    pub idle_days: f64,
    pub mode: String,
    pub sweep_interval_minutes: f64,
    pub keep_transcripts: bool,
    pub agents: Map<String, Value>,
    pub sessions: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            idle_days: DEFAULT_IDLE_DAYS,
            mode: "dry-run".to_string(),
            sweep_interval_minutes: DEFAULT_SWEEP_INTERVAL_MINUTES,
            keep_transcripts: true,
            agents: Map::new(),
            sessions: vec!["default".to_string()],
        }
    }
}

fn is_number(v: &Value) -> bool {
    v.is_i64() || v.is_u64() || v.is_f64()
}

fn as_f64(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_i64() {
        return Some(n as f64);
    }
    if let Some(n) = v.as_u64() {
        return Some(n as f64);
    }
    v.as_f64()
}

/// Load config.json, applying defaults for missing keys. `warn=false`
/// suppresses unknown-key warnings.
pub fn load(config_dir: Option<&Path>, warn: bool) -> Result<Config, ConfigError> {
    let mut cfg = Config::default();
    let Some(dir) = config_dir else {
        return Ok(cfg);
    };
    let path = dir.join("config.json");
    let text = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(cfg),
        Err(e) => return Err(ConfigError(format!("{}: {e}", path.display()))),
        Ok(t) => t,
    };
    let raw: Value =
        serde_json::from_str(&text).map_err(|e| ConfigError(format!("{}: {e}", path.display())))?;
    let top = match raw.as_object() {
        Some(o) => o,
        None => {
            return Err(ConfigError(format!(
                "{}: the top level must be an object",
                path.display()
            )));
        }
    };
    let known = [
        "idle_days",
        "mode",
        "sweep_interval_minutes",
        "keep_transcripts",
        "agents",
        "sessions",
    ];
    let mut unknown: Vec<&str> = top
        .keys()
        .filter(|k| !known.contains(&k.as_str()))
        .map(String::as_str)
        .collect();
    unknown.sort();
    if !unknown.is_empty() && warn {
        crate::log_warn!(
            "{}: ignoring unknown key(s): {}",
            path.display(),
            unknown.join(", ")
        );
    }
    // Validated in Python's exact order so the first reported error matches:
    // idle_days, mode, sweep_interval_minutes, keep_transcripts, agents, sessions.
    if let Some(v) = top.get("idle_days") {
        cfg.idle_days = validated_number(&path, "idle_days", v)?;
    }
    if cfg.idle_days.is_nan() || !cfg.idle_days.is_finite() || cfg.idle_days <= 0.0 {
        return Err(ConfigError(format!(
            "{}: idle_days must be a positive number",
            path.display()
        )));
    }
    if cfg.idle_days > MAX_IDLE_DAYS {
        return Err(ConfigError(format!(
            "{}: idle_days must be at most {MAX_IDLE_DAYS}",
            path.display()
        )));
    }
    if let Some(v) = top.get("mode") {
        cfg.mode = v.as_str().unwrap_or("\u{0}").to_string();
    }
    if cfg.mode != "dry-run" && cfg.mode != "live" {
        return Err(ConfigError(format!(
            "{}: mode must be \"dry-run\" or \"live\"",
            path.display()
        )));
    }
    if let Some(v) = top.get("sweep_interval_minutes") {
        cfg.sweep_interval_minutes = validated_number(&path, "sweep_interval_minutes", v)?;
    }
    if !cfg.sweep_interval_minutes.is_finite() || cfg.sweep_interval_minutes < 0.0 {
        return Err(ConfigError(format!(
            "{}: sweep_interval_minutes must be zero or more",
            path.display()
        )));
    }
    if cfg.sweep_interval_minutes > MAX_SWEEP_INTERVAL_MINUTES {
        return Err(ConfigError(format!(
            "{}: sweep_interval_minutes must be at most {MAX_SWEEP_INTERVAL_MINUTES}",
            path.display()
        )));
    }
    if let Some(v) = top.get("keep_transcripts") {
        match v.as_bool() {
            Some(b) => cfg.keep_transcripts = b,
            None => {
                return Err(ConfigError(format!(
                    "{}: keep_transcripts must be true or false",
                    path.display()
                )));
            }
        }
    }
    if let Some(v) = top.get("agents") {
        match v.as_object() {
            Some(o) if o.values().all(Value::is_object) => cfg.agents = o.clone(),
            _ => {
                return Err(ConfigError(format!(
                    "{}: agents must map agent names to objects",
                    path.display()
                )));
            }
        }
        for (name, entry) in &cfg.agents {
            validate_agent(name, entry.as_object().unwrap(), &path, warn)?;
        }
    }
    if let Some(v) = top.get("sessions") {
        if !is_valid_sessions_value(v) {
            return Err(ConfigError(format!(
                "{}: sessions must be a non-empty list of non-empty strings",
                path.display()
            )));
        }
        cfg.sessions = v
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
    }
    Ok(cfg)
}

fn validated_number(path: &Path, key: &str, v: &Value) -> Result<f64, ConfigError> {
    match as_f64(v).filter(|_| is_number(v)) {
        Some(n) => Ok(n),
        None => {
            let msg = if key == "idle_days" {
                "idle_days must be a positive number"
            } else {
                "sweep_interval_minutes must be zero or more"
            };
            Err(ConfigError(format!("{}: {msg}", path.display())))
        }
    }
}

fn is_valid_sessions_value(v: &Value) -> bool {
    match v.as_array() {
        Some(a) => !a.is_empty() && a.iter().all(|s| s.as_str().is_some_and(|s| !s.is_empty())),
        None => false,
    }
}

fn is_str_list(v: &Value) -> bool {
    v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
}

fn validate_agent(
    name: &str,
    entry: &Map<String, Value>,
    path: &Path,
    warn: bool,
) -> Result<(), ConfigError> {
    let mut unknown: Vec<&str> = entry
        .keys()
        .filter(|k| !AGENT_KEYS.contains(&k.as_str()))
        .map(String::as_str)
        .collect();
    unknown.sort();
    if !unknown.is_empty() && warn {
        crate::log_warn!(
            "{}: agents.{name}: ignoring unknown key(s): {}",
            path.display(),
            unknown.join(", ")
        );
    }
    if let Some(p) = entry.get("program") {
        if p.as_str().is_none_or(str::is_empty) {
            return Err(ConfigError(format!(
                "{}: agents.{name}.program must be a non-empty string",
                path.display()
            )));
        }
    }
    if let Some(r) = entry.get("resume") {
        if !(is_str_list(r) && !r.as_array().unwrap().is_empty()) {
            return Err(ConfigError(format!(
                "{}: agents.{name}.resume must be a non-empty list of strings",
                path.display()
            )));
        }
        if !r
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().contains("{id}"))
        {
            return Err(ConfigError(format!(
                "{}: agents.{name}.resume must contain \"{{id}}\" in at least one element",
                path.display()
            )));
        }
    }
    if let Some(s) = entry.get("strip") {
        if !is_str_list(s) {
            return Err(ConfigError(format!(
                "{}: agents.{name}.strip must be a list of strings",
                path.display()
            )));
        }
    }
    if let Some(s) = entry.get("strip_bare") {
        if !is_str_list(s) {
            return Err(ConfigError(format!(
                "{}: agents.{name}.strip_bare must be a list of strings",
                path.display()
            )));
        }
    }
    if let Some(s) = entry.get("strip_subcommand") {
        if !s.is_string() {
            return Err(ConfigError(format!(
                "{}: agents.{name}.strip_subcommand must be a string",
                path.display()
            )));
        }
    }
    if let Some(r) = entry.get("relaunch") {
        if r.as_str() != Some("plain") {
            return Err(ConfigError(format!(
                "{}: agents.{name}.relaunch must be \"plain\"",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Whether Archive should act in the herdr session called `name`.
pub fn session_enabled(sessions: &[String], name: &str) -> bool {
    if sessions.is_empty() {
        return name == "default";
    }
    sessions.iter().any(|s| s == ALL_SESSIONS) || sessions.iter().any(|s| s == name)
}

/// The `sessions` list for the allowlist gate alone. Never fails, never logs:
/// valid sessions are used even when the rest of the file is broken.
pub fn sessions_for_gate(config_dir: Option<&Path>) -> Vec<String> {
    let default = vec!["default".to_string()];
    let Some(dir) = config_dir else {
        return default;
    };
    let text = match std::fs::read_to_string(dir.join("config.json")) {
        Ok(t) => t,
        Err(_) => return default,
    };
    let raw: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return default,
    };
    let top = match raw.as_object() {
        Some(o) => o,
        None => return default,
    };
    match top.get("sessions") {
        Some(v) if is_valid_sessions_value(v) => v
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect(),
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let cfg = load(None, true).unwrap();
        assert_eq!(cfg.idle_days, 7.0);
        assert_eq!(cfg.mode, "dry-run");
        assert_eq!(cfg.sweep_interval_minutes, 60.0);
        assert!(cfg.keep_transcripts);
        assert!(cfg.agents.is_empty());
        assert_eq!(cfg.sessions, vec!["default".to_string()]);
    }

    #[test]
    fn session_enabled_matrix() {
        assert!(session_enabled(&["default".to_string()], "default"));
        assert!(!session_enabled(&["default".to_string()], "other"));
        assert!(session_enabled(&["*".to_string()], "anything"));
        assert!(session_enabled(&[], "default"));
        assert!(!session_enabled(&[], "other"));
    }

    #[test]
    fn gate_never_fails() {
        assert_eq!(sessions_for_gate(None), vec!["default".to_string()]);
        assert_eq!(
            sessions_for_gate(Some(Path::new("/nonexistent-xyz"))),
            vec!["default".to_string()]
        );
    }
}
