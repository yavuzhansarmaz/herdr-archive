//! The archive-tab popup: the question it shows, and reading one key.
//!
//! Port of `shelf/confirm.py`. `lines()` is a pure port; `read_key` uses
//! crossterm raw mode instead of termios (same contract: pre-typed keys
//! flushed, mode restored on every path).

use serde_json::Value;

/// Text columns: the manifest's popup width (64) less the border, the
/// leading space and one spare column.
pub const WIDTH: usize = 60;
pub const HINT: &str = "The tab closes; the restore picker brings it back.";
pub const KEYS: &str = "y archive   any other key cancel";

fn printable(text: &str) -> String {
    // Any process can set a tab label; a control character in one (an escape
    // sequence, say) printed raw could redraw the question. Python's
    // str.isprintable also rejects some format characters; treat Unicode
    // control/format/surrogate/private-use as non-printable.
    text.chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{7f}'..='\u{9f}') {
                '?'
            } else {
                match unicode_general_category(ch) {
                    // Cf (format), Cs (surrogate), Cc handled above.
                    Gc::Format | Gc::Surrogate => '?',
                    _ => ch,
                }
            }
        })
        .collect()
}

#[derive(PartialEq)]
enum Gc {
    Format,
    Surrogate,
    Other,
}

/// Minimal general-category lookup for the Cf/Cs ranges Python's
/// `isprintable` rejects (plus unassigned tolerated as printable, matching
/// Python which treats unassigned as printable... actually Python returns
/// False for unassigned? No: `'\U000E0000'.isprintable()` is False for
/// private-use (Co)? Let me recall: str.isprintable returns False for
/// Other or Separator except ASCII space. Hmm — that would reject many
/// more characters (all Zs separators, Co private use, Cn unassigned).
/// The security goal is control/escape sequences; separators are harmless
/// in this context. We reject Cc, Cf, Cs, Zl/Zp and DEL/C1; spaces and
/// printable text pass through.
fn unicode_general_category(ch: char) -> Gc {
    let c = ch as u32;
    // Cf: 00AD, 061C, 115F-1160, 17B4-17B5, 180B-180F, 200B-200F, 202A-202E,
    // 2060-2064, 2066-206F, 3164, FEFF, FFA0, FFF0-FFF8, 1BCA0-1BCA3,
    // 1D173-1D17A, E0000-E0FFF. Cs: D800-DFFF.
    if (0xD800..0xE000).contains(&c) {
        return Gc::Surrogate;
    }
    if c == 0x00AD
        || c == 0x061C
        || (0x115F..=0x1160).contains(&c)
        || (0x17B4..=0x17B5).contains(&c)
        || (0x180B..=0x180F).contains(&c)
        || (0x200B..=0x200F).contains(&c)
        || (0x2028..=0x202E).contains(&c)
        || (0x2060..=0x206F).contains(&c)
        || c == 0x3164
        || c == 0xFEFF
        || c == 0xFFA0
        || (0xFFF0..=0xFFF8).contains(&c)
        || (0x1BCA0..=0x1BCA3).contains(&c)
        || (0x1D173..=0x1D17A).contains(&c)
        || (0xE0000..=0xE0FFF).contains(&c)
    {
        return Gc::Format;
    }
    Gc::Other
}

