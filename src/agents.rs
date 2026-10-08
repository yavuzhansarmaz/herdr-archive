//! Per-agent resume arguments, and building the command that resumes a session.
//!
//! Port of `shelf/agents.py`, extended from 19 to all 24 herdr kinds
//! (DESIGN.md §6). Rows 1-18 mirror herdr's `src/agent_resume.rs `plan()`;
//! row 19 is the fork's muse entry (verified against the muse CLI);
//! rows 20-24 are new and **experimental** (U1-U5: none of the cline,
//! kiro, gemini, maki or amp CLIs exist on this machine, so the resume
//! flags below are sourced from agent docs/research and must be re-verified
//! against the real CLIs before release).

use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub program: String,
    pub resume: Vec<String>,
    pub strip: Vec<String>,
    pub strip_bare: Vec<String>,
    pub strip_subcommand: Option<String>,
    /// Second token of a two-word subcommand pair (e.g. amp's
    /// `threads continue <id>`), handled like `strip_subcommand`.
    /// EXPERIMENTAL (U5): unverified against the real CLI.
    pub strip_subcommand2: Option<(String, String)>,
    pub relaunch: Option<String>,
    /// Basename-prefix program match: set for muse, whose installed binary
    /// is versioned (`muse-bin-1.4.3-…`, `muse-bin-1.4.4-…`), so launch and
    /// process detection must accept `muse*`, not just exactly `muse`.
    /// Relaunch still uses the plain `program` (`muse` resolves on PATH).
    pub prefix_match: bool,
}

impl Entry {
    fn new(program: &str, resume: &[&str]) -> Self {
        Entry {
            program: program.to_string(),
            resume: resume.iter().map(|s| s.to_string()).collect(),
            strip: Vec::new(),
            strip_bare: Vec::new(),
            strip_subcommand: None,
            strip_subcommand2: None,
            relaunch: None,
            prefix_match: false,
        }
    }
}

fn builtin_entries() -> Vec<(&'static str, Entry)> {
    let mut v: Vec<(&'static str, Entry)> = vec![
        ("claude", {
            let mut e = Entry::new("claude", &["--resume", "{id}"]);
            e.strip = vec!["-r".into(), "--session-id".into()];
            e.strip_bare = vec!["--continue".into(), "-c".into(), "--fork-session".into()];
            e
        }),
        ("codex", {
            let mut e = Entry::new("codex", &["resume", "{id}"]);
            e.strip_subcommand = Some("resume".into());
            e.strip_bare = vec!["--last".into(), "--all".into()];
            e
        }),
        ("copilot", Entry::new("copilot", &["--resume={id}"])),
        ("devin", Entry::new("devin", &["--resume", "{id}"])),
        ("droid", Entry::new("droid", &["--resume", "{id}"])),
        ("kimi", Entry::new("kimi", &["--session", "{id}"])),
        (
            "mastracode",
            Entry::new("mastracode", &["--thread", "{id}"]),
        ),
        ("pi", Entry::new("pi", &["--session", "{id}"])),
        ("omp", {
            let mut e = Entry::new("omp", &["--resume={id}"]);
            e.strip = vec!["-r".into()];
            e
        }),
        ("hermes", Entry::new("hermes", &["--resume", "{id}"])),
        ("opencode", Entry::new("opencode", &["--session", "{id}"])),
        ("qodercli", Entry::new("qodercli", &["--resume", "{id}"])),
        ("qwen", Entry::new("qwen", &["--resume", "{id}"])),
        ("kilo", Entry::new("kilo", &["--session", "{id}"])),
        ("cursor", Entry::new("cursor-agent", &["--resume", "{id}"])),
        ("agy", Entry::new("agy", &["--conversation", "{id}"])),
        ("grok", Entry::new("grok", &["--resume", "{id}"])),
        ("letta", Entry::new("letta", &["--conversation", "{id}"])),
        ("muse", {
            let mut e = Entry::new("muse", &["resume", "{id}"]);
            e.strip_subcommand = Some("resume".into());
            e.prefix_match = true;
            e
        }),
        // --- New rows (experimental, U1-U5) ---
        // U3: gemini --resume flag + session JSON schema unverified.
        ("gemini", Entry::new("gemini", &["--resume", "{id}"])),
        // U2: kiro-cli chat --resume-id flag unverified.
        (
            "kiro",
            Entry::new("kiro-cli", &["chat", "--resume-id", "{id}"]),
        ),
        // U5: amp threads continue form unverified.
        ("amp", {
            let mut e = Entry::new("amp", &["threads", "continue", "{id}"]);
            e.strip_subcommand2 = Some(("threads".into(), "continue".into()));
            e
        }),
        // U4: maki --resume vs --session choice unverified.
        ("maki", Entry::new("maki", &["--resume", "{id}"])),
    ];
    // U1: cline has no verified resume form at all. The row exists so the
    // table covers all 24 kinds and config overrides can patch it, but its
    // resume template is a placeholder guess: `cline resume {id}`.
    // EXPERIMENTAL.
    let mut cline = Entry::new("cline", &["resume", "{id}"]);
    cline.strip_subcommand = Some("resume".into());
    v.push(("cline", cline));
    v
}

