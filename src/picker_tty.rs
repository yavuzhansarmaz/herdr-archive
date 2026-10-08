//! crossterm event loop around the pure picker: keys, mouse, resize.
//!
//! Same key/mouse map as the curses version: alternate screen + raw mode +
//! mouse capture, hidden cursor, 25 ms escape timing, reverse-video cursor
//! row. Double-click is synthesized (two left releases on one row within
//! 500 ms) since crossterm reports no double-click event itself.

use crate::picker::{self, Action, Arch, Event, RestoreOutcome, State};
use crate::restore::RestoreTarget;
use crate::util::Ts;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use std::io::Write;
use std::time::{Duration, Instant};

/// Translate one crossterm event. `click` tracks the last left release for
/// double-click synthesis: (row, when, down_row, dragged).
pub fn translate(
    ev: CEvent,
    state: &State,
    click: &mut Option<(usize, Instant)>,
    press: &mut Option<(usize, bool)>,
) -> Option<Event> {
    match ev {
        CEvent::Key(k) => {
            if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                return None;
            }
            // Ctrl-C / Ctrl-D close the popup quietly (KeyboardInterrupt/EOF
            // in the curses version).
            if k.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('d'))
            {
                return Some(Event::Char('q'));
            }
            // Alt+key is ignored rather than closing the popup.
            if k.modifiers.contains(KeyModifiers::ALT) {
                return Some(Event::Other);
            }
            Some(match k.code {
                KeyCode::Up => Event::Up,
                KeyCode::Down => Event::Down,
                KeyCode::Left => Event::Left,
                KeyCode::Right => Event::Right,
                KeyCode::PageUp => Event::PgUp,
                KeyCode::PageDown => Event::PgDn,
                KeyCode::Home => Event::Home,
                KeyCode::End => Event::End,
                KeyCode::Enter => Event::Enter,
                KeyCode::Backspace => Event::Backspace,
                KeyCode::Esc => Event::Esc,
                KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => Event::Char(c),
                KeyCode::Char(_) => Event::Other,
                _ => Event::Other,
            })
        }
        CEvent::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => Some(Event::WheelUp),
            MouseEventKind::ScrollDown => Some(Event::WheelDown),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = picker::window_row(state, m.row as usize);
                *press = row.map(|r| (r, false));
                None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let row = picker::window_row(state, m.row as usize)?;
                // A drag (press-move-release, e.g. selecting text) is not a click.
                let (down_row, dragged) = (*press)?;
                if dragged || down_row != row {
                    *press = None;
                    return None;
                }
                *press = None;
                let now = Instant::now();
                if let Some((last_row, when)) = *click {
                    if last_row == row && now.duration_since(when) < Duration::from_millis(500) {
                        *click = None;
                        return Some(Event::DClick(row));
                    }
                }
                *click = Some((row, now));
                Some(Event::Click(row))
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(p) = press {
                    p.1 = true;
                }
                None
            }
            _ => None,
        },
        CEvent::Resize(w, h) => Some(Event::Resize {
            h: h as usize,
            w: w as usize,
        }),
        _ => None,
    }
}

struct TtyGuard;

impl Drop for TtyGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(out, DisableMouseCapture, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(out, crossterm::cursor::Show);
    }
}

fn draw(state: &State) {
    let (lines, highlight) = picker::render(state);
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(
        out,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    );
    for (y, line) in lines.iter().enumerate().take(state.height) {
        let _ = crossterm::execute!(out, crossterm::cursor::MoveTo(0, y as u16));
        if Some(y) == highlight {
            let _ = crossterm::execute!(
                out,
                crossterm::style::SetAttribute(crossterm::style::Attribute::Reverse)
            );
            // Pad the highlighted row so the reverse bar spans the width,
            // like the curses version's ljust.
            let padded = format!("{:width$}", line, width = state.width.saturating_sub(1));
            let _ = write!(
                out,
                "{padded:.width$}",
                width = state.width.saturating_sub(1)
            );
            let _ = crossterm::execute!(
                out,
                crossterm::style::SetAttribute(crossterm::style::Attribute::NoReverse)
            );
        } else {
            let _ = write!(out, "{line}");
        }
        let _ = crossterm::execute!(
            out,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::UntilNewLine)
        );
    }
    let _ = out.flush();
}