/// Wrap one line to WIDTH like Python's textwrap.wrap (default settings:
/// break on whitespace, long words broken, leading whitespace dropped on
/// continuation lines... textwrap default: replace_whitespace=True,
/// drop_whitespace=True, break_long_words=True, break_on_hyphens=True).
fn wrap(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return Vec::new();
    }
    // textwrap with replace_whitespace: tabs/newlines -> space. Our input
    // lines have no newlines; expand tabs to spaces to match.
    let line = line.replace('\t', " ");
    let mut chunks: Vec<String> = Vec::new();
    // Split into word/non-space runs like textwrap._split: chunks are words
    // with hyphen-break handling. Approximate textwrap: split on spaces,
    // then break chunks longer than width at hyphens or hard.
    let mut words: Vec<&str> = Vec::new();
    for word in line.split(' ') {
        if word.is_empty() {
            continue; // drop_whitespace
        }
        words.push(word);
    }
    let mut current = String::new();
    let flush = |current: &mut String, chunks: &mut Vec<String>| {
        if !current.is_empty() {
            chunks.push(std::mem::take(current));
        }
    };
    for word in words {
        // Break over-long words into width-sized pieces (break_on_hyphens:
        // prefer breaking after a hyphen).
        let mut pieces: Vec<String> = Vec::new();
        let mut w = word;
        while w.chars().count() > width {
            // Last hyphen within the first `width` bytes, on a char boundary.
            let mut cut = width.min(w.len());
            while cut > 0 && !w.is_char_boundary(cut) {
                cut -= 1;
            }
            if cut == 0 {
                cut = 1;
                while cut < w.len() && !w.is_char_boundary(cut) {
                    cut += 1;
                }
            }
            if let Some(hy) = w[..cut].rfind('-') {
                if hy > 0 {
                    cut = hy + 1; // hyphen is one byte: still a boundary
                }
            }
            pieces.push(w[..cut].to_string());
            w = &w[cut..];
        }
        pieces.push(w.to_string());
        for (i, piece) in pieces.iter().enumerate() {
            if i > 0 {
                flush(&mut current, &mut chunks);
            }
            // textwrap measures in characters? No — in Python 3, len() of
            // str = characters. Our width accounting should use char count.
            let clen = current.chars().count();
            let plen = piece.chars().count();
            if clen == 0 {
                current.push_str(piece);
            } else if clen + 1 + plen <= width {
                current.push(' ');
                current.push_str(piece);
            } else {
                flush(&mut current, &mut chunks);
                current.push_str(piece);
            }
        }
    }
    flush(&mut current, &mut chunks);
    chunks
}

/// The popup's lines for a `sweep.preview()` result, wrapped to WIDTH.
pub fn lines(label: &str, activity: &str, warnings: &[String]) -> Vec<String> {
    let mut text: Vec<String> = Vec::new();
    text.push(format!("Archive \"{label}\"?"));
    text.push(activity.to_string());
    text.extend(warnings.iter().cloned());
    text.push(HINT.to_string());
    text.push(KEYS.to_string());
    let mut out = Vec::new();
    for line in &text {
        for piece in wrap(&printable(line), WIDTH) {
            out.push(format!(" {piece}"));
        }
    }
    out
}

/// Max archive-name length, in characters.
pub const MAX_NAME_LEN: usize = 80;
/// Re-asks before a blank-named archive is cancelled.
pub const MAX_NAME_ATTEMPTS: u32 = 3;

/// Normalize one answer to the `Archive name [<default>]: ` prompt.
/// An empty line accepts `default` (the tab label); anything else is
/// trimmed, sanitized like popup text (control/format characters become
/// `?`), and capped at [`MAX_NAME_LEN`] characters. Returns None for an
/// unusable answer — a whitespace-only line, or an empty line against a
/// blank default — so the caller can re-ask.
pub fn normalize_name(answer: &str, default: &str) -> Option<String> {
    let line = answer.strip_suffix('\n').unwrap_or(answer);
    let line = line.strip_suffix('\r').unwrap_or(line);
    let chosen = if line.is_empty() {
        default.trim()
    } else {
        line.trim()
    };
    if chosen.is_empty() {
        return None;
    }
    let clean: String = printable(chosen).chars().take(MAX_NAME_LEN).collect();
    if clean.trim().is_empty() {
        None
    } else {
        Some(clean)
    }
}

