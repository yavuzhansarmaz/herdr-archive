# herdr-archive

Park [herdr](https://herdr.dev) tabs to free memory without losing them.
Each archive is a named restore point — layout, working directories, agent
sessions — that brings the tab back exactly where you left off, whenever
you return.

Agent tabs pile up and eat RAM. herdr has no undo for a closed tab, so
closing them by hand means losing the conversation for good. Archive closes
them for you — by hand with a key, or automatically when idle for days — and
keeps everything needed to rebuild them: the split layout, labels, working
directories, each agent's session id, and the command line the agent was
started with.

Archive coexists with the Python Shelf plugin: it installs as
`herdr-archive`, keeps its state under `plugins/herdr-archive`, and reads the same
config format. Archive records are cross-compatible both ways.

## Requirements

- herdr 0.9.0 or newer. Linux and macOS.
- A Rust toolchain (`cargo`, edition 2024) to build from source. There are
  no runtime dependencies beyond the `herdr-archive` binary.

## Quick start

```sh
herdr plugin install yavuzhansarmaz/herdr-archive
```

or from a checkout:

```sh
git clone <this-repo> herdr-archive
cd herdr-archive
cargo build --release --locked
herdr plugin link "$PWD"
```

Add a key for the restore picker to `~/.config/herdr/config.toml`, then run
`herdr server reload-config`:

```toml
[[keys.command]]
key = "prefix+shift+s"
type = "plugin_action"
command = "herdr-archive.restore"
description = "restore an archived tab"
```

`prefix+shift+s` is unbound in herdr's default keymap (`prefix+s` is
settings). Many macOS terminals turn alt chords into characters instead of
sending them as key events, so `prefix+alt+...` bindings often do nothing
there.