/// The built-in table (fresh copy each call; overrides never mutate it).
pub fn builtin() -> BTreeMap<String, Entry> {
    builtin_entries()
        .into_iter()
        .map(|(k, e)| (k.to_string(), e))
        .collect()
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v.and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
        None => Vec::new(),
    }
}

fn entry_from_value(v: &Value) -> Entry {
    let o = v.as_object();
    Entry {
        program: o
            .and_then(|m| m.get("program"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        resume: str_list(o.and_then(|m| m.get("resume"))),
        strip: str_list(o.and_then(|m| m.get("strip"))),
        strip_bare: str_list(o.and_then(|m| m.get("strip_bare"))),
        strip_subcommand: o
            .and_then(|m| m.get("strip_subcommand"))
            .and_then(Value::as_str)
            .map(str::to_string),
        strip_subcommand2: None,
        relaunch: o
            .and_then(|m| m.get("relaunch"))
            .and_then(Value::as_str)
            .map(str::to_string),
        prefix_match: false,
    }
}

/// The built-in table with config overrides merged per agent. Entries
/// without both a program and resume arguments are dropped.
pub fn table(overrides: Option<&Map<String, Value>>) -> BTreeMap<String, Entry> {
    let mut merged = builtin();
    if let Some(ov) = overrides {
        for (name, value) in ov {
            let o = value.as_object().cloned().unwrap_or_default();
            let mut base = merged.get(name).cloned().unwrap_or(Entry {
                program: String::new(),
                resume: Vec::new(),
                strip: Vec::new(),
                strip_bare: Vec::new(),
                strip_subcommand: None,
                strip_subcommand2: None,
                relaunch: None,
                prefix_match: false,
            });
            // Preserve strip_subcommand2 through overrides (no config key
            // names it, so only the built-in amp pair can set it).
            let pair = base.strip_subcommand2.clone();
            // Same for prefix_match (only the built-in muse row sets it).
            let prefix = base.prefix_match;
            let patch = entry_from_value(value);
            if o.contains_key("program") {
                base.program = patch.program;
            }
            if o.contains_key("resume") {
                base.resume = patch.resume;
            }
            if o.contains_key("strip") {
                base.strip = patch.strip;
            }
            if o.contains_key("strip_bare") {
                base.strip_bare = patch.strip_bare;
            }
            if o.contains_key("strip_subcommand") {
                base.strip_subcommand = patch.strip_subcommand;
            }
            if o.contains_key("relaunch") {
                base.relaunch = patch.relaunch;
            }
            base.strip_subcommand2 = pair;
            base.prefix_match = prefix;
            merged.insert(name.clone(), base);
        }
    }
    merged
        .into_iter()
        .filter(|(_, e)| !e.program.is_empty() && !e.resume.is_empty())
        .collect()
}

pub fn resume_args(agent: &str, entry: &Entry, session_value: &str) -> Vec<String> {
    if agent == "letta" && session_value.starts_with("default:") {
        return vec![
            "--conversation".to_string(),
            "default".to_string(),
            "--agent".to_string(),
            session_value["default:".len()..].to_string(),
        ];
    }
    entry
        .resume
        .iter()
        .map(|p| p.replace("{id}", session_value))
        .collect()
}

fn own_flag(entry: &Entry) -> Option<String> {
    let first = entry.resume.first()?;
    if !first.starts_with('-') {
        return None;
    }
    Some(first.split('=').next().unwrap_or(first).to_string())
}

/// Remove any previous resume or continue arguments. `argv[0]` is never
/// touched. A value-taking flag (or a strip_subcommand) only consumes the
/// following token when one exists and it does not itself look like a flag.
pub fn strip_resume(argv: &[String], entry: &Entry) -> Vec<String> {
    let mut value_flags: HashSet<&str> = entry.strip.iter().map(String::as_str).collect();
    let own = own_flag(entry);
    if let Some(o) = &own {
        value_flags.insert(o.as_str());
    }
    let bare: HashSet<&str> = entry.strip_bare.iter().map(String::as_str).collect();
    let mut out: Vec<String> = argv.iter().take(1).cloned().collect();
    let mut i = 1;
    while i < argv.len() {
        let tok = &argv[i];
        if value_flags.contains(tok.as_str())
            || entry.strip_subcommand.as_ref().is_some_and(|s| s == tok)
        {
            let has_value = i + 1 < argv.len() && !argv[i + 1].starts_with('-');
            i += if has_value { 2 } else { 1 };
            continue;
        }
        // Two-word subcommand pair (amp: `threads continue <id>`).
        if let Some((w1, w2)) = &entry.strip_subcommand2 {
            if tok == w1 && argv.get(i + 1).is_some_and(|n| n == w2) {
                let has_value = i + 2 < argv.len() && !argv[i + 2].starts_with('-');
                i += if has_value { 3 } else { 2 };
                continue;
            }
        }
        if bare.contains(tok.as_str())
            || value_flags
                .iter()
                .any(|flag| tok.starts_with(&format!("{flag}=")))
        {
            i += 1;
            continue;
        }
        out.push(tok.clone());
        i += 1;
    }
    out
}

// Agents whose session values can be filesystem paths need more room than
// the default cap.
fn long_value_agents() -> HashSet<&'static str> {
    HashSet::from(["pi", "omp"])
}

const DEFAULT_MAX_VALUE_LENGTH: usize = 512;
const PATH_MAX_VALUE_LENGTH: usize = 4096;

/// Whether value is safe to substitute into a relaunch command.
pub fn valid_session_value(agent: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let max = if long_value_agents().contains(agent) {
        PATH_MAX_VALUE_LENGTH
    } else {
        DEFAULT_MAX_VALUE_LENGTH
    };
    // Character count, like Python's len() — not bytes.
    if value.chars().count() > max {
        return false;
    }
    if value
        .chars()
        .any(|c| (c as u32) < 32 || (127..=0x9F).contains(&(c as u32)))
    {
        return false;
    }
    if value.starts_with('-') {
        return false;
    }
    if agent == "letta" && value.starts_with("default:") {
        return value.len() > "default:".len();
    }
    true
}

fn looks_like_a_prompt(tokens: &[String]) -> bool {
    tokens
        .iter()
        .any(|tok| tok == "--" || tok.chars().any(|ch| ch.is_whitespace()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSession(pub String);

impl std::fmt::Display for InvalidSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InvalidSession {}

pub fn relaunch_argv(
    agent: &str,
    entry: &Entry,
    session_value: &str,
    launch_argv: Option<&[String]>,
) -> Result<Vec<String>, InvalidSession> {
    if !valid_session_value(agent, session_value) {
        return Err(InvalidSession(format!(
            "invalid session value for {agent:?}: {session_value:?}"
        )));
    }
    let args = resume_args(agent, entry, session_value);
    let empty = launch_argv.is_none_or(|a| a.is_empty());
    if entry.relaunch.as_deref() == Some("plain") || empty {
        let mut out = vec![entry.program.clone()];
        out.extend(args);
        return Ok(out);
    }
    let launch = launch_argv.unwrap();
    let mut stripped = strip_resume(launch, entry);
    if !stripped.is_empty() && matches_program(entry, &stripped[0]) {
        stripped[0] = entry.program.clone();
    }
    if looks_like_a_prompt(&stripped[1.min(stripped.len())..]) {
        crate::log_warn!(
            "saved command for {agent} looked like it carried a prompt; using a plain relaunch"
        );
        let mut out = vec![entry.program.clone()];
        out.extend(args);
        return Ok(out);
    }
    stripped.extend(args);
    Ok(stripped)
}

pub fn matches_program(entry: &Entry, argv0: &str) -> bool {
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    base == entry.program
        || (!entry.program.is_empty()
            && entry.prefix_match
            && base.starts_with(entry.program.as_str()))
}

/// POSIX-quote one argv element (shlex.quote semantics).
pub fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'@' | b'%' | b'_' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
            )
    });
    if safe {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Run argv, then leave an interactive shell in the pane when it exits.
pub fn shell_command(argv: &[String]) -> Vec<String> {
    let joined = argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("trap : INT; {joined}; exec \"${{SHELL:-sh}}\""),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn expected_plain() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("pi", vec!["pi", "--session", "S"]),
            ("claude", vec!["claude", "--resume", "S"]),
            ("codex", vec!["codex", "resume", "S"]),
            ("cursor", vec!["cursor-agent", "--resume", "S"]),
            ("devin", vec!["devin", "--resume", "S"]),
            ("agy", vec!["agy", "--conversation", "S"]),
            ("omp", vec!["omp", "--resume=S"]),
            ("mastracode", vec!["mastracode", "--thread", "S"]),
            ("opencode", vec!["opencode", "--session", "S"]),
            ("copilot", vec!["copilot", "--resume=S"]),
            ("kimi", vec!["kimi", "--session", "S"]),
            ("droid", vec!["droid", "--resume", "S"]),
            ("grok", vec!["grok", "--resume", "S"]),
            ("hermes", vec!["hermes", "--resume", "S"]),
            ("kilo", vec!["kilo", "--session", "S"]),
            ("qodercli", vec!["qodercli", "--resume", "S"]),
            ("qwen", vec!["qwen", "--resume", "S"]),
            ("letta", vec!["letta", "--conversation", "S"]),
            ("muse", vec!["muse", "resume", "S"]),
            ("gemini", vec!["gemini", "--resume", "S"]),
            ("cline", vec!["cline", "resume", "S"]),
            ("kiro", vec!["kiro-cli", "chat", "--resume-id", "S"]),
            ("amp", vec!["amp", "threads", "continue", "S"]),
            ("maki", vec!["maki", "--resume", "S"]),
        ]
    }

    #[test]
    fn every_builtin_plain_relaunch_matches_herdr() {
        let t = table(None);
        let exp = expected_plain();
        assert_eq!(t.len(), exp.len());
        for (name, argv) in &exp {
            let entry = t.get(*name).unwrap_or_else(|| panic!("missing {name}"));
            let got = relaunch_argv(name, entry, "S", None).unwrap();
            let want: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, want, "agent {name}");
        }
    }

    #[test]
    fn letta_default_agent_form() {
        let t = table(None);
        assert_eq!(
            relaunch_argv("letta", &t["letta"], "default:agent-7", None).unwrap(),
            vec!["letta", "--conversation", "default", "--agent", "agent-7"]
        );
    }

    #[test]
    fn override_merges_adds_and_drops_incomplete() {
        let ov = json!({
            "claude": {"relaunch": "plain"},
            "myagent": {"program": "myagent", "resume": ["--load", "{id}"]},
            "broken": {"relaunch": "plain"},
        });
        let t = table(ov.as_object());
        assert_eq!(t["claude"].program, "claude");
        assert_eq!(t["claude"].relaunch.as_deref(), Some("plain"));
        let saved = ["myagent".to_string(), "-v".to_string()];
        assert_eq!(
            relaunch_argv("myagent", &t["myagent"], "S", Some(&saved)).unwrap(),
            vec!["myagent", "-v", "--load", "S"]
        );
        assert!(!t.contains_key("broken"));
    }

    #[test]
    fn builtin_not_mutated_by_override() {
        let ov = json!({"claude": {"program": "other"}});
        table(ov.as_object());
        assert_eq!(builtin().get("claude").unwrap().program, "claude");
    }

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keeps_flags_and_strips_old_resume() {
        let t = table(None);
        let saved = sv(&[
            "/usr/local/bin/claude",
            "--agent",
            "reviewer",
            "--resume",
            "OLD",
            "--effort",
            "max",
        ]);
        assert_eq!(
            relaunch_argv("claude", &t["claude"], "NEW", Some(&saved)).unwrap(),
            sv(&[
                "claude", "--agent", "reviewer", "--effort", "max", "--resume", "NEW"
            ])
        );
    }

    #[test]
    fn strips_equals_form_aliases_and_bare_flags() {
        let t = table(None);
        let saved = sv(&[
            "claude",
            "--resume=OLD",
            "-r",
            "OLD2",
            "-c",
            "--continue",
            "--model",
            "opus",
        ]);
        assert_eq!(
            relaunch_argv("claude", &t["claude"], "NEW", Some(&saved)).unwrap(),
            sv(&["claude", "--model", "opus", "--resume", "NEW"])
        );
    }

    #[test]
    fn codex_subcommand_removed() {
        let t = table(None);
        let saved = sv(&["codex", "--model", "o4", "resume", "OLD"]);
        assert_eq!(
            relaunch_argv("codex", &t["codex"], "NEW", Some(&saved)).unwrap(),
            sv(&["codex", "--model", "o4", "resume", "NEW"])
        );
    }

    #[test]
    fn muse_subcommand_removed() {
        let t = table(None);
        let saved = sv(&["muse", "resume", "OLD"]);
        assert_eq!(
            relaunch_argv("muse", &t["muse"], "NEW", Some(&saved)).unwrap(),
            sv(&["muse", "resume", "NEW"])
        );
    }

    #[test]
    fn codex_strips_last_and_all_bare_flags() {
        let t = table(None);
        assert_eq!(
            relaunch_argv(
                "codex",
                &t["codex"],
                "NEW",
                Some(&sv(&["codex", "resume", "--last"]))
            )
            .unwrap(),
            sv(&["codex", "resume", "NEW"])
        );
        let saved = sv(&["codex", "--model", "o4", "resume", "--all"]);
        assert_eq!(
            relaunch_argv("codex", &t["codex"], "NEW", Some(&saved)).unwrap(),
            sv(&["codex", "--model", "o4", "resume", "NEW"])
        );
    }

    #[test]
    fn equals_template_strips_both_forms() {
        let t = table(None);
        let saved = sv(&["omp", "--resume", "OLD", "-r", "OLD2", "--fast"]);
        assert_eq!(
            relaunch_argv("omp", &t["omp"], "NEW", Some(&saved)).unwrap(),
            sv(&["omp", "--fast", "--resume=NEW"])
        );
    }

    #[test]
    fn amp_pair_stripped() {
        let t = table(None);
        let saved = sv(&["amp", "--theme", "dark", "threads", "continue", "T-OLD"]);
        assert_eq!(
            relaunch_argv("amp", &t["amp"], "T-NEW", Some(&saved)).unwrap(),
            sv(&["amp", "--theme", "dark", "threads", "continue", "T-NEW"])
        );
    }

    #[test]
    fn argv0_never_stripped() {
        let t = table(None);
        assert_eq!(
            strip_resume(&sv(&["-c", "x"]), &t["claude"]),
            sv(&["-c", "x"])
        );
    }

    #[test]
    fn value_validation() {
        assert!(valid_session_value("claude", "abc-123"));
        assert!(!valid_session_value("claude", ""));
        assert!(!valid_session_value("claude", "-x"));
        assert!(!valid_session_value("claude", "a\nb"));
        assert!(!valid_session_value("claude", &"x".repeat(513)));
        assert!(valid_session_value("pi", &"x".repeat(4096)));
        assert!(!valid_session_value("pi", &"x".repeat(4097)));
        assert!(!valid_session_value("letta", "default:"));
        assert!(valid_session_value("letta", "default:a"));
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(
            shell_command(&sv(&["claude", "--resume", "a b"])),
            vec![
                "sh",
                "-c",
                "trap : INT; claude --resume 'a b'; exec \"${SHELL:-sh}\""
            ]
        );
    }

    #[test]
    fn prompt_guard_uses_plain() {
        let t = table(None);
        let saved = sv(&["claude", "fix the build"]);
        assert_eq!(
            relaunch_argv("claude", &t["claude"], "NEW", Some(&saved)).unwrap(),
            sv(&["claude", "--resume", "NEW"])
        );
        let dd = sv(&["claude", "--", "foo"]);
        assert_eq!(
            relaunch_argv("claude", &t["claude"], "NEW", Some(&dd)).unwrap(),
            sv(&["claude", "--resume", "NEW"])
        );
    }

    #[test]
    fn optional_value_not_swallowed() {
        let t = table(None);
        let saved = sv(&["claude", "-r", "--effort", "max"]);
        assert_eq!(
            relaunch_argv("claude", &t["claude"], "NEW", Some(&saved)).unwrap(),
            sv(&["claude", "--effort", "max", "--resume", "NEW"])
        );
        let saved = sv(&["codex", "resume", "-m", "o3"]);
        assert_eq!(
            relaunch_argv("codex", &t["codex"], "NEW", Some(&saved)).unwrap(),
            sv(&["codex", "-m", "o3", "resume", "NEW"])
        );
    }

    #[test]
    fn argv0_rewrite() {
        let t = table(None);
        let saved = sv(&["/opt/versioned/claude", "--model", "opus"]);
        assert_eq!(
            relaunch_argv("claude", &t["claude"], "NEW", Some(&saved)).unwrap(),
            sv(&["claude", "--model", "opus", "--resume", "NEW"])
        );
        let saved = sv(&["node", "/opt/claude-cli.js", "--model", "opus"]);
        assert_eq!(
            relaunch_argv("claude", &t["claude"], "NEW", Some(&saved)).unwrap(),
            sv(&[
                "node",
                "/opt/claude-cli.js",
                "--model",
                "opus",
                "--resume",
                "NEW"
            ])
        );
    }

    #[test]
    fn quoting_roundtrip_through_sh() {
        let argv = sv(&["printf", "%s|", "it's", "a b", "\"q\"", "$HOME"]);
        let cmd = shell_command(&argv);
        assert_eq!(cmd[..2], ["sh".to_string(), "-c".to_string()]);
        assert!(cmd[2].starts_with("trap : INT; "));
        assert!(cmd[2].ends_with("; exec \"${SHELL:-sh}\""));
        let script = &cmd[2]["trap : INT; ".len()..cmd[2].len() - "; exec \"${SHELL:-sh}\"".len()];
        let out = std::process::Command::new("sh")
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "it's|a b|\"q\"|$HOME|"
        );
    }

    #[test]
    fn invalid_value_rejected() {
        let t = table(None);
        assert!(relaunch_argv("claude", &t["claude"], "-rf", None).is_err());
        assert!(relaunch_argv("claude", &t["claude"], "", None).is_err());
    }

    #[test]
    fn muse_matches_versioned_binaries() {
        let t = table(None);
        let muse = &t["muse"];
        assert!(matches_program(muse, "muse"));
        // the installed versioned binaries (1.4.3 and 1.4.4 seen live)
        assert!(matches_program(muse, "muse-bin-1.4.3-R5410.2"));
        assert!(matches_program(muse, "muse-bin-1.4.4-R5419.1"));
        assert!(matches_program(
            muse,
            "/home/u/.local/bin/muse-bin-1.4.4-R5419.1"
        ));
        assert!(!matches_program(muse, "other"));
        assert!(!matches_program(muse, ""));
    }

    #[test]
    fn other_kinds_still_match_exactly() {
        let t = table(None);
        assert!(matches_program(&t["claude"], "/usr/bin/claude"));
        assert!(!matches_program(&t["claude"], "claude-bin-2.0"));
        assert!(!matches_program(&t["codex"], "codex-resume"));
        assert!(!matches_program(&t["codex"], "codex2"));
    }

    #[test]
    fn muse_prefix_survives_overrides_relaunch_stays_plain() {
        let ov = json!({"muse": {"relaunch": "plain"}});
        let t = table(ov.as_object());
        assert!(matches_program(&t["muse"], "muse-bin-1.4.4-R5419.1"));
        // relaunch keeps the plain program (`muse` resolves on PATH)
        assert_eq!(
            relaunch_argv("muse", &t["muse"], "S", None).unwrap(),
            sv(&["muse", "resume", "S"])
        );
        // ...and a versioned saved argv0 rewrites to it
        let saved = sv(&["/home/u/.local/bin/muse-bin-1.4.4-R5419.1", "resume", "OLD"]);
        assert_eq!(
            relaunch_argv("muse", &t["muse"], "NEW", Some(&saved)).unwrap(),
            sv(&["muse", "resume", "NEW"])
        );
    }
}