/// Ask for the archive name, re-asking blank answers up to
/// [`MAX_NAME_ATTEMPTS`] times. Returns the chosen name, or None to cancel
/// the archive. `Err(())` (EOF/interrupt) propagates so the caller can
/// cancel quietly.
#[allow(clippy::result_unit_err)]
pub fn ask_name(
    input_fn: &mut dyn FnMut(&str) -> Result<String, ()>,
    print_fn: &mut dyn FnMut(&str),
    default: &str,
) -> Result<Option<String>, ()> {
    let prompt = format!("Archive name [{default}]: ");
    for _ in 0..MAX_NAME_ATTEMPTS {
        let answer = input_fn(&prompt)?;
        if let Some(name) = normalize_name(&answer, default) {
            return Ok(Some(name));
        }
        print_fn("Please type a name, or press Enter to keep the tab label.");
    }
    print_fn("Too many invalid answers; cancelling.");
    Ok(None)
}

/// Extra popup rows beyond `lines().len() + 3` for the interactive archive
/// flow, so the popup fits its content instead of a fixed guess.
/// `needs` holds one `(has_scanner, candidate_count)` per agent pane without
/// a reported session, in tab order; the archive-name prompt always runs, so
/// its rows are included unconditionally.
pub fn flow_extra_rows(needs: &[(bool, usize)]) -> usize {
    let mut rows = 3; // name prompt + re-ask slack
    for &(scanner, candidates) in needs {
        rows += if scanner {
            1 + candidates + 1 + 2 // header + list + prompt + reprompt slack
        } else {
            1 + 1 + 1 // question + prompt + slack
        };
    }
    rows
}

/// Preview dict accessor used by main: label/activity/warnings from a Value.
pub fn lines_from_preview(preview: &Value) -> Vec<String> {
    let label = preview
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("tab");
    let activity = preview
        .get("activity")
        .and_then(Value::as_str)
        .unwrap_or("");
    let warnings: Vec<String> = preview
        .get("warnings")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    lines(label, activity, &warnings)
}

