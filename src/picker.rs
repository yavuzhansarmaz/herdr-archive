//! The popup that lists archived tabs and restores one.
//!
//! Port of `shelf/picker.py`: pure `State`/`reduce`/`render`/`apply` logic
//! with no terminal I/O (the crossterm loop lives in `picker_tty`).
//!
//! Time zones: Python threads an explicit `tz` through grouping and the
//! details line. Here `State` carries an [`OffsetFn`] — local UTC offset in
//! seconds at a given instant — which expresses both fixed test zones and
//! the machine zone (each timestamp's own DST offset, like `astimezone()`).

use crate::archive::Archive;
use crate::restore::RestoreTarget;
use crate::util::{
    FileLock, LockError, Ts, local_days_with, local_offset, parse_iso, shelved_local,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

pub const DELETE_LOCK_WAIT_SECONDS: f64 = 30.0;
pub const LIST_ROWS: usize = 10;
pub const TODAY: &str = "Archived today";
pub const WEEK: &str = "Last 7 days";
pub const MONTH: &str = "Last 30 days";
pub const OLDER: &str = "Older";
pub const GROUPS: [&str; 4] = [TODAY, WEEK, MONTH, OLDER];
pub const KEYS_HINT: &str = " Enter restore  Right/Left open/close  / filter  d delete  q quit";
pub const SWEEP_BUSY: &str = "A sweep is running; try again in a moment.";
pub const TOO_SMALL: &str = "Popup too small";

/// Local UTC offset in seconds at an instant.
pub type OffsetFn = fn(Ts) -> i32;

pub fn days_idle(record: &Value, now: Ts) -> Option<i64> {
    let stamps: Vec<Ts> = record
        .get("panes")
        .and_then(Value::as_object)
        .map(|panes| {
            panes
                .values()
                .filter_map(|m| {
                    m.get("last_activity")
                        .and_then(Value::as_str)
                        .and_then(|s| parse_iso(Some(s)))
                })
                .collect()
        })
        .unwrap_or_default();
    stamps
        .iter()
        .max()
        .map(|max| (now.0 - max.0).div_euclid(86_400 * 1_000_000))
}

/// The record's display name: the user-given archive `name` when present,
/// else the tab label, else the workspace label (records written before
/// naming, or by the Python shelf, have no `name`).
pub fn record_label(record: &Value) -> String {
    record
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            record
                .get("tab")
                .and_then(|t| t.get("label"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            record
                .get("workspace")
                .and_then(|w| w.get("label"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("tab")
        .to_string()
}

pub fn agent_names(record: &Value) -> String {
    let mut names: Vec<&str> = record
        .get("panes")
        .and_then(Value::as_object)
        .map(|panes| {
            panes
                .values()
                .filter_map(|m| {
                    m.get("agent")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names.join(",")
}

/// The record's group, by local calendar days since archived_at.
pub fn group_of(record: &Value, now_days: i64, offset_fn: OffsetFn) -> &'static str {
    let at = record
        .get("archived_at")
        .and_then(Value::as_str)
        .and_then(|s| parse_iso(Some(s)));
    match at {
        None => OLDER,
        Some(ts) => {
            let days = now_days - local_days_with(ts, offset_fn(ts));
            if days <= 0 {
                TODAY
            } else if days < 7 {
                WEEK
            } else if days < 30 {
                MONTH
            } else {
                OLDER
            }
        }
    }
}

fn sort_key(record: &Value, now: Ts) -> (i64, i64) {
    let at = record
        .get("archived_at")
        .and_then(Value::as_str)
        .and_then(|s| parse_iso(Some(s)));
    // Newest first, then least idle; no-activity sorts after sweep mates.
    (
        -at.map(|t| t.0).unwrap_or(0),
        days_idle(record, now).unwrap_or(i64::MAX),
    )
}

fn matches(record: &Value, text: &str) -> bool {
    let lower = text.to_lowercase();
    let fields = [
        record.get("name").and_then(Value::as_str).unwrap_or(""),
        record
            .get("tab")
            .and_then(|t| t.get("label"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        record
            .get("workspace")
            .and_then(|w| w.get("label"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    ];
    let agents = agent_names(record);
    fields.iter().any(|f| f.to_lowercase().contains(&lower))
        || agents.to_lowercase().contains(&lower)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Header,
    Tab,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub kind: RowKind,
    pub group: String,
    pub record: Option<Value>,
    pub count: usize,
    pub is_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Move,
    Filter,
    Confirm,
    Target,
}

pub struct State {
    pub now: Ts,
    pub now_days: i64,
    pub offset_fn: OffsetFn,
    pub grouped: BTreeMap<String, Vec<Value>>,
    pub open_groups: HashSet<String>,
    pub saved_open: Option<HashSet<String>>,
    pub cursor: usize,
    pub offset: usize,
    pub filter_text: String,
    pub mode: Mode,
    pub message: String,
    pub height: usize,
    pub width: usize,
    pub pending_restore: Option<PendingRestore>,
}

impl State {
    pub fn total(&self) -> usize {
        self.grouped.values().map(Vec::len).sum()
    }
}

fn load(state: &mut State, records: Vec<Value>) {
    state.grouped = GROUPS.iter().map(|g| (g.to_string(), Vec::new())).collect();
    let mut sorted = records;
    sorted.sort_by_key(|r| sort_key(r, state.now));
    for rec in sorted {
        let g = group_of(&rec, state.now_days, state.offset_fn);
        state.grouped.get_mut(g).unwrap().push(rec);
    }
}

pub fn initial_state(records: Vec<Value>, now: Ts, offset_fn: OffsetFn) -> State {
    let now_days = local_days_with(now, offset_fn(now));
    let mut state = State {
        now,
        now_days,
        offset_fn,
        grouped: BTreeMap::new(),
        open_groups: HashSet::new(),
        saved_open: None,
        cursor: 0,
        offset: 0,
        filter_text: String::new(),
        mode: Mode::Move,
        message: String::new(),
        height: 16,
        width: 80,
        pending_restore: None,
    };
    load(&mut state, records);
    let present: Vec<String> = GROUPS
        .iter()
        .filter(|g| !state.grouped[**g].is_empty())
        .map(|g| g.to_string())
        .collect();
    // Older starts collapsed, unless it is all there is.
    let mut open: HashSet<String> = present
        .iter()
        .filter(|g| g.as_str() != OLDER)
        .cloned()
        .collect();
    if open.is_empty() {
        open = present.into_iter().collect();
    }
    state.open_groups = open;
    state.cursor = first_tab(&rows(&state));
    state
}

pub fn rows(state: &State) -> Vec<Row> {
    let mut out = Vec::new();
    for name in GROUPS {
        let members = state.grouped.get(name).cloned().unwrap_or_default();
        let members: Vec<Value> = if state.filter_text.is_empty() {
            members
        } else {
            members
                .into_iter()
                .filter(|r| matches(r, &state.filter_text))
                .collect()
        };
        if members.is_empty() {
            continue;
        }
        let is_open = state.open_groups.contains(name);
        out.push(Row {
            kind: RowKind::Header,
            group: name.to_string(),
            record: None,
            count: members.len(),
            is_open,
        });
        if is_open {
            for r in members {
                out.push(Row {
                    kind: RowKind::Tab,
                    group: name.to_string(),
                    record: Some(r),
                    count: 0,
                    is_open: false,
                });
            }
        }
    }
    out
}

fn first_tab(rs: &[Row]) -> usize {
    rs.iter().position(|r| r.kind == RowKind::Tab).unwrap_or(0)
}

fn too_small(state: &State) -> bool {
    state.height < 5 || state.width < 30
}

/// (list rows, show the scroll markers, show the details line).
pub fn layout(height: usize) -> (usize, bool, bool) {
    if height >= LIST_ROWS + 5 {
        (LIST_ROWS, true, true)
    } else if height >= LIST_ROWS + 4 {
        (LIST_ROWS, true, false)
    } else {
        (LIST_ROWS.min(height.saturating_sub(2)).max(1), false, false)
    }
}

/// The list-window row at screen line `y`, or None outside the list.
pub fn window_row(state: &State, y: usize) -> Option<usize> {
    if too_small(state) {
        return None;
    }
    let (win, markers, _) = layout(state.height);
    let top = if markers { 2 } else { 1 };
    if y < top {
        return None;
    }
    let row = y - top;
    if row < win { Some(row) } else { None }
}

fn follow(state: &mut State) {
    let n = rows(state).len();
    let win = layout(state.height).0;
    state.cursor = if n == 0 { 0 } else { state.cursor.min(n - 1) };
    if state.cursor < state.offset {
        state.offset = state.cursor;
    } else if state.cursor >= state.offset + win {
        state.offset = state.cursor - win + 1;
    }
    state.offset = state.offset.min(n.saturating_sub(win));
}

fn current(state: &State) -> Option<Row> {
    rows(state).into_iter().nth(state.cursor)
}

fn toggle(state: &mut State, group: &str) {
    if state.open_groups.contains(group) {
        state.open_groups.remove(group);
    } else {
        state.open_groups.insert(group.to_string());
    }
    follow(state);
}

fn set_filter(state: &mut State, text: String) {
    if !text.is_empty() && state.saved_open.is_none() {
        state.saved_open = Some(state.open_groups.clone());
    }
    state.filter_text = text.clone();
    if !text.is_empty() {
        state.open_groups = GROUPS
            .iter()
            .filter(|g| state.grouped[**g].iter().any(|r| matches(r, &text)))
            .map(|g| g.to_string())
            .collect();
    } else if let Some(saved) = state.saved_open.take() {
        state.open_groups = saved;
    }
    state.cursor = first_tab(&rows(state));
    state.offset = 0;
    follow(state);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RowKey {
    Tab(String),
    Header(String),
}

fn row_key(row: &Row) -> RowKey {
    match row.kind {
        RowKind::Tab => RowKey::Tab(
            row.record
                .as_ref()
                .and_then(|r| r.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        RowKind::Header => RowKey::Header(row.group.clone()),
    }
}

/// Swap in a re-read archive, keeping the cursor when its row still exists.
pub fn refresh(state: &mut State, records: Vec<Value>) {
    let cur = current(state);
    let key = cur.as_ref().map(row_key);
    let old = state.cursor;
    let before: HashSet<String> = GROUPS
        .iter()
        .filter(|g| !state.grouped[**g].is_empty())
        .map(|g| g.to_string())
        .collect();
    load(state, records);
    let present: Vec<String> = GROUPS
        .iter()
        .filter(|g| !state.grouped[**g].is_empty())
        .map(|g| g.to_string())
        .collect();
    let mut appeared: HashSet<String> = present
        .iter()
        .filter(|g| !before.contains(*g) && g.as_str() != OLDER)
        .cloned()
        .collect();
    if present.len() == 1 && present[0] == OLDER {
        appeared.insert(OLDER.to_string());
    }
    state.open_groups.extend(appeared.iter().cloned());
    if let Some(saved) = state.saved_open.as_mut() {
        saved.extend(appeared);
    }
    if !state.filter_text.is_empty() {
        let ft = state.filter_text.clone();
        for g in &present {
            if state.grouped[g].iter().any(|r| matches(r, &ft)) {
                state.open_groups.insert(g.clone());
            }
        }
    }
    let keys: Vec<RowKey> = rows(state).iter().map(row_key).collect();
    state.cursor = key
        .and_then(|k| keys.iter().position(|x| x == &k))
        .unwrap_or(old);
    follow(state);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Up,
    Down,
    PgUp,
    PgDn,
    Home,
    End,
    Enter,
    Left,
    Right,
    Backspace,
    Esc,
    WheelUp,
    WheelDown,
    Other,
    Char(char),
    Click(usize),
    DClick(usize),
    Resize { h: usize, w: usize },
}

/// A restore request. `target` is None while undecided (the initial Enter):
/// [`apply`] then checks whether the record's original workspace is still
/// live and either proceeds with [`RestoreTarget::New`] or prompts for the
/// choice; the choice keys produce a decided request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRequest {
    pub id: String,
    pub target: Option<RestoreTarget>,
}

impl RestoreRequest {
    pub fn undecided(id: String) -> Self {
        RestoreRequest { id, target: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Restore(RestoreRequest),
    Delete(String),
    Quit,
}

/// A restore awaiting the original-vs-new workspace choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRestore {
    pub id: String,
    pub workspace_id: String,
    pub workspace_label: String,
}

fn steps(kind: &Event) -> Option<isize> {
    match kind {
        Event::Up => Some(-1),
        Event::Down => Some(1),
        Event::PgUp => Some(-(LIST_ROWS as isize)),
        Event::PgDn => Some(LIST_ROWS as isize),
        Event::WheelUp => Some(-3),
        Event::WheelDown => Some(3),
        _ => None,
    }
}

/// Apply one input event to `state` (in place); return an action or None.
pub fn reduce(state: &mut State, event: Event) -> Option<Action> {
    if let Event::Resize { h, w } = event {
        state.height = h;
        state.width = w;
        follow(state);
        return None;
    }
    state.message.clear();
    if state.total() == 0 {
        return Some(Action::Quit);
    }
    if too_small(state) {
        return match event {
            Event::Esc => Some(Action::Quit),
            Event::Char('q') => Some(Action::Quit),
            _ => None,
        };
    }
    if state.mode == Mode::Confirm {
        state.mode = Mode::Move;
        if let Event::Char('y') | Event::Char('Y') = event {
            if let Some(cur) = current(state) {
                if cur.kind == RowKind::Tab {
                    let id = cur
                        .record
                        .unwrap()
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    return Some(Action::Delete(id));
                }
            }
        }
        return None;
    }
    if state.mode == Mode::Target {
        state.mode = Mode::Move;
        let p = state.pending_restore.take()?;
        // Enter = new workspace (the default); o = original; anything else
        // (Esc, q, ...) cancels, like the delete confirm.
        return match event {
            Event::Enter => Some(Action::Restore(RestoreRequest {
                id: p.id,
                target: Some(RestoreTarget::New),
            })),
            Event::Char('o') | Event::Char('O') => Some(Action::Restore(RestoreRequest {
                id: p.id,
                target: Some(RestoreTarget::Existing(p.workspace_id)),
            })),
            _ => None,
        };
    }
    if state.mode == Mode::Filter {
        match event {
            Event::Char(c) => {
                let mut t = state.filter_text.clone();
                t.push(c);
                set_filter(state, t);
            }
            Event::Backspace => {
                if !state.filter_text.is_empty() {
                    let mut t = state.filter_text.clone();
                    t.pop();
                    set_filter(state, t);
                } else {
                    state.mode = Mode::Move;
                }
            }
            Event::Enter => state.mode = Mode::Move,
            Event::Esc => {
                if !state.filter_text.is_empty() {
                    set_filter(state, String::new());
                }
                state.mode = Mode::Move;
            }
            Event::Click(_) | Event::DClick(_) | Event::WheelUp | Event::WheelDown => {
                state.mode = Mode::Move;
                return reduce_move(state, event);
            }
            _ => {}
        }
        return None;
    }
    reduce_move(state, event)
}

fn reduce_move(state: &mut State, event: Event) -> Option<Action> {
    let event = match event {
        Event::Char('k') => Event::Up,
        Event::Char('j') => Event::Down,
        Event::Char('h') => Event::Left,
        Event::Char('l') => Event::Right,
        e => e,
    };
    let rs = rows(state);
    if let Some(step) = steps(&event) {
        state.cursor = (state.cursor as isize + step).max(0) as usize;
        follow(state);
        return None;
    }
    match event {
        Event::Home => {
            state.cursor = 0;
            follow(state);
            return None;
        }
        Event::End => {
            state.cursor = rs.len().saturating_sub(1);
            follow(state);
            return None;
        }
        Event::Click(row) | Event::DClick(row) => {
            let dclick = matches!(&event, Event::DClick(_));
            let index = state.offset + row;
            if index >= rs.len() {
                return None;
            }
            state.cursor = index;
            let r = &rs[index];
            if r.kind == RowKind::Header {
                let group = r.group.clone();
                toggle(state, &group);
                return None;
            }
            if dclick {
                let id = r
                    .record
                    .as_ref()
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                return Some(Action::Restore(RestoreRequest::undecided(id)));
            }
            return None;
        }
        _ => {}
    }
    let cur = current(state);
    match event {
        Event::Char('q') => return Some(Action::Quit),
        Event::Esc => {
            if !state.filter_text.is_empty() {
                set_filter(state, String::new());
                return None;
            }
            return Some(Action::Quit);
        }
        Event::Char('/') => {
            state.mode = Mode::Filter;
            return None;
        }
        _ => {}
    }
    let cur = cur?;
    match event {
        Event::Enter => {
            if cur.kind == RowKind::Tab {
                let id = cur
                    .record
                    .as_ref()
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                return Some(Action::Restore(RestoreRequest::undecided(id)));
            }
            toggle(state, &cur.group);
        }
        Event::Right => {
            if !state.open_groups.contains(&cur.group) {
                toggle(state, &cur.group);
            }
        }
        Event::Left => {
            if cur.kind == RowKind::Tab {
                let rs = rows(state);
                if let Some(i) = rs
                    .iter()
                    .position(|r| r.kind == RowKind::Header && r.group == cur.group)
                {
                    state.cursor = i;
                }
            }
            if state.open_groups.contains(&cur.group) {
                toggle(state, &cur.group);
            }
        }
        Event::Char('d') if cur.kind == RowKind::Tab => {
            state.mode = Mode::Confirm;
        }
        _ => {}
    }
    None
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

fn pad_to(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len >= n {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(n - len))
    }
}

fn tab_line(record: &Value, selected: bool, width: usize, now: Ts) -> String {
    let limit = (width.saturating_sub(1)).max(20);
    let fixed = 3 + 1 + 12 + 1 + 4;
    let room = limit.saturating_sub(fixed).max(10);
    let tab_w = ((room as f64 * 0.6) as usize).max(8);
    let ws_w = room.saturating_sub(tab_w + 1).max(6);
    let title = record
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            record
                .get("tab")
                .and_then(|t| t.get("label"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("(unnamed)");
    let tab = truncate_chars(title, tab_w);
    let workspace = truncate_chars(
        record
            .get("workspace")
            .and_then(|w| w.get("label"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        ws_w,
    );
    let names = truncate_chars(&agent_names(record), 12);
    let idle = match days_idle(record, now) {
        None => "?".to_string(),
        Some(d) => format!("{d}d"),
    };
    format!(
        " {} {} {} {} {idle}",
        if selected { ">" } else { " " },
        pad_to(&tab, tab_w),
        pad_to(&workspace, ws_w),
        pad_to(&names, 12)
    )
    .trim_end()
    .to_string()
}

fn details(record: &Value, offset_fn: OffsetFn) -> String {
    let mut cwd = record
        .get("workspace")
        .and_then(|w| w.get("cwd"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let home = crate::history::home_dir().to_string_lossy().into_owned();
    if !home.is_empty() && (cwd == home || cwd.starts_with(&format!("{home}/"))) {
        cwd = format!("~{}", &cwd[home.len()..]);
    }
    let shelved = record
        .get("archived_at")
        .and_then(Value::as_str)
        .and_then(|s| parse_iso(Some(s)))
        .map(|at| shelved_local(at, offset_fn(at)))
        .unwrap_or_default();
    let panes = record.get("panes").and_then(Value::as_object);
    let n = panes.map(|p| p.len()).unwrap_or(0);
    let names = agent_names(record);
    let count = format!("{n} pane{}", if n == 1 { "" } else { "s" })
        + &if names.is_empty() {
            String::new()
        } else {
            format!(": {names}")
        };
    format!(
        " {}",
        [cwd, shelved, count]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("  ")
    )
}

fn status(state: &State) -> String {
    if state.mode == Mode::Confirm {
        let label = current(state)
            .and_then(|c| c.record)
            .map(|r| record_label(&r))
            .unwrap_or_else(|| "tab".to_string());
        return format!(" Delete \"{label}\"? This cannot be undone. [y/N]");
    }
    if state.mode == Mode::Target {
        if let Some(p) = &state.pending_restore {
            return format!(
                " Original workspace \"{}\" still exists. [Enter] new workspace, [o] original, [Esc] cancel",
                p.workspace_label
            );
        }
    }
    if state.mode == Mode::Filter {
        return format!(" /{}", state.filter_text);
    }
    if !state.message.is_empty() {
        return format!(" {}", state.message);
    }
    if !state.filter_text.is_empty() {
        return format!(" /{}  (Esc clears the filter)", state.filter_text);
    }
    KEYS_HINT.to_string()
}

/// The lines to draw, and the index of the highlighted line (or None).
pub fn render(state: &State) -> (Vec<String>, Option<usize>) {
    if too_small(state) {
        return (vec![TOO_SMALL.to_string()], None);
    }
    let limit = state.width.saturating_sub(1);
    if state.total() == 0 {
        return (
            vec![
                "No archived tabs.".to_string(),
                "Press any key to close.".to_string(),
            ],
            None,
        );
    }
    let rs = rows(state);
    let (win, markers, show_details) = layout(state.height);
    let mut lines = vec![format!(" Archive - {} archived", state.total())];
    let above = state.offset;
    let below = rs.len().saturating_sub(state.offset + win);
    if markers {
        lines.push(if above > 0 {
            format!(" ^ {above} more")
        } else {
            String::new()
        });
    }
    let mut highlight = None;
    let mut listed = Vec::new();
    for (index, row) in rs.iter().enumerate().skip(state.offset).take(win) {
        if index == state.cursor {
            highlight = Some(lines.len() + listed.len());
        }
        if row.kind == RowKind::Header {
            listed.push(format!(
                " {} {} ({})",
                if row.is_open { "-" } else { "+" },
                row.group,
                row.count
            ));
        } else {
            listed.push(tab_line(
                row.record.as_ref().unwrap(),
                index == state.cursor,
                state.width,
                state.now,
            ));
        }
    }
    if rs.is_empty() {
        listed.push(" No match".to_string());
    }
    while listed.len() < win {
        listed.push(String::new());
    }
    lines.extend(listed);
    if markers {
        lines.push(if below > 0 {
            format!(" v {below} more")
        } else {
            String::new()
        });
    }
    if show_details {
        let cur = current(state);
        lines.push(match cur {
            Some(c) if c.kind == RowKind::Tab => {
                details(c.record.as_ref().unwrap(), state.offset_fn)
            }
            _ => String::new(),
        });
    }
    lines.push(status(state));
    (
        lines
            .into_iter()
            .map(|l| truncate_chars(&l, limit))
            .collect(),
        highlight,
    )
}

/// Archive access for [`apply`] (the real store, or a fake in tests).
pub trait Arch {
    fn list(&self) -> Vec<Value>;
    fn delete(&self, id: &str);
    fn state_dir(&self) -> PathBuf;
}

impl Arch for Archive {
    fn list(&self) -> Vec<Value> {
        Archive::list(self)
    }
    fn delete(&self, id: &str) {
        let _ = Archive::delete(self, id);
    }
    fn state_dir(&self) -> PathBuf {
        self.root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

pub enum RestoreOutcome {
    Ok(Vec<String>),
    Busy,
    Failed(String),
}

/// The record's original workspace, when the record names one.
fn pending_for(state: &State, id: &str) -> Option<PendingRestore> {
    let rec = state
        .grouped
        .values()
        .flatten()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(id))?;
    let workspace_id = rec
        .get("workspace")?
        .get("workspace_id")?
        .as_str()
        .filter(|s| !s.is_empty())?
        .to_string();
    let workspace_label = rec
        .get("workspace")
        .and_then(|w| w.get("label"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(PendingRestore {
        id: id.to_string(),
        workspace_id,
        workspace_label,
    })
}

/// Carry out a reducer action. Returns true when the popup should close.
/// An undecided restore checks (via `workspace_live`) whether the record's
/// original workspace is still live: if so the popup prompts for the choice
/// ([Enter] new, [o] original) instead of restoring; otherwise — and for
/// records with no recorded id — it restores into a new workspace at once.
pub fn apply(
    state: &mut State,
    action: Option<Action>,
    arch: &dyn Arch,
    do_restore: &dyn Fn(&str, RestoreTarget) -> RestoreOutcome,
    workspace_live: &dyn Fn(&str) -> bool,
    notify: &dyn Fn(&str, &str),
    draw: &dyn Fn(&State),
) -> bool {
    let Some(action) = action else {
        return false;
    };
    let (id, target) = match &action {
        Action::Restore(req) => (req.id.clone(), req.target.clone()),
        Action::Delete(id) => (id.clone(), None),
        Action::Quit => return true,
    };
    if matches!(action, Action::Delete(_)) {
        state.message = "Waiting for a sweep to finish...".to_string();
        draw(state);
        state.message.clear();
        match FileLock::new(
            &arch.state_dir().join("sweep.lock"),
            DELETE_LOCK_WAIT_SECONDS,
        )
        .lock()
        {
            Ok(_guard) => {
                arch.delete(&id);
            }
            Err(LockError::Busy(_)) => {
                state.message = SWEEP_BUSY.to_string();
            }
            Err(_) => {
                state.message = SWEEP_BUSY.to_string();
            }
        }
        refresh(state, arch.list());
        return false;
    }
    let target = match target {
        Some(t) => t,
        None => {
            if let Some(p) = pending_for(state, &id) {
                if workspace_live(&p.workspace_id) {
                    state.pending_restore = Some(p);
                    state.mode = Mode::Target;
                    draw(state);
                    return false;
                }
            }
            RestoreTarget::New
        }
    };
    let cur = current(state);
    let label = cur
        .as_ref()
        .and_then(|c| c.record.clone())
        .map(|r| record_label(&r))
        .unwrap_or_else(|| id.clone());
    state.message = format!("Restoring {label}...");
    draw(state);
    let previous = ignore_sigint();
    let outcome = do_restore(&id, target);
    restore_sigint(previous);
    match outcome {
        RestoreOutcome::Ok(warnings) => {
            if !warnings.is_empty() {
                notify("herdr-archive", &warnings.join("; "));
            }
            return true;
        }
        RestoreOutcome::Busy => {
            state.message = SWEEP_BUSY.to_string();
        }
        RestoreOutcome::Failed(e) => {
            state.message = format!("Restore failed: {e}");
        }
    }
    refresh(state, arch.list());
    false
}

fn ignore_sigint() -> libc::sighandler_t {
    unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) }
}

fn restore_sigint(previous: libc::sighandler_t) {
    unsafe {
        libc::signal(libc::SIGINT, previous);
    }
}

/// Default offset function: the machine's own zone.
pub fn machine_offset(ts: Ts) -> i32 {
    local_offset(ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::iso;

    const LOCAL_OFF: i32 = -7 * 3600;
    fn local(_: Ts) -> i32 {
        LOCAL_OFF
    }
    // 2026-09-29 10:00 at -07:00 == 17:00Z.
    fn now_ts() -> Ts {
        parse_iso(Some("2026-09-29T17:00:00Z")).unwrap()
    }
    fn now_days() -> i64 {
        local_days_with(now_ts(), LOCAL_OFF)
    }

    fn rec(
        id: &str,
        archived_at: &str,
        label: &str,
        workspace: &str,
        agent: &str,
        idle_days: i64,
    ) -> Value {
        let last = iso(Ts(now_ts().0 - (idle_days * 86_400 + 3600) * 1_000_000));
        serde_json::json!({"id": id, "archived_at": archived_at, "tab": {"label": label},
            "workspace": {"label": workspace, "cwd": null},
            "panes": {"p": {"agent": agent, "last_activity": last}}})
    }

    const TODAY_AT: &str = "2026-09-29T16:14:00Z";
    const WEEK_AT: &str = "2026-09-25T21:41:17Z";
    const MONTH_AT: &str = "2026-09-10T12:00:00Z";
    const OLDER_AT: &str = "2026-08-01T12:00:00Z";

    fn eleven() -> Vec<Value> {
        vec![
            rec("t1", TODAY_AT, "perf-probe", "backend", "claude", 21),
            rec("t2", TODAY_AT, "flaky-test", "backend", "codex", 15),
            rec("w1", WEEK_AT, "onboarding", "docs", "claude", 28),
            rec("w2", WEEK_AT, "api-refactor", "backend", "claude", 17),
            rec("w3", WEEK_AT, "release-notes", "docs", "claude", 18),
            rec("m1", MONTH_AT, "db-migration", "backend", "claude", 34),
            rec(
                "m2",
                "2026-09-09T12:00:00Z",
                "style-guide",
                "docs",
                "claude",
                42,
            ),
            rec("o1", OLDER_AT, "lint-cleanup", "backend", "claude", 43),
            rec("o2", OLDER_AT, "api-sketch", "backend", "claude", 60),
            rec("o3", OLDER_AT, "old-spike", "backend", "claude", 32),
            rec(
                "o4",
                "2026-07-01T12:00:00Z",
                "misc",
                "backend",
                "claude",
                90,
            ),
        ]
    }

    fn state_of(records: Vec<Value>) -> State {
        initial_state(records, now_ts(), local)
    }

    fn labels(state: &State) -> Vec<String> {
        rows(state)
            .iter()
            .map(|r| match r.kind {
                RowKind::Header => r.group.clone(),
                RowKind::Tab => r.record.as_ref().unwrap()["tab"]["label"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            })
            .collect()
    }

    #[test]
    fn group_boundaries_in_local_calendar_days() {
        let cases = [
            ("2026-09-29T07:00:00Z", TODAY), // 00:00 local today
            ("2026-09-29T06:59:00Z", WEEK),  // 23:59 local yesterday
            ("2026-09-23T12:00:00Z", WEEK),
            ("2026-09-22T12:00:00Z", MONTH),
            ("2026-08-31T12:00:00Z", MONTH),
            ("2026-08-30T12:00:00Z", OLDER),
            ("2026-10-01T12:00:00Z", TODAY), // clock ahead
            ("2026-09-29T03:00:00Z", WEEK),  // 20:00 local yesterday
        ];
        for (at, expected) in cases {
            let r = serde_json::json!({"archived_at": at});
            assert_eq!(group_of(&r, now_days(), local), expected, "{at}");
        }
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"archived_at": ""}),
            serde_json::json!({"archived_at": "x"}),
        ] {
            assert_eq!(group_of(&bad, now_days(), local), OLDER);
        }
    }

    #[test]
    fn initial_grouping_sort_collapse() {
        let s = state_of(eleven());
        assert_eq!(
            labels(&s),
            vec![
                TODAY,
                "flaky-test",
                "perf-probe", // least idle first within the day
                WEEK,
                "api-refactor",
                "release-notes",
                "onboarding",
                MONTH,
                "db-migration",
                "style-guide",
                OLDER, // collapsed
            ]
        );
        // cursor starts on the first tab row
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn older_opens_when_only_group() {
        let s = state_of(vec![rec("o1", OLDER_AT, "x", "b", "claude", 1)]);
        assert_eq!(labels(&s), vec![OLDER, "x"]);
    }

    #[test]
    fn moves_and_vim_and_pages() {
        let mut s = state_of(eleven());
        assert_eq!(reduce(&mut s, Event::Down), None);
        assert_eq!(s.cursor, 2);
        assert_eq!(reduce(&mut s, Event::Char('k')), None);
        assert_eq!(s.cursor, 1);
        assert_eq!(reduce(&mut s, Event::Char('j')), None);
        assert_eq!(s.cursor, 2);
        assert_eq!(reduce(&mut s, Event::Up), None);
        assert_eq!(reduce(&mut s, Event::Up), None);
        assert_eq!(s.cursor, 0); // stops at the top
        assert_eq!(reduce(&mut s, Event::End), None);
        assert_eq!(s.cursor, rows(&s).len() - 1);
        assert_eq!(reduce(&mut s, Event::Down), None);
        assert_eq!(s.cursor, rows(&s).len() - 1); // stops at the bottom
        assert_eq!(reduce(&mut s, Event::Home), None);
        assert_eq!(s.cursor, 0);
        assert_eq!(reduce(&mut s, Event::PgDn), None);
        assert_eq!(s.cursor, 10);
        assert_eq!(reduce(&mut s, Event::WheelUp), None);
        assert_eq!(s.cursor, 7);
        assert_eq!(reduce(&mut s, Event::WheelDown), None);
        assert_eq!(s.cursor, 10);
    }

    #[test]
    fn enter_left_right_toggles() {
        let mut s = state_of(eleven());
        // enter on a tab restores it (target undecided until apply checks)
        assert_eq!(
            reduce(&mut s, Event::Enter),
            Some(Action::Restore(RestoreRequest::undecided("t2".to_string())))
        );
        // enter on a header toggles
        s.cursor = 0;
        assert_eq!(reduce(&mut s, Event::Enter), None);
        assert!(!s.open_groups.contains(TODAY));
        // right opens
        assert_eq!(reduce(&mut s, Event::Right), None);
        assert!(s.open_groups.contains(TODAY));
        // left on a tab jumps to its header and closes
        s.cursor = 1;
        assert_eq!(reduce(&mut s, Event::Left), None);
        assert_eq!(s.cursor, 0);
        assert!(!s.open_groups.contains(TODAY));
    }

    #[test]
    fn q_and_esc_quit() {
        let mut s = state_of(eleven());
        assert_eq!(reduce(&mut s, Event::Char('q')), Some(Action::Quit));
        let mut s = state_of(eleven());
        assert_eq!(reduce(&mut s, Event::Esc), Some(Action::Quit));
    }

    #[test]
    fn click_and_dclick() {
        let mut s = state_of(eleven());
        // list starts at screen line 2 (header + marker)
        assert_eq!(reduce(&mut s, Event::Click(1)), None);
        assert_eq!(s.cursor, 1);
        assert_eq!(
            reduce(&mut s, Event::DClick(2)),
            Some(Action::Restore(RestoreRequest::undecided("t1".to_string())))
        );
        // click on a header toggles
        assert_eq!(reduce(&mut s, Event::Click(0)), None);
        assert!(!s.open_groups.contains(TODAY));
        // dclick on a header toggles without restoring
        assert_eq!(reduce(&mut s, Event::DClick(0)), None);
        assert!(s.open_groups.contains(TODAY));
        // click past the end is ignored
        assert_eq!(reduce(&mut s, Event::Click(50)), None);
    }

    #[test]
    fn window_row_mapping() {
        let s = state_of(eleven());
        assert_eq!(window_row(&s, 0), None);
        assert_eq!(window_row(&s, 1), None);
        assert_eq!(window_row(&s, 2), Some(0));
        assert_eq!(window_row(&s, 11), Some(9));
        assert_eq!(window_row(&s, 12), None);
    }

    #[test]
    fn filter_lifecycle() {
        let mut s = state_of(eleven());
        assert_eq!(reduce(&mut s, Event::Char('/')), None);
        assert_eq!(s.mode, Mode::Filter);
        for c in "docs".chars() {
            assert_eq!(reduce(&mut s, Event::Char(c)), None);
        }
        assert_eq!(s.filter_text, "docs");
        // only groups with matches, shown open
        assert_eq!(
            labels(&s),
            vec![WEEK, "release-notes", "onboarding", MONTH, "style-guide"]
        );
        assert_eq!(reduce(&mut s, Event::Enter), None);
        assert_eq!(s.mode, Mode::Move);
        assert_eq!(s.filter_text, "docs");
        // esc clears the filter, esc quits
        assert_eq!(reduce(&mut s, Event::Esc), None);
        assert_eq!(s.filter_text, "");
        assert_eq!(labels(&s).len(), 11);
        assert_eq!(reduce(&mut s, Event::Esc), Some(Action::Quit));
    }

    #[test]
    fn filter_backspace_and_empty_esc() {
        let mut s = state_of(eleven());
        reduce(&mut s, Event::Char('/'));
        reduce(&mut s, Event::Char('x'));
        assert_eq!(reduce(&mut s, Event::Backspace), None);
        assert_eq!(s.filter_text, "");
        assert_eq!(s.mode, Mode::Filter);
        assert_eq!(reduce(&mut s, Event::Backspace), None);
        assert_eq!(s.mode, Mode::Move);
        // esc on an empty filter leaves the cursor where it was
        let cursor = s.cursor;
        reduce(&mut s, Event::Char('/'));
        assert_eq!(reduce(&mut s, Event::Esc), None);
        assert_eq!(s.cursor, cursor);
    }

    #[test]
    fn filter_restores_open_groups() {
        let mut s = state_of(eleven());
        s.open_groups.remove(WEEK);
        reduce(&mut s, Event::Char('/'));
        reduce(&mut s, Event::Char('z'));
        reduce(&mut s, Event::Char('z'));
        assert!(rows(&s).is_empty()); // no match
        reduce(&mut s, Event::Esc);
        assert!(!s.open_groups.contains(WEEK));
        assert!(s.open_groups.contains(TODAY));
    }

    #[test]
    fn delete_confirm() {
        let mut s = state_of(eleven());
        assert_eq!(reduce(&mut s, Event::Char('d')), None);
        assert_eq!(s.mode, Mode::Confirm);
        assert_eq!(
            reduce(&mut s, Event::Char('y')),
            Some(Action::Delete("t2".to_string()))
        );
        // anything else cancels
        let mut s = state_of(eleven());
        reduce(&mut s, Event::Char('d'));
        assert_eq!(reduce(&mut s, Event::Char('n')), None);
        assert_eq!(s.mode, Mode::Move);
        // d on a header does nothing
        let mut s = state_of(eleven());
        s.cursor = 0;
        assert_eq!(reduce(&mut s, Event::Char('d')), None);
        assert_eq!(s.mode, Mode::Move);
    }

    #[test]
    fn too_small_only_closing_works() {
        let mut s = state_of(eleven());
        s.height = 4;
        assert_eq!(reduce(&mut s, Event::Down), None);
        assert_eq!(reduce(&mut s, Event::Enter), None);
        assert_eq!(reduce(&mut s, Event::Esc), Some(Action::Quit));
        let mut s = state_of(eleven());
        s.width = 20;
        assert_eq!(reduce(&mut s, Event::Char('q')), Some(Action::Quit));
        assert_eq!(reduce(&mut s, Event::Char('/')), None);
        assert_eq!(reduce(&mut s, Event::Char('q')), Some(Action::Quit));
    }

    #[test]
    fn refresh_keeps_cursor_and_opens_appeared() {
        let mut s = state_of(eleven());
        s.cursor = 5; // w3 release-notes
        let mut recs = eleven();
        recs.retain(|r| r["id"] != "w2"); // remove the row above the cursor
        refresh(&mut s, recs);
        assert_eq!(s.cursor, 4);
        assert_eq!(labels(&s)[4], "release-notes");
        // a group that appears opens (Older only if sole)
        let mut s = state_of(vec![rec("t1", TODAY_AT, "a", "b", "claude", 1)]);
        let mut recs = vec![rec("t1", TODAY_AT, "a", "b", "claude", 1)];
        recs.push(rec("w1", WEEK_AT, "b", "b", "claude", 1));
        refresh(&mut s, recs);
        assert!(s.open_groups.contains(WEEK));
    }

    #[test]
    fn render_popup() {
        let s = state_of(eleven());
        let (lines, highlight) = render(&s);
        assert_eq!(lines[0], " Archive - 11 archived");
        assert_eq!(lines[1], "");
        assert!(lines[2].contains("Archived today (2)"), "{lines:?}");
        assert_eq!(highlight, Some(3));
        assert!(lines[3].contains("flaky-test"), "{lines:?}");
        assert_eq!(lines[12], " v 1 more");
        assert!(
            lines[13].contains("2 panes") || lines[13].contains("1 pane"),
            "{lines:?}"
        );
        assert_eq!(lines[14], KEYS_HINT);
        // every line fits
        for l in &lines {
            assert!(l.chars().count() <= 79, "{l:?}");
        }
    }

    #[test]
    fn record_label_prefers_name_with_fallbacks() {
        // Named record: the user-given name wins.
        let mut r = rec("t1", TODAY_AT, "tab-label", "ws", "claude", 21);
        r["name"] = serde_json::json!("my name");
        assert_eq!(record_label(&r), "my name");
        // Nameless records (old archives, Python-written): tab label,
        // then workspace label, then "tab".
        let r = rec("t1", TODAY_AT, "tab-label", "ws", "claude", 21);
        assert_eq!(record_label(&r), "tab-label");
        let r = rec("t1", TODAY_AT, "", "ws", "claude", 21);
        assert_eq!(record_label(&r), "ws");
        let r = rec("t1", TODAY_AT, "", "", "claude", 21);
        assert_eq!(record_label(&r), "tab");
        // An empty name falls back to the label.
        let mut r = rec("t1", TODAY_AT, "tab-label", "ws", "claude", 21);
        r["name"] = serde_json::json!("");
        assert_eq!(record_label(&r), "tab-label");
    }

    #[test]
    fn rows_show_name_and_filter_matches_it() {
        let mut named = rec("t1", TODAY_AT, "tab-one", "ws", "claude", 21);
        named["name"] = serde_json::json!("sprint-planning");
        // The row title shows the name, not the tab label.
        let line = tab_line(&named, false, 80, now_ts());
        assert!(line.contains("sprint-planning"), "{line:?}");
        assert!(!line.contains("tab-one"), "{line:?}");
        // The filter matches the name (label/workspace/agent still match).
        assert!(matches(&named, "sprint"));
        assert!(matches(&named, "tab-one"));
        assert!(matches(&named, "ws"));
        assert!(matches(&named, "claude"));
        assert!(!matches(&named, "nope"));
        // Through the reducer: a name-only filter narrows to that record.
        let mut s = state_of(vec![
            named,
            rec("t2", TODAY_AT, "tab-two", "ws", "codex", 15),
        ]);
        assert_eq!(reduce(&mut s, Event::Char('/')), None);
        for c in "sprint".chars() {
            assert_eq!(reduce(&mut s, Event::Char(c)), None);
        }
        assert_eq!(labels(&s), vec![TODAY, "tab-one"]);
        // Nameless record: the row shows the label, the name filter misses.
        let r = rec("t1", TODAY_AT, "tab-one", "ws", "claude", 21);
        let line = tab_line(&r, false, 80, now_ts());
        assert!(line.contains("tab-one"), "{line:?}");
        assert!(matches(&r, "tab-one"));
        assert!(!matches(&r, "sprint"));
    }

    #[test]
    fn render_empty_and_too_small() {
        let s = state_of(vec![]);
        let (lines, highlight) = render(&s);
        assert_eq!(lines, vec!["No archived tabs.", "Press any key to close."]);
        assert_eq!(highlight, None);
        let mut s = state_of(eleven());
        s.height = 4;
        let (lines, highlight) = render(&s);
        assert_eq!(lines, vec![TOO_SMALL]);
        assert_eq!(highlight, None);
    }

    #[test]
    fn render_short_heights() {
        let mut s = state_of(eleven());
        s.height = 14;
        let (lines, _) = render(&s);
        assert_eq!(lines.len(), 14); // 10 rows + markers, no details
        assert_eq!(lines[13], KEYS_HINT);
        s.height = 10;
        let (lines, _) = render(&s);
        assert_eq!(lines.len(), 10); // 8 rows, no markers/details
    }

    struct FakeArch {
        records: std::cell::RefCell<Vec<Value>>,
        deleted: std::cell::RefCell<Vec<String>>,
        dir: PathBuf,
        _guard: crate::testutil::TempDir,
    }

    impl FakeArch {
        fn new(records: Vec<Value>) -> Self {
            let (guard, dir) = crate::testutil::tempdir();
            FakeArch {
                records: std::cell::RefCell::new(records),
                deleted: std::cell::RefCell::new(vec![]),
                dir,
                _guard: guard,
            }
        }
    }

    impl Arch for FakeArch {
        fn list(&self) -> Vec<Value> {
            self.records.borrow().clone()
        }
        fn delete(&self, id: &str) {
            self.deleted.borrow_mut().push(id.to_string());
            self.records.borrow_mut().retain(|r| r["id"] != id);
        }
        fn state_dir(&self) -> PathBuf {
            self.dir.clone()
        }
    }

    #[test]
    fn apply_restore_delete_quit() {
        let arch = FakeArch::new(eleven());
        let mut s = state_of(eleven());
        assert!(apply(
            &mut s,
            Some(Action::Quit),
            &arch,
            &|_, _| RestoreOutcome::Ok(vec![]),
            &|_| false,
            &|_, _| {},
            &|_| {}
        ));
        assert!(!apply(
            &mut s,
            None,
            &arch,
            &|_, _| RestoreOutcome::Ok(vec![]),
            &|_| false,
            &|_, _| {},
            &|_| {}
        ));
        // restore closes, warnings notified
        let notified = std::cell::RefCell::new(vec![]);
        let closed = apply(
            &mut s,
            Some(Action::Restore(RestoreRequest::undecided("t1".to_string()))),
            &arch,
            &|id, target| {
                assert_eq!(id, "t1");
                assert_eq!(target, RestoreTarget::New);
                RestoreOutcome::Ok(vec!["w1".to_string(), "w2".to_string()])
            },
            &|_| false,
            &|t, b| notified.borrow_mut().push((t.to_string(), b.to_string())),
            &|_| {},
        );
        assert!(closed);
        assert_eq!(
            notified.borrow().as_slice(),
            &[("herdr-archive".to_string(), "w1; w2".to_string())]
        );
        // failed restore keeps the popup
        let closed = apply(
            &mut s,
            Some(Action::Restore(RestoreRequest::undecided("t1".to_string()))),
            &arch,
            &|_, _| RestoreOutcome::Failed("boom".to_string()),
            &|_| false,
            &|_, _| {},
            &|_| {},
        );
        assert!(!closed);
        assert_eq!(s.message, "Restore failed: boom");
        // busy restore is friendly
        let closed = apply(
            &mut s,
            Some(Action::Restore(RestoreRequest::undecided("t1".to_string()))),
            &arch,
            &|_, _| RestoreOutcome::Busy,
            &|_| false,
            &|_, _| {},
            &|_| {},
        );
        assert!(!closed);
        assert_eq!(s.message, SWEEP_BUSY);
        // delete removes the entry
        let closed = apply(
            &mut s,
            Some(Action::Delete("t1".to_string())),
            &arch,
            &|_, _| RestoreOutcome::Ok(vec![]),
            &|_| false,
            &|_, _| {},
            &|_| {},
        );
        assert!(!closed);
        assert_eq!(arch.deleted.borrow().as_slice(), &["t1".to_string()]);
        assert_eq!(s.total(), 10);
    }

    #[test]
    fn apply_ignores_sigint_during_restore() {
        unsafe extern "C" fn marker(_: libc::c_int) {}
        let before = unsafe { libc::signal(libc::SIGINT, marker as *const () as usize) };
        let arch = FakeArch::new(eleven());
        let mut s = state_of(eleven());
        let saw = std::cell::Cell::new(false);
        apply(
            &mut s,
            Some(Action::Restore(RestoreRequest::undecided("t1".to_string()))),
            &arch,
            &|_, _| {
                let prev = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
                saw.set(prev == libc::SIG_IGN);
                RestoreOutcome::Ok(vec![])
            },
            &|_| false,
            &|_, _| {},
            &|_| {},
        );
        assert!(saw.get());
        let after = unsafe { libc::signal(libc::SIGINT, before) };
        assert_eq!(after, marker as *const () as usize);
    }

    #[test]
    fn dst_uses_offset_of_own_date() {
        unsafe extern "C" {
            fn tzset();
        }
        unsafe { std::env::set_var("TZ", "America/Los_Angeles") };
        unsafe { tzset() };
        // DST ends 2026-11-01. 07:30Z Nov 1 is 00:30 PDT: same local day as
        // 22:59 PST Nov 1... now is Nov 2 06:59Z == Nov 1 22:59 PST.
        let now = parse_iso(Some("2026-11-02T06:59:00Z")).unwrap();
        let now_days = local_days_with(now, local_offset(now));
        let r = serde_json::json!({"archived_at": "2026-11-01T07:30:00Z"});
        assert_eq!(group_of(&r, now_days, machine_offset), TODAY);
        // shelved line uses PDT (-7) for the Oct 30 stamp
        let at = parse_iso(Some("2026-10-30T16:14:00Z")).unwrap();
        assert_eq!(shelved_local(at, local_offset(at)), "shelved Oct 30 09:14");
        unsafe { std::env::remove_var("TZ") };
        unsafe { tzset() };
    }

    fn rec_with_id(id: &str, ws_label: &str, ws_id: &str) -> Value {
        let mut r = rec(id, TODAY_AT, id, ws_label, "claude", 1);
        r["workspace"]["workspace_id"] = serde_json::json!(ws_id);
        r
    }

    #[test]
    fn undecided_restore_prompts_only_when_original_live() {
        let records = vec![rec_with_id("t1", "backend", "w1")];
        let arch = FakeArch::new(records.clone());
        // original live: prompt, no restore yet
        let mut s = state_of(records.clone());
        let restored = std::cell::Cell::new(false);
        let closed = apply(
            &mut s,
            Some(Action::Restore(RestoreRequest::undecided("t1".to_string()))),
            &arch,
            &|_, _| {
                restored.set(true);
                RestoreOutcome::Ok(vec![])
            },
            &|id| {
                assert_eq!(id, "w1");
                true
            },
            &|_, _| {},
            &|_| {},
        );
        assert!(!closed);
        assert!(!restored.get());
        assert_eq!(s.mode, Mode::Target);
        assert_eq!(
            s.pending_restore,
            Some(PendingRestore {
                id: "t1".to_string(),
                workspace_id: "w1".to_string(),
                workspace_label: "backend".to_string(),
            })
        );
        let (lines, _) = render(&s);
        assert!(
            lines.iter().any(|l| l.contains("[Enter] new workspace")
                && l.contains("[o] original")
                && l.contains("backend")),
            "{lines:?}"
        );
        // original gone: straight to a new workspace, no prompt
        let mut s = state_of(records.clone());
        let saw = std::cell::RefCell::new(None);
        let closed = apply(
            &mut s,
            Some(Action::Restore(RestoreRequest::undecided("t1".to_string()))),
            &arch,
            &|id, target| {
                *saw.borrow_mut() = Some((id.to_string(), target));
                RestoreOutcome::Ok(vec![])
            },
            &|_| false,
            &|_, _| {},
            &|_| {},
        );
        assert!(closed);
        assert_eq!(
            saw.borrow().clone(),
            Some(("t1".to_string(), RestoreTarget::New))
        );
    }

    #[test]
    fn legacy_record_without_id_restores_new_without_check() {
        // records with no recorded id (old/Python) never consult liveness
        let records = vec![rec("t1", TODAY_AT, "x", "b", "claude", 1)];
        let arch = FakeArch::new(records.clone());
        let mut s = state_of(records);
        let saw = std::cell::RefCell::new(None);
        let closed = apply(
            &mut s,
            Some(Action::Restore(RestoreRequest::undecided("t1".to_string()))),
            &arch,
            &|id, target| {
                *saw.borrow_mut() = Some((id.to_string(), target));
                RestoreOutcome::Ok(vec![])
            },
            &|_| panic!("liveness must not be consulted without a recorded id"),
            &|_, _| {},
            &|_| {},
        );
        assert!(closed);
        assert_eq!(s.mode, Mode::Move);
        assert_eq!(
            saw.borrow().clone(),
            Some(("t1".to_string(), RestoreTarget::New))
        );
    }

    #[test]
    fn target_choice_keys() {
        let records = vec![rec_with_id("t1", "backend", "w1")];
        // Enter = new workspace (the default)
        let mut s = state_of(records.clone());
        s.mode = Mode::Target;
        s.pending_restore = Some(PendingRestore {
            id: "t1".to_string(),
            workspace_id: "w1".to_string(),
            workspace_label: "backend".to_string(),
        });
        assert_eq!(
            reduce(&mut s, Event::Enter),
            Some(Action::Restore(RestoreRequest {
                id: "t1".to_string(),
                target: Some(RestoreTarget::New),
            }))
        );
        assert_eq!(s.mode, Mode::Move);
        assert_eq!(s.pending_restore, None);
        // o = original workspace
        let mut s = state_of(records.clone());
        s.mode = Mode::Target;
        s.pending_restore = Some(PendingRestore {
            id: "t1".to_string(),
            workspace_id: "w1".to_string(),
            workspace_label: "backend".to_string(),
        });
        assert_eq!(
            reduce(&mut s, Event::Char('o')),
            Some(Action::Restore(RestoreRequest {
                id: "t1".to_string(),
                target: Some(RestoreTarget::Existing("w1".to_string())),
            }))
        );
        // Esc / q cancel like the delete confirm
        for ev in [Event::Esc, Event::Char('q'), Event::Char('x')] {
            let mut s = state_of(records.clone());
            s.mode = Mode::Target;
            s.pending_restore = Some(PendingRestore {
                id: "t1".to_string(),
                workspace_id: "w1".to_string(),
                workspace_label: "backend".to_string(),
            });
            assert_eq!(reduce(&mut s, ev), None);
            assert_eq!(s.mode, Mode::Move);
            assert_eq!(s.pending_restore, None);
        }
    }

    #[test]
    fn decided_restore_skips_the_prompt() {
        // a decided request goes straight through, whatever liveness says
        let records = vec![rec_with_id("t1", "backend", "w1")];
        let arch = FakeArch::new(records.clone());
        let mut s = state_of(records);
        let saw = std::cell::RefCell::new(None);
        let closed = apply(
            &mut s,
            Some(Action::Restore(RestoreRequest {
                id: "t1".to_string(),
                target: Some(RestoreTarget::Existing("w1".to_string())),
            })),
            &arch,
            &|id, target| {
                *saw.borrow_mut() = Some((id.to_string(), target));
                RestoreOutcome::Ok(vec![])
            },
            &|_| panic!("decided requests must not re-check liveness"),
            &|_, _| {},
            &|_| {},
        );
        assert!(closed);
        assert_eq!(
            saw.borrow().clone(),
            Some(("t1".to_string(), RestoreTarget::Existing("w1".to_string())))
        );
    }
}