To also archive the current tab on demand (see
[Archive a tab now](#archive-a-tab-now)), add a second key; `prefix+shift+a`
is unbound in the default keymap too:

```toml
[[keys.command]]
key = "prefix+shift+a"
type = "plugin_action"
command = "herdr-archive.archive-tab"
description = "archive the current tab"
```

## Actions and panes

Actions (bind these as `herdr-archive.<id>`):

| Action id | Title | Does |
|---|---|---|
| `restore` | Archive: restore an archived tab | Open the restore picker |
| `archive-tab` | Archive: archive this tab | Ask, then archive the current tab |
| `sweep-now` | Archive: sweep now | Sweep immediately, ignoring the interval |

Popup panes: `picker` (the restore list, 18 rows high) and
`archive-confirm` (the yes/no question). Hooks: `sweep --if-due` runs at
herdr startup and on workspace focus; `track` runs on agent status and agent
detected events.

## It starts in dry-run

Out of the box Archive only reports. Each sweep shows a notification like
`herdr-archive (dry-run): would archive 3 tabs: fix-retries, docs-pass,
perf-probe`. When the list looks right, turn archiving on in `config.json`
(below) with `"mode": "live"`.

## What gets archived

A tab is archived when all of these hold:

- it has at least one agent pane;
- every agent pane has a session id reported by herdr's official integration
  for that agent, and no agent activity for `idle_days` (default 7);
- nothing in the tab is `working`;
- it is not the focused tab.

A blocked agent (waiting on you) follows the same rule. A sweep never
touches a tab with no agent. Shell panes inside an archived agent tab come
back as shells in the same directory.

Sweeps run at herdr startup and on focus changes, at most once per
`sweep_interval_minutes` (default 60).

## How activity is measured

For every agent herdr detects, Archive records activity when the agent goes to
`working`, `blocked` or `done`. Looking at a finished agent (`idle`) does
not count.

Starting or resuming an agent in a pane also counts as activity -- for
example resuming an old conversation by hand with `claude --resume <id>`,
or an agent that restarts on its own -- even though that alone sends no
message and would otherwise leave the session looking exactly as idle as
before. This survives herdr restarting too: it is remembered against the
conversation itself, not just the pane it happened in.

For Claude Code and Codex it also reads the agent's own session files, so
tabs that were already inactive before you installed Archive can qualify on
the first sweep. For other agents the clock starts when Archive first sees
the session, so their tabs qualify no earlier than `idle_days` after
install.

## Restore

Press your picker key. The popup groups archived tabs by when they were
archived: Archived today, Last 7 days, Last 30 days, and Older. Older starts
collapsed, unless it is the only group. The list shows 10 rows and scrolls.
Each row shows the archive's name, or the tab label for archives made
before naming existed (or by the Python shelf).
A line under it shows the highlighted tab's directory, when it was shelved,
and its panes.

| Key | Does |
|---|---|
| Up/Down (or k/j), PgUp/PgDn, Home/End | Move |
| Enter | Restore the highlighted tab, or open/close a group |
| Right/Left (or l/h) | Open/close a group |
| `/` | Filter by archive name, tab, workspace or agent name; Enter keeps it, Esc clears it |
| `d` | Delete the highlighted tab |
| q or Esc | Close |

A click highlights a row, a double-click restores it, a click on a group
header opens or closes it, and the wheel scrolls. Deleting asks for
confirmation (`[y/N]`, default no) and is permanent: there is no undo.

A restored tab goes into a brand-new workspace (same label) with the
same splits, labels and directories — workspaces are never matched by
name, since names need not be unique. Each agent is started again with
its original command line plus that agent's resume arguments, so the
conversation continues. When the agent exits you are left at a shell.
If the tab's original workspace still exists, the restore picker offers
it as a choice (`[Enter] new workspace, [o] original`); the
`restore <id>` command never asks and always creates a new workspace.

Claude Code deletes conversation files 30 days after they were last written
(`cleanupPeriodDays`). Archive keeps a copy of each archived Claude
conversation and puts it back on restore if Claude has removed it. For other
agents, their own retention applies.

## Archive a tab now

Press your archive key (above) in the tab you are done with. Archive asks
first:

```
 Archive "api-refactor"?
 Last activity 3 days ago.
 The tab closes; the restore picker brings it back.
 y archive   any other key cancel
```

`y` archives and closes the tab; any other key, including Esc, cancels.
After any session questions, the popup asks for a name for the archive:

```
Archive name [api-refactor]:
```

Enter keeps the tab's label; typing a name (up to 80 characters) stores it
with the archive, and the restore picker, `list`, and notifications show it
instead of the label. The tab label itself is kept untouched, so archives
stay copy-compatible with the Python shelf.
For muse panes with no reported session, Archive first checks the live
process instead of asking: a `resume <id>` command line, or (Linux only) an
open `session.jsonl` handle — or, when no log is held open, exactly one
`.session.lock` — identifies the session silently. Two or more distinct
locks holding validated sessions stays ambiguous: the picker then lists
exactly those sessions (`resolved_by` `detect:ambiguous`) instead of the
scanner history. Anything else uncertain falls back to the picker as
before. Records note the source in `resolved_by` (`detect:argv`,
`detect:fd`, `detect:lock`, `detect:ambiguous`).
Unlike a sweep, this ignores `idle_days` and dry-run mode, and archives the
tab you are looking at. The question also warns about anything unusual, and
`y` still archives:

- a pane is still working (archiving stops it);
- the tab has no agent, or a pane has no session id or runs an agent Archive
  cannot resume (that pane comes back as a shell);
- the conversation is open in two panes here (both come back resuming it),
  or in another tab (restore waits until that copy is closed).

It refuses, with a notification instead of the question, only when the entry
could not be restored correctly: a pane carrying another agent's session, or
an invalid session id.

## Supported agents

Every agent herdr can resume after a restart:

claude, codex, copilot, devin, droid, kimi, mastracode, pi, omp, hermes,
opencode, qodercli, qwen, kilo, cursor, agy, grok, letta, muse, gemini,
cline, kiro, amp, maki.

The resume arguments mirror herdr's own table. To change one, or add an
agent, use `agents` in `config.json`.

Six kinds are **experimental**: herdr does not report a session id for
muse, gemini, cline, kiro, amp or maki panes yet, so their tabs are archived
by hand (the archive-tab popup scans the agent's session store and asks
which session to resume) and automatic sweeps skip them. The muse row is
verified against the muse CLI (`muse resume <id>`); the other five could
not be verified locally (their CLIs are not installed here), so their resume
forms, store layouts and scanners are best-effort guesses marked
experimental in the source, with fixture-based tests. If you use one of
them, please verify against the real CLI and report back.

## Rust vs Python

Differences from the Python shelf (plugin id `shelf`, 19 agents, 1
scanner). Both plugins install side by side; neither reads the other's
state.

- One `herdr-archive` binary, no interpreter. Same subcommands, same exit
  codes, same `config.json` format.
- Resume table: 24 kinds vs 19. The 5 new rows (gemini, cline, kiro,
  amp, maki) are experimental: their CLIs are not installed here, so the
  resume forms are best-effort and fixture-tested only. The other 19
  rows match herdr's table (muse verified against the muse CLI).
- Scanners: 6 (one per never-reporting kind) vs 1 (muse only). Store
  paths are listed below.
- Manual resolution is generalized: the archive-tab popup offers
  scanner candidates for any of the 6 kinds (Python: muse only). Scanners
  never run during auto-sweep on either side. A pasted session id is
  verified against the agent's local session store (muse accepts a UUID
  or a Session Name) and rejected with a reason when unknown, so a typo
  can no longer archive a session that will not resume.
- Additive record fields: `panes[<id>].resume_argv`,
  `panes[<id>].resolved_by` (`herdr`, `scanner:<kind>`, `manual-paste`,
  `shell`, `detect:*`), top-level `tool: "herdr-archive <version>"`, top-level `name`
  (the user-given archive name; the picker falls back to the tab label
  when it is absent), `workspace.workspace_id` (the original workspace
  id, offered as a restore choice when still live). Restore replays a
  recorded `resume_argv` verbatim after validating it against herdr's
  rules, falling back to the table when invalid; nothing writes that
  field today (herdr 0.9.3 exposes no agent-reported argv), so it is
  forward-compat only.
- Shell fallback unchanged: panes without a session restore as shells
  in the same directory.

Compat:

- Archives are copy-compatible both ways. `record.json` stays
  `version: 1` with the same required keys, so an archive directory can
  be copied between the two plugins' `archive/` trees and just works.
  Rust-to-Python restores rebuild argv from the Python table and ignore
  `resume_argv`; a pane whose only resume path is a verbatim argv comes
  back as a shell under Python.
- State dirs are separate: `herdr/plugins/herdr-archive` vs
  `herdr/plugins/shelf` (own activity store, locks, log, archives).
  There is no automatic import; copy archive directories by hand.
- Keybindings are distinct: `herdr-archive.restore` /
  `herdr-archive.archive-tab` / `herdr-archive.sweep-now` vs the `shelf.*` names.

Benchmarks, measured 2026-10-08 on CachyOS, Ryzen 9 4900HS (16
threads), 15 GB RAM, rustc 1.99.0, Python 3.12.13:

| | herdr-archive | shelf (Python) |
|---|---|---|
| `list`, min / median (N=10) | 1.0 / 1.1 ms | 48.5 / 50.6 ms |
| `sweep` dry-run, min / median (N=5) | 2.6 / 3.5 ms | 51.3 / 77.7 ms |
| Peak RSS, one `sweep` | 13,472 KB | 21,328 KB |
| Footprint | 1.8 MB binary | 420 KB `shelf/` package (1.7 MB checkout) |
| Test suite | 123 tests, 15.7 s | 534 tests, 18.8 s |

Methodology: interleaved back-to-back runs so both sides share machine
conditions; `list` with empty archives on both sides (first of 11 runs
discarded as warmup); `sweep` on 3 live single-pane tabs with default
dry-run configs (no `config.json` on either side; both reported
"nothing to archive", nothing was archived or closed). RSS is
`RUSAGE_CHILDREN` max RSS of one `sweep` in a fresh process
(`/usr/bin/time` is not installed here). Test wall time is
`cargo test --locked --offline` with a warm cache vs
`python3 -m unittest discover -s tests -t .`. What dominates: process
startup, not socket RTT — `sweep` exceeds `list` by about 2 ms on both
sides (two socket calls over 3 tabs), so the gap is interpreter startup
(~49 ms) vs native startup (~1 ms). Both test-suite walls are
sleep-bound (lock-wait and timing tests), not compute-bound.

Support matrix. "herdr" rows resume from the session id herdr reports;
"manual" rows get no session id from herdr, so auto-sweep skips them
and the archive-tab popup resolves the session via the scanner, a
paste, or an explicit shell.

| Agent | Program | Resume argv | Resume source | Scanner | Status |
|---|---|---|---|---|---|
| claude | claude | `--resume {id}` | herdr | — | herdr table |
| codex | codex | `resume {id}` | herdr | — | herdr table |
| copilot | copilot | `--resume={id}` | herdr | — | herdr table |
| devin | devin | `--resume {id}` | herdr | — | herdr table |
| droid | droid | `--resume {id}` | herdr | — | herdr table |
| kimi | kimi | `--session {id}` | herdr | — | herdr table |
| mastracode | mastracode | `--thread {id}` | herdr | — | herdr table |
| pi | pi | `--session {id}` | herdr | — | herdr table |
| omp | omp | `--resume={id}` | herdr | — | herdr table |
| hermes | hermes | `--resume {id}` | herdr | — | herdr table |
| opencode | opencode | `--session {id}` | herdr | — | herdr table |
| qodercli | qodercli | `--resume {id}` | herdr | — | herdr table |
| qwen | qwen | `--resume {id}` | herdr | — | herdr table |
| kilo | kilo | `--session {id}` | herdr | — | herdr table |
| cursor | cursor-agent | `--resume {id}` | herdr | — | herdr table |
| agy | agy | `--conversation {id}` | herdr | — | herdr table |
| grok | grok | `--resume {id}` | herdr | — | herdr table |
| letta | letta | `--conversation {id}` (`--conversation default --agent <a>` for `default:<a>`) | herdr | — | herdr table |
| muse | muse | `resume {id}` | manual | muse | verified vs CLI |
| gemini | gemini | `--resume {id}` | manual | gemini | experimental |
| cline | cline | `resume {id}` (placeholder) | manual | cline | experimental |
| kiro | kiro-cli | `chat --resume-id {id}` | manual | kiro | experimental |
| amp | amp | `threads continue {id}` | manual | amp | experimental |
| maki | maki | `--resume {id}` | manual | maki | experimental |

Scanner stores (`src/scan/`; muse ported as-is, the rest experimental):

| Kind | Store |
|---|---|
| muse | `$XDG_DATA_HOME/muse/sessions/YYYY/MM/DD/<id>/session.jsonl` (else `~/.local/share/…`); workspace match on `"workspace_root":"<cwd>"` |
| gemini | `~/.gemini/tmp/*/chats/session-*.json`; id and cwd parsed from the JSON |
| cline | `${CLINE_SESSION_DATA_DIR:-~/.cline/data/sessions}/<id>/<id>.json` manifest (cwd) + `<id>.messages.json` |
| kiro | `~/.kiro/sessions/cli/<uuid>.json` metadata (cwd) + `.jsonl` + `.lock` (active wins ties) |
| maki | `${XDG_STATE_HOME:-~/.local/state}/maki/sessions/<base58>.jsonl`; cwd field when present, else newest-first |
| amp | No disk scan (server-authoritative threads); `amp threads list --json`, 5 s timeout, failures yield no candidates |

## Configuration

`config.json` in the directory printed by
`herdr plugin config-dir herdr-archive`. Every key is optional. Changes take
effect the next time a hook runs (the next status change, focus change, or
sweep); there is nothing to reload.

```json
{
  "idle_days": 7,
  "mode": "dry-run",
  "sweep_interval_minutes": 60,
  "keep_transcripts": true,
  "sessions": ["default"],
  "agents": {
    "qwen": {"relaunch": "plain"},
    "myagent": {"program": "myagent", "resume": ["--load", "{id}"], "strip": ["-l"]}
  }
}
```

- `mode`: `dry-run` reports only; `live` archives.
- `keep_transcripts`: keep copies of Claude conversation files in the
  archive.
- `sessions`: which herdr sessions Archive acts in (`"*"` allows all).
- `agents.<name>.program`: executable to look for and to run.
- `agents.<name>.resume`: resume arguments; `{id}` becomes the session id.
- `agents.<name>.strip` / `strip_bare`: flags (with or without a value)
  removed from the saved command line before resume arguments are appended.
- `agents.<name>.strip_subcommand`: a resume subcommand (like codex's
  `resume`) removed the same way.
- `agents.<name>.relaunch`: `"plain"` rebuilds the command from the table
  instead of reusing the saved command line.

## Updating

```sh
cd herdr-archive
git pull
cargo build --release --locked
```

The linked plugin runs `./target/release/herdr-archive` in place, so rebuilding
is all it takes. State and config live outside the checkout and survive
updates. Records written by older versions (or by the Python shelf) keep
working: the `record.json` schema only ever gains optional fields
(`resume_argv`, `resolved_by`, `tool`, `name`).

State is keyed by plugin id, so it does not follow an id change on its
own. To move archives and activity from another install (the Python
shelf, or this plugin under a previous id), stop herdr's sweeps, then
copy the per-session trees by hand, e.g.

```sh
cp -r ~/.local/state/herdr/plugins/shelf/sessions/* \
      ~/.local/state/herdr/plugins/herdr-archive/sessions/
```

and check the result with `herdr-archive list` (run from the checkout's
`target/release`, or via the linked plugin).

## Development

```sh
cargo fmt --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo test --locked --offline
cargo build --release --locked --offline
```

Dependencies are pinned (`=` in `Cargo.toml`, committed `Cargo.lock`), and
the build works offline from the cargo cache. `DESIGN.md` is the
authoritative spec: module map, the 24-kind table, the session-discovery
architecture, and the unresolved-questions log.

Cross-compat fixtures under `tests/fixtures/` are real `record.json` files
written by the Python shelf's own `capture()`; regenerate them with
`python3 tests/fixtures/gen_fixtures.py tests/fixtures/` from that checkout
(see `tests/fixtures/README.md`).

## Troubleshooting

- `herdr-archive: nothing to archive` on every sweep: expected in dry-run with
  no idle tabs. Check the plugin log (`herdr plugin log list`) for per-tab
  skip reasons.
- A tab you expect to qualify is skipped for "activity unknown": the agent
  only ever sat `idle` there. Activity starts counting once the agent goes
  `working`, `blocked` or `done`.
- "A sweep is running; try again in a moment": a sweep or restore holds the
  lock; retries are safe.
- Archive records live under
  `~/.local/state/herdr/plugins/herdr-archive/sessions/<session>/archive/`, one
  directory per tab; `herdr-archive list` prints them. The log is
  `~/.local/state/herdr/plugins/herdr-archive/herdr-archive.log`.
- A sweep that finds invalid `config.json` notifies at most once an hour.

## Credits

Behavioral design started from
[herdr-shelf](https://github.com/anilkmr-a2z/herdr-shelf)
(MIT, © 2026 Anil Kumar) as a reference; this Rust implementation — 24-kind
support, session discovery, named archives, and the rest — was written fresh
for this project. `LICENSE` carries the original MIT text.