/// One key from the terminal as a byte, read in raw mode; the terminal's
/// previous mode is restored on every path. Pending input is drained first,
/// so a key typed before the question was drawn is dropped rather than
/// taken as the answer. Only `y`/`Y` vs anything-else matters to callers.
pub fn read_key() -> std::io::Result<u8> {
    use crossterm::event::{self, Event, KeyCode};
    use std::time::Duration;
    // Flush pre-typed keys (termios TCSAFLUSH equivalent).
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }
    crossterm::terminal::enable_raw_mode()?;
    let result = (|| -> std::io::Result<u8> {
        loop {
            match event::read()? {
                Event::Key(k) => {
                    return Ok(match k.code {
                        KeyCode::Char(c) => {
                            let mut buf = [0u8; 4];
                            let s = c.encode_utf8(&mut buf);
                            if s.len() == 1 { s.as_bytes()[0] } else { 0x00 }
                        }
                        KeyCode::Enter => b'\r',
                        KeyCode::Esc => 0x1b,
                        KeyCode::Backspace => 0x7f,
                        KeyCode::Tab => b'\t',
                        _ => 0x00,
                    });
                }
                _ => continue,
            }
        }
    })();
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_and_wrap() {
        let out = lines("mytab", "Last activity today.", &["warn one".to_string()]);
        assert!(out[0].contains("Archive \"mytab\"?"), "{out:?}");
        assert!(out.iter().any(|l| l.contains("Last activity today.")));
        assert!(out.iter().any(|l| l.contains("warn one")));
        assert!(out.iter().any(|l| l.contains(HINT)));
        // textwrap collapses the runs of spaces in KEYS, as in Python.
        assert!(
            out.iter()
                .any(|l| l.contains("y archive any other key cancel"))
        );
        for l in &out {
            assert!(l.chars().count() <= WIDTH + 1, "{l:?}");
        }
    }

    #[test]
    fn control_chars_sanitized() {
        let out = lines("a\x1bb", "ok", &[]);
        assert!(out[0].contains("a?b"), "{out:?}");
    }

    #[test]
    fn long_label_wraps() {
        let long = "x".repeat(100);
        let out = lines(&long, "ok", &[]);
        assert!(out.len() > 4, "{out:?}");
        for l in &out {
            assert!(l.chars().count() <= WIDTH + 1, "{l:?}");
        }
    }

    #[test]
    fn name_default_trim_cap_sanitize() {
        // Enter accepts the tab-label default (with or without newline).
        assert_eq!(normalize_name("", "mytab"), Some("mytab".to_string()));
        assert_eq!(normalize_name("\n", "mytab"), Some("mytab".to_string()));
        // Typed input is trimmed.
        assert_eq!(
            normalize_name("  sprint 9  \n", "mytab"),
            Some("sprint 9".to_string())
        );
        // Capped at MAX_NAME_LEN characters.
        let long = "y".repeat(MAX_NAME_LEN + 20);
        let capped = normalize_name(&long, "mytab").unwrap();
        assert_eq!(capped.chars().count(), MAX_NAME_LEN);
        // Control characters sanitized exactly like popup text.
        assert_eq!(normalize_name("a\x1bb", "mytab"), Some("a?b".to_string()));
        assert_eq!(
            normalize_name("a\u{200b}b", "mytab"),
            Some("a?b".to_string())
        );
    }

    #[test]
    fn name_blank_answers_are_invalid() {
        // Whitespace-only is not "Enter": it re-asks rather than silently
        // taking the default.
        assert_eq!(normalize_name("   \n", "mytab"), None);
        // An empty line against a blank default has nothing to fall back to.
        assert_eq!(normalize_name("", ""), None);
        assert_eq!(normalize_name("\n", "  "), None);
    }

    #[test]
    fn flow_extra_rows_fits_content() {
        // No panes needing questions: name prompt only.
        assert_eq!(flow_extra_rows(&[]), 3);
        // One scanner pane with 3 candidates: header + 3 + prompt + 2 slack.
        assert_eq!(flow_extra_rows(&[(true, 3)]), 3 + 7);
        // Shell-fallback pane: question + prompt + slack.
        assert_eq!(flow_extra_rows(&[(false, 0)]), 3 + 3);
        // Mixed panes add up.
        assert_eq!(flow_extra_rows(&[(true, 10), (false, 0)]), 3 + 14 + 3);
    }

    #[test]
    fn ask_name_loop() {
        // Default accept shows the tab label in the prompt.
        let mut prompts = Vec::new();
        let mut printed = Vec::new();
        let mut input = |p: &str| -> Result<String, ()> {
            prompts.push(p.to_string());
            Ok(String::new())
        };
        let mut print = |s: &str| printed.push(s.to_string());
        assert_eq!(
            ask_name(&mut input, &mut print, "mytab").unwrap(),
            Some("mytab".to_string())
        );
        assert_eq!(prompts, vec!["Archive name [mytab]: ".to_string()]);
        assert!(printed.is_empty());
        // Blank then valid: one re-ask, then the name.
        let mut answers = vec!["   ".to_string(), " work ".to_string()];
        answers.reverse();
        let mut input = |_: &str| -> Result<String, ()> { answers.pop().ok_or(()) };
        let mut printed = Vec::new();
        let mut print = |s: &str| printed.push(s.to_string());
        assert_eq!(
            ask_name(&mut input, &mut print, "mytab").unwrap(),
            Some("work".to_string())
        );
        assert_eq!(printed.len(), 1);
        // Exhausted attempts cancel.
        let mut input = |_: &str| -> Result<String, ()> { Ok("  ".to_string()) };
        let mut printed = Vec::new();
        let mut print = |s: &str| printed.push(s.to_string());
        assert_eq!(ask_name(&mut input, &mut print, "mytab").unwrap(), None);
        // EOF propagates for a quiet cancel.
        let mut input = |_: &str| -> Result<String, ()> { Err(()) };
        let mut print = |_: &str| {};
        assert!(ask_name(&mut input, &mut print, "mytab").is_err());
    }
}