/// Run the picker popup. `now_fn` supplies local-time "now".
pub fn run(
    arch: &dyn Arch,
    do_restore: &dyn Fn(&str, RestoreTarget) -> RestoreOutcome,
    workspace_live: &dyn Fn(&str) -> bool,
    now_fn: &dyn Fn() -> Ts,
    notify: &dyn Fn(&str, &str),
) -> std::io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    crossterm::execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    let _guard = TtyGuard;
    let mut state = picker::initial_state(arch.list(), now_fn(), picker::machine_offset);
    if let Ok((w, h)) = terminal::size() {
        picker::reduce(
            &mut state,
            Event::Resize {
                h: h as usize,
                w: w as usize,
            },
        );
    }
    let mut click: Option<(usize, Instant)> = None;
    let mut press: Option<(usize, bool)> = None;
    loop {
        draw(&state);
        let ev = crossterm::event::read()?;
        if let Some(mapped) = translate(ev, &state, &mut click, &mut press) {
            let action: Option<Action> = picker::reduce(&mut state, mapped);
            if picker::apply(
                &mut state,
                action,
                arch,
                do_restore,
                workspace_live,
                notify,
                &draw,
            ) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::parse_iso;

    fn state() -> State {
        let now = parse_iso(Some("2026-09-29T17:00:00Z")).unwrap();
        picker::initial_state(vec![], now, |_| -7 * 3600)
    }

    fn key(code: KeyCode) -> CEvent {
        CEvent::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn key_translation() {
        let s = state();
        let mut click = None;
        let mut press = None;
        let mut t = |ev| translate(ev, &s, &mut click, &mut press);
        assert_eq!(t(key(KeyCode::Up)), Some(Event::Up));
        assert_eq!(t(key(KeyCode::Down)), Some(Event::Down));
        assert_eq!(t(key(KeyCode::PageUp)), Some(Event::PgUp));
        assert_eq!(t(key(KeyCode::PageDown)), Some(Event::PgDn));
        assert_eq!(t(key(KeyCode::Home)), Some(Event::Home));
        assert_eq!(t(key(KeyCode::End)), Some(Event::End));
        assert_eq!(t(key(KeyCode::Enter)), Some(Event::Enter));
        assert_eq!(t(key(KeyCode::Backspace)), Some(Event::Backspace));
        assert_eq!(t(key(KeyCode::Esc)), Some(Event::Esc));
        assert_eq!(t(key(KeyCode::Char('q'))), Some(Event::Char('q')));
        assert_eq!(t(key(KeyCode::Char('/'))), Some(Event::Char('/')));
        assert_eq!(t(key(KeyCode::F(1))), Some(Event::Other));
        // Alt+key ignored
        let alt = CEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::ALT,
        ));
        assert_eq!(t(alt), Some(Event::Other));
        // Ctrl-C quits quietly
        let cc = CEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(t(cc), Some(Event::Char('q')));
    }

    #[test]
    fn wheel_and_resize() {
        let s = state();
        let mut click = None;
        let mut press = None;
        let mouse = |kind| {
            CEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 0,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert_eq!(
            translate(mouse(MouseEventKind::ScrollUp), &s, &mut click, &mut press),
            Some(Event::WheelUp)
        );
        assert_eq!(
            translate(
                mouse(MouseEventKind::ScrollDown),
                &s,
                &mut click,
                &mut press
            ),
            Some(Event::WheelDown)
        );
        assert_eq!(
            translate(CEvent::Resize(80, 24), &s, &mut click, &mut press),
            Some(Event::Resize { h: 24, w: 80 })
        );
    }

    #[test]
    fn click_double_click_and_drag() {
        let mut s = state();
        s.height = 16;
        s.width = 80;
        let mut click = None;
        let mut press = None;
        let mouse = |kind, row| {
            CEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 0,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        // press + release on row 2 (list row 0)
        assert_eq!(
            translate(
                mouse(MouseEventKind::Down(MouseButton::Left), 2),
                &s,
                &mut click,
                &mut press
            ),
            None
        );
        assert_eq!(
            translate(
                mouse(MouseEventKind::Up(MouseButton::Left), 2),
                &s,
                &mut click,
                &mut press
            ),
            Some(Event::Click(0))
        );
        // second release quickly: double-click
        assert_eq!(
            translate(
                mouse(MouseEventKind::Down(MouseButton::Left), 2),
                &s,
                &mut click,
                &mut press
            ),
            None
        );
        assert_eq!(
            translate(
                mouse(MouseEventKind::Up(MouseButton::Left), 2),
                &s,
                &mut click,
                &mut press
            ),
            Some(Event::DClick(0))
        );
        // release without press: not a click
        assert_eq!(
            translate(
                mouse(MouseEventKind::Up(MouseButton::Left), 3),
                &s,
                &mut click,
                &mut press
            ),
            None
        );
        // drag then release: not a click
        assert_eq!(
            translate(
                mouse(MouseEventKind::Down(MouseButton::Left), 4),
                &s,
                &mut click,
                &mut press
            ),
            None
        );
        assert_eq!(
            translate(
                mouse(MouseEventKind::Drag(MouseButton::Left), 5),
                &s,
                &mut click,
                &mut press
            ),
            None
        );
        assert_eq!(
            translate(
                mouse(MouseEventKind::Up(MouseButton::Left), 5),
                &s,
                &mut click,
                &mut press
            ),
            None
        );
    }
}
