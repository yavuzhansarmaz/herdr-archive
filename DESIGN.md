# herdr-archive — Design

Rust rewrite of the [herdr-shelf](https://github.com/) Python plugin (`shelf`
0.5.1 fork), behavior-compatible, coexisting side by side with the original.

Source material (read in full during design):

- `/tmp/HERDR-full.md` — how herdr resumes all 24 agent kinds (native
  session-ref path vs self-reported `resume_argv`; the 6 never-reporting
  kinds: gemini/cline/kiro/amp/maki/muse).
- `/tmp/SPEC-full.md` — complete functional spec of shelf 0.5.1.
- `/tmp/RUST-full.md` — how to ship a Rust herdr plugin others can install.
- Shelf source at `/home/yohansin/Desktop/PP/herdr-shelf` (read-only reference).

## 1. Plugin identity (decision)

| Field | Value |
|---|---|
| Plugin `id` | `herdr-archive` |
| `name` | `Archive` |
| Binary | `herdr-archive` (single binary, subcommands) |
| Action ids | `restore`, `archive-tab`, `sweep-now` (unchanged) |
| Pane ids | `picker`, `archive-confirm` (unchanged) |
| Default state dir suffix | `herdr/plugins/herdr-archive` |
| Default config dir suffix | `herdr/plugins/config/herdr-archive` |

Justification: herdr keys everything by plugin id (action keybindings
`<plugin-id>.<action-id>`, `--plugin` scoping, per-plugin state/config env),
and refuses install-over-link, so the id **must** differ from the Python
plugin's `shelf`. `herdr-archive` is the published name: repo name, plugin
id, and binary are all the same token, following the `herdr-<thing>`
community convention, so `herdr plugin install owner/herdr-archive` yields
id `herdr-archive` with no surprise. Name research (GitHub repo search,
`herdr-plugin` topic scan, crates.io) found no collision; the longer
keybinding prefix (`herdr-archive.restore`) is typed once into config.
Action/pane ids stay identical because they are plugin-scoped — keybinding
migration from the Python plugin is a one-token change
(`shelf.restore` → `herdr-archive.restore`), documented in the README.
Separate state/config dirs mean both plugins can run concurrently without
touching each other's locks, activity store, or archives; §3 covers how
archives still interoperate.

## 2. Goals / non-goals

Goals:

1. Behavior parity with shelf 0.5.1 per the spec: sweep/decide/assess,
   activity model, archive record schema, restore, picker, confirm flow,
   config, session gating, migrate, transcripts, notifications.
2. NOT muse-specific: a per-agent resume table covering **all 24** herdr
   kinds, and a general session-discovery architecture (§7) where muse is
   one scanner among several.
3. Ship as an installable herdr plugin per the Rust recommendations
   (`[[build]]`, prebuilts before announcing, marketplace card).

Non-goals for v1: Windows support (unix-only, `platforms = ["linux",
"macos"]`); auto-import of the Python shelf's state (§3, U10); transcript
copying beyond claude (§5); scanners for the 18 herdr-reporting kinds.

## 3. On-disk compatibility (decision)

### 3.1 Archive records: same schema, additive-only

**Decision: keep `version: 1` and the exact Python record shape; add only
optional fields that the Python shelf ignores.**

Verified against the Python source: `archive.py` *writes* `"version": 1`
but nothing in the codebase ever *reads* it — there is no version gate on
`load`/`list`. Restore reads only the keys it needs (`tab`, `workspace`,
`layout`, `panes[].{cwd,agent,session,launch_argv}`, `session_copies`,
`herdr_session`, `id`, `archived_at`). Unknown keys are therefore inert.
Consequence: a "versioned divergence" (e.g. `version: 2`) would **not** stop
the Python shelf from reading Rust archives anyway, so divergence buys no
safety and costs interop. Additive v1 is strictly better.

Additive optional fields (all `Option`, omitted when absent):

- `panes[<id>].resume_argv: [string]` — verbatim replay command honored
  from an agent-reported `resume_argv` (§7.1). Python ignores it and falls
  back to table-built argv (see asymmetry below).
- `panes[<id>].resolved_by: string` — provenance marker: `herdr`,
  `resume_argv`, `scanner:<kind>`, `manual-paste`, `shell`. Debugging aid.
- Top-level `tool: "herdr-archive <version>"` — provenance marker.
- Top-level `name: string` — user-given archive name, asked in the
  archive-confirm flow (default: tab label). Shown as the picker's row
  title and preferred by `list` and notifications; `tab.label` is kept
  untouched so Python-side restores and old records behave as before.
- `workspace.workspace_id: string` — the archived tab's workspace id.
  Restore never targets by label (labels need not be unique); the default
  target is always a brand-new workspace, and the picker offers the
  original workspace as a choice only when that id is still live (§7.4).

Interop matrix:

| Writer → reader | Result |
|---|---|
| Python → Rust | Full fidelity (Rust reads the whole v1 schema). |
| Rust → Python | Restores; per-pane argv rebuilt from the Python table, `resume_argv` ignored. For table-covered agents this is equivalent; for panes whose only resume path is a verbatim argv (self-reporting agents outside any table), Python archives them as shells. |

A Rust archive directory can therefore be copied into the Python shelf's
`archive/` (or vice versa) and just works; `id == dirname` and the
`YYYYMMDDTHHMMSSZ-xxxxxx` id format are unchanged.

### 3.2 State dirs: separate, same layout

Each plugin gets its own `<state>/sessions/<name>/` tree
(`HERDR_PLUGIN_STATE_DIR` is per-plugin; the XDG fallback uses the
`herdr-archive` suffix). The per-session layout is identical to Python's:
`archive/<id>/{record.json,sessions/…}`, `activity.json`, `activity.lock`,
`sweep.lock`, `last_sweep`, `installed_at`, plus root `herdr-archive.log`,
migrate/config-error markers. `activity.json` record fields are identical
(`first_seen`, `last_active`, `last_status`, `restored_at`,
`agent_started_at`, `terminals`), so activity stores are also copy-compatible.

No automatic import of Python-shelf state in v1 (see U10): silent
cross-plugin migration risks double-archiving races when both are installed.
The legacy `merge_into_default_session` migration (§5) is still ported, but
scoped to the `herdr-archive` tree (it guards the same pre-per-session layout
shape, harmless if never present).

## 4. Architecture / module map

One binary, subcommands mirroring the Python CLI (`track`, `sweep`,
`archive`, `open-archive`, `confirm-archive`, `open-picker`, `pick`, `list`,
`restore`). A `src/lib.rs` exposes the modules so integration tests can
drive logic without subprocesses.

```
src/
  main.rs      CLI dispatch, logging (file+stderr, rotation), config-error
               rate-limiting, session gating, hook exit-0 semantics
  api.rs       Unix-socket JSON-RPC client; HerdrError{code,message,definite}
  config.rs    config.json load/validate/defaults, sessions_for_gate, AGENT_KEYS
  session.rs   herdr_session_name(), allowlist matrix
  agents.rs    24-kind resume table + overrides, resume_args (letta default:),
               strip_resume, valid_session_value, relaunch_argv,
               matches_program, shell_command, prompt guard
  activity.rs  ActivityStore (JSON + flock), touch/record_status/see/
               mark_restored/effective/record_terminal_started, track()
  sweep.rs     decide/assess/gather/_open_sessions/_record_presence/
               _activity_lookup/is_due/run/_sweep/summary/archive_now/preview
  archive.rs   Archive store (save/list/delete/new_id), capture incl. layout
               guards + launch_argv + session_overrides + resolved_by,
               archive_tab incl. tab-changed/worktree rechecks
  restore.rs   restore() incl. live-session guard, build_tree (+resume_argv
               replay), RestoreTarget (New default / Existing id),
               apply-failure cleanup, log-before-delete, mark_restored
  picker.rs    pure State/reduce/render/apply (grouping, sort, filter, cursor,
               refresh, delete/restore intents) — no crossterm here
  picker_tty.rs  crossterm event loop: keys, wheel/click/dclick, resize,
               SIGINT-ignore during restore, popup-too-small
  confirm.rs   lines() wrap/sanitize, read_key (crossterm-based)
  manual.rs    generalized resolve_missing_sessions (any scanner kind)
  scan.rs      Scanner trait + registry; scan/gemini.rs, cline.rs, kiro.rs,
               maki.rs, muse.rs, amp.rs (CLI-backed)
  history.rs   transcript readers: claude, codex (+v1 additions, §5)
  migrate.rs   merge_into_default_session port (herdr-archive tree)
  util.rs      ISO-8601 parse/format, atomic JSON read/write, FileLock (flock)
tests/
  common/fakeherdr.rs  Rust fake herdr server (Unix socket stub)
  <per-module>_tests.rs  integration tests (see §11)
```

Data flow is unchanged from Python: every invocation migrates → gates on
session → dispatches; `track` updates activity; `sweep` gathers via
`tab.list`+`pane.list`, decides, archives; `pick`/`confirm-archive` run
inside plugin popups.

## 5. Behavior parity checklist

Each item is `SPEC §` → Rust target. “Same” = byte/logic parity including
constants; deviations are called out.

- CLI dispatch, hook exit-0, `LockBusy`/`HerdrError`/`Skip` handling (§1
  Entry) → `main.rs`. Same commands, same exit codes (0/1/2), same messages
  (`shelf: a sweep is running…` becomes `herdr-archive: …`; log tag `[herdr-archive]`).
- `sweep --if-due` default-interval pre-check before config load → same.
- Logging: file + stderr, rotation at 1 MiB, `_logs_off_stderr` during
  popups → same (file `herdr-archive.log`).
- Config-error hourly rate-limit (`config-error.lock` + marker) → same.
- `list` / `_describe`, `restore` usage/exit codes, `pick` lazy config +
  Enter-to-dismiss → same.
- State/config path resolution (`HERDR_PLUGIN_*` → XDG → home) → same,
  with `herdr-archive` suffixes.
- `decide` 7-rule chain, dup-pane/dup-tab, `open_in=None` skip → `sweep.rs`,
  same reason strings (used in logs/tests).
- `assess` blocks/warnings + activity line (none/today/N days, future
  clamp) → same.
- `gather` (one `tab.list` + one `pane.list`), `_open_sessions` → same.
- `_record_presence` (first_seen, working backstop, `agent_started_at`
  copy, terminal prune incl. concurrent-track race rule, non-dict reset) →
  same.
- `_activity_lookup` (max of effective + terminal started) → same.
- `is_due` (missing/unparseable → due), `_installed_at` write-once → same.
- `run` (pre+post-lock due check, `last_sweep` only if gather ok), `_sweep`
  (re-load/re-gather/re-decide per target, `Skip`→skipped, else failed) →
  same.
- `summary` dry-run/live formats → same (with `herdr-archive` prefix).
- `archive_now` (10 s lock wait, terminal-set `_match`, confirmed vs
  decide-with-zero-idle, `_warn_if_open_elsewhere`) → same.
- `preview` (no lock, double `pane.list` race check) → same.
- Activity keys/fields, `TOUCH_SKIP=60 s`, status-change rules,
  `effective()` (installed_at-gated first_seen), `track` (event shapes,
  10×0.5 s session retry, 5 s activity lock) → same.
- Archive store (atomic save + fsync + rmtree-on-failure, newest-first
  list, id regex, delete order, `new_id`) → same.
- `capture` (layout guards `MAX_LAYOUT_PANES=24`/`MAX_LAYOUT_DEPTH=16`,
  numeric-label drop, agent-vs-shell rule, `_launch_argv` outermost-match
  rule) → same, plus `resume_argv`/`resolved_by` capture (§7.1).
- `archive_tab` (terminal recheck, definite/indefinite close errors,
  `confirmation_required` worktree rule) → same.
- Restore: 30 s lock wait, `put_back_sessions`, claude/cwd warnings, label
  fallback, live-session guard (entry kept), `build_tree`, workspace
  targeting (always-new default; picker-confirmed id reuse; §7.4),
  apply-failure cleanup, log-before-delete, `mark_restored`,
  `finally delete` → same, plus verbatim `resume_argv` replay (§7.1).
- Picker: groups/sort/filter/keys/mouse/render/apply/refresh rules (§1
  Picker) → `picker.rs` + `picker_tty.rs`, same keybindings and layout math
  (`LIST_ROWS=10`, height rules, 5×30 minimum).
- Confirm flow: `lines()` (WIDTH=60, control-char sanitize), popup sizing
  + slack rule (14 when `pane.list` fails or any same-tab agent pane lacks
  a value), `read_key` (flush pre-typed, restore mode), SIGINT+SIGHUP
  ignore, `session_overrides` threading → same; `manual.rs` generalized to
  all scanner kinds (§7.3). V1 addition: after session resolution, the
  popup asks `Archive name [<label>]: ` (Enter = tab label, blank =
  re-ask, sanitized + capped at 80 chars) and stores it as the record's
  `name` (see §3.1).
- Config defaults/validation/`AGENT_KEYS`/sessions semantics → same file
  format (`config.json`); `agents` overrides merge into the 24-kind table.
- Session gating incl. name regex, None-never-allowed, per-command
  disabled matrix, unknown-tag logging → same.
- Migrate (detection, 10 s triple-lock, identical-vs-conflict,
  later-wins/dest-wins merge rules, marker rules, incomplete-gate matrix;
  `archive/` leftovers never gate; `list`/`track` unaffected) → same,
  scoped to the `herdr-archive` tree.
- Transcripts: claude/codex readers + `keep_transcripts` claude-only copy +
  `put_back_sessions` traversal guards → same. V1 addition: readers for
  scanner kinds where the format is trivially parseable JSON/JSONL with
  timestamps (gemini, muse, kiro, maki; see U9). Transcript *copying* stays
  claude-only in v1 (U8).
- Notifications: same titles/bodies/triggers, failures never fatal → same.
- Fork deltas: muse table entry, manual resolution, musescan, overrides
  threading, popup slack → all ported; musescan/manual generalized (§7).

Intentional deviations from Python (all justified, none silent):

1. `herdr-archive` naming in log prefix, user-visible strings, binary, dirs.
2. Additive record fields (§3.1) + `resume_argv` replay (§7.1).
3. Resume table extended 19 → 24 kinds (§6); scanners for 6 kinds (§7.2).
4. `confirm.read_key` via crossterm instead of termios (same UX).
5. Picker/mouse via crossterm instead of curses (same keybindings).

## 6. Resume table (all 24 kinds)

`agents.rs::BUILTIN` covers every `herdr agent start --kind` value. Fields
per entry: `program`, `resume` (with `{id}`), `strip`/`strip_bare`/
`strip_subcommand` (launch-argv cleanup), `relaunch`. Rows 1–18 mirror
herdr's `plan()` exactly (HERDR §2, cross-checked against the v0.9.3
session-state doc); row 19 is the fork's muse entry (verified against the
muse CLI); rows 20–24 are new, sourced from agent docs/CLI research.
They could NOT be verified during the v0 build (no CLIs installed locally)
and ship as EXPERIMENTAL rows + scanners with fixture-based tests only
(see U1–U5, §13).

| kind | program | resume argv template | ref kind | source |
|---|---|---|---|---|
| `pi` | `pi` | `pi --session {id}` | path preferred, id fallback | herdr `plan()` |
| `claude` | `claude` | `claude --resume {id}` | id | herdr `plan()` |
| `codex` | `codex` | `codex resume {id}` | id | herdr `plan()` |
| `cursor` | `cursor-agent` | `cursor-agent --resume {id}` | id | herdr `plan()` |
| `devin` | `devin` | `devin --resume {id}` | id | herdr `plan()` |
| `agy` | `agy` | `agy --conversation {id}` | id | herdr `plan()` |
| `omp` | `omp` | `omp --resume={id}` | path preferred, id fallback | herdr `plan()` |
| `mastracode` | `mastracode` | `mastracode --thread {id}` | id | herdr `plan()` |
| `opencode` | `opencode` | `opencode --session {id}` | id | herdr `plan()` |
| `copilot` | `copilot` | `copilot --resume={id}` | id | herdr `plan()` |
| `kimi` | `kimi` | `kimi --session {id}` | id | herdr `plan()` |
| `droid` | `droid` | `droid --resume {id}` | id | herdr `plan()` |
| `grok` | `grok` | `grok --resume {id}` | id | herdr `plan()` |
| `hermes` | `hermes` | `hermes --resume {id}` | id | herdr `plan()` |
| `kilo` | `kilo` | `kilo --session {id}` | id | herdr `plan()` |
| `qodercli` | `qodercli` | `qodercli --resume {id}` | id | herdr `plan()` |
| `qwen` | `qwen` | `qwen --resume {id}` | id | herdr `plan()` |
| `letta` | `letta` | `letta --conversation {id}` (`--conversation default --agent <a>` for `default:<a>`) | id | herdr `plan()` |
| `muse` | `muse` | `muse resume {id}` | id | fork, verified vs muse CLI |
| `gemini` | `gemini` | `gemini --resume {id}` | id | agent research — VERIFY (U3) |
| `cline` | `cline` | `cline resume {id}` (placeholder guess) | id | EXPERIMENTAL — unverified, U1 still open |
| `kiro` | `kiro-cli` | `kiro-cli chat --resume-id {id}` | id | agent research — VERIFY (U2) |
| `amp` | `amp` | `amp threads continue {id}` | id (`T-<uuid>`) | agent research — VERIFY (U5) |
| `maki` | `maki` | `maki --resume {id}` | id | agent research — VERIFY (U4) |

Notes:

- `strip_subcommand: "resume"` for codex/muse (existing); amp gets
  equivalent handling for its `threads continue` subcommand pair once the
  form is verified; cline gets whatever its CLI uses (U1).
- `valid_session_value` keeps the 512/4096 caps, control-char and leading-`-`
  rules; `_LONG_VALUE_AGENTS = {pi, omp}` unchanged. No new kind takes paths.
- `relaunch_argv` prompt guard and warn-once behavior unchanged.
- Like the Python table header says: re-compare against herdr's current
  `src/agent_resume.rs` before each release (U14); the config `agents`
  overrides can already patch any row without a code change.
- Herdr-side reliability caveats (HERDR §4–5: agy reports only after the
  first prompt, codex needs `transcript_path`, opencode excludes
  attach/shared-server, omp nested-shell suppression, claude subagent
  suppression, letta experimental) are inherited, not fixed: a pane with no
  reported value keeps hitting the `"<pane>: no session id"` decide path in
  auto-sweep and the manual-resolution path in the confirm flow.

## 7. Session-discovery design

### 7.1 Precedence architecture

For any agent pane, session identity is resolved by this precedence chain.
Legs 1–2 are non-interactive and run in both sweep and manual flows; legs
3–5 are interactive and run **only** in the manual confirm flow (auto-sweep
never scans or prompts — parity with the fork's manual-only muse handling,
generalized).

1. **Herdr-reported `agent_session`** (authoritative). If the pane payload
   carries `agent_session.value` (non-empty dict), use it. `resolved_by =
   "herdr"`. This covers the 18 reporting kinds when their integration is
   installed and current.
2. **Agent-reported `resume_argv`** (verbatim replay). If the pane payload
   carries a self-reported resume command (herdr Path B; persists as
   `agent_resume`), validate it with herdr's own rules (bare command name
   resolvable on `PATH`, ≤64 args, ≤8 KiB, no control chars/apostrophes)
   and store it as the pane's `resume_argv`; on restore, replay it verbatim
   (wrapped in `shell_command`) instead of table-built argv. Invalid →
   warning + fall through to leg 3/table/shell. `resolved_by =
   "resume_argv"`. **Gated on U6**: implementation must first verify herdr
   actually exposes the reported argv in `pane.list`/`pane.get`; if it does
   not, this leg is dead code and stays out — `launch_argv` capture remains
   the verbatim-ish mechanism.
3. **Per-kind session-store scanners** (§7.2) + **interactive user confirm**
   (§7.3). Only for panes with no usable value from legs 1–2 whose agent
   kind has a scanner. `resolved_by = "scanner:<kind>"`.
4. **Manual paste** of a session id: the generic `valid_session_value`
   format check, then strict existence in the kind's local store
   (`scan::session_exists`; muse also accepts Session Names, §7.2).
   Rejects say why (bad format vs not found) and re-prompt.
   `resolved_by = "manual-paste"`.
5. **Explicit plain shell** (user-confirmed) or **abort**. Shell panes
   record no agent/session, exactly as today.

Sweep-time rule (unchanged): `decide` refuses panes with missing/invalid/
unknown sessions with the existing reason strings; never-reporting kinds
are therefore manual-only in auto-sweep unless legs 1–2 yield something.
`assess` keeps its blocks/warnings split; scanner-resolved overrides flow
through the existing `session_overrides` path with `source: "manual"`.

### 7.2 Per-kind scanners (`scan/`)

Common `Scanner` trait: `candidates(cwd) -> Vec<Candidate {id, mtime,
matched}>`, newest-first, workspace matches first, then recent fallback —
the musescan shape generalized. Shared guardrails (ported from musescan):
newest-first cap on scan (`_SCAN_LIMIT`-style), max matched / max recent
caps, 60-day age cutoff, 1 MiB read heads, all I/O failures → skip, never
raise. Session-id validation (`_SESSION_ID_RE`-equivalent fullmatch) before
any id touches a glob/path.

| kind | store layout | id | workspace match | status |
|---|---|---|---|---|
| `muse` | `$XDG_DATA_HOME/muse/sessions/YYYY/MM/DD/<id>/session.jsonl` (else `~/.local/share/…`) | dir name | `"workspace_root":"<cwd>"` in first 1 MiB | Port musescan as-is |
| `gemini` | `~/.gemini/tmp/*/chats/session-*.json` | parsed from session JSON (`--resume <uuid>`) | cwd/project field inside JSON — do NOT depend on the `<project_hash>` algorithm | Implement; verify JSON schema (U3) |
| `cline` | `${CLINE_SESSION_DATA_DIR:-~/.cline/data/sessions}/<id>/<id>.messages.json` + `<id>.json` manifest (title/cwd/ts) | dir name | manifest cwd field | Implement; v1 standalone dir only, VS Code `globalStorage/…/tasks/` fallback deferred (U7) |
| `kiro` | `~/.kiro/sessions/cli/<uuid>.json` (metadata: cwd, timestamps) + `<uuid>.jsonl` + `<uuid>.lock` | file stem | metadata cwd; prefer `.lock`-present (active) on ties | Implement; cross-check sqlite store `~/.local/share/kiro-cli/data.sqlite3` only if JSON store proves incomplete |
| `maki` | `${XDG_STATE_HOME:-~/.local/state}/maki/sessions/<base58>.jsonl` | file stem | TBD — verify whether session JSONL records cwd; else newest-first + confirm (U4) | Implement after schema check |
| `amp` | Server-authoritative threads; local `~/.local/share/amp` is a legacy cache — **do not scan** | `T-<uuid>` | CLI-backed: `amp threads list --json` (timeout ~5 s), match cwd/project field | Best-effort; auth/network failure → paste/shell/abort, never fatal (U5) |

Explicitly **no** scanners for the other 18 kinds: herdr reports their
sessions, and on-disk fallback for their edge gaps (agy pre-first-prompt,
codex missing-transcript, opencode attach-mode, omp nesting, claude
subagents) is out of v1 scope — those panes keep the existing skip/manual
paths. If a gap proves painful in practice, the `Scanner` trait makes adding
one mechanical (and several stores are already documented in HERDR §3, e.g.
`~/.claude/projects`, `~/.codex/sessions`).

Scanner results feed two consumers: the manual confirm flow (§7.3) and the
new `history` readers where formats allow (U9). Scanners never run during
auto-sweep.

Each file-backed scanner also exposes `session_exists(id)` for strict
paste validation (leg 4): gemini/cline/kiro/maki check their store for the
id; amp has no local store (server-side threads) so pastes keep the generic
check. Muse accepts a UUID (session dir + `session.jsonl` must exist) or a
Session Name: names live in no metafile — each `session.jsonl` carries
`session.name.changed` events and the latest `new_name` wins (renames
append); matching is case-insensitive like muse's own. The byte-oriented
scan (last `"new_name"` on a `name.changed` line, token JSON-unescaped)
was validated against `session-index.db` on a real 700-session store: full
agreement, plus fresher hits the index had not yet picked up.

### 7.3 Interactive confirm (generalized `manual.rs`)

`resolve_missing_sessions` keeps its contract (`{pane_id: value}` omitting
shells, `None` = abort, EOF/Ctrl-C propagate) and its skip rule (panes with
a reported value stay on the assess/capture path). The muse-only
`_pick_muse_session` becomes `_pick_scanned_session(pane, scanner, …)`:

- No candidates → paste / `s` shell / `q` abort prompt (existing wording,
  kind name parameterized), with the same 3-attempt re-prompt loop as the
  list branch: a rejected paste prints its reason instead of cancelling.
- Candidates → numbered newest-first list, `*` = workspace-matched, `~` =
  recent fallback, with age (`_age` buckets unchanged). Enter = newest,
  digit = pick, paste = strict-validate + re-prompt with reason, `s` =
  shell, `q` = abort, 3 attempts. Digit picks bypass validation (valid by
  construction).
- Kinds without a scanner keep `_shell_or_abort` (existing wording).

The popup-height slack rule (14 lines when any same-tab agent pane lacks a
value) already covers multi-pane scanner picks and is unchanged.

### 7.4 Restore targeting (always-new default)

Restore never targets by workspace label — duplicate labels are ambiguous
(a live failure restored into the wrong same-named workspace), so labels
are display-only. `restore()` takes a [`RestoreTarget`]: `New` (default)
always creates a brand-new workspace; `Existing(id)` reuses one live
workspace id, verified at restore time and falling back to new with a
warning when the id is gone.

The non-interactive `restore <id>` CLI always passes `New` and never
prompts. The picker resolves an undecided Enter via a confirm-style choice
mode (`Mode::Target`, same shape as delete-confirm): it checks whether the
record's `workspace_id` is still live and prompts only then —
`[Enter] new workspace, [o] original, [Esc] cancel` (Enter = new). Records
with no recorded id (old/Python) restore into a new workspace with no
prompt. The `workspace_id` record field is additive; Python ignores it.

## 8. Socket transport (decision) + methods used

**Decision: raw Unix-domain socket via `std::os::unix::net::UnixStream`,
same JSON-line protocol as Python. No `interprocess`, no `HERDR_BIN_PATH`
shell-out.**

Rationale: the plugin is unix-only (§2), and `std` UnixStream needs zero
dependencies. Archive makes few calls per invocation (gather = 2 calls; a
sweep with N targets ≈ a handful more) — latency and streaming do not
matter, which removes the only reason to prefer raw sockets over
`HERDR_BIN_PATH` CLI calls, while shelling out would add a process spawn
per call plus coupling to CLI output-format stability. Keeping the exact
Python wire behavior (one `{id:"herdr-archive:N",method,params}` JSON line, one
`\n`-terminated reply, 10 s timeout, 64 KiB reads) also preserves the
`definite`/`indefinite` error distinction that `archive_tab` and restore
depend on. If Windows support is ever added, switch the transport to
`interprocess::LocalSocketStream` (verified cached, v2.4.2) behind the same
`Client::call` signature — the RUST report's recommended seam.

Socket methods used (identical set to Python, SPEC §3):

| method | params | used by |
|---|---|---|
| `tab.list` | `{}` | sweep gather |
| `pane.list` | `{}` | sweep gather/preview, capture guards, confirm flow, restore guard |
| `pane.get` | `{pane_id}` | track (agent_detected retry, status) |
| `pane.process_info` | `{pane_id}` | `_launch_argv` |
| `workspace.list` | `{}` | labels (failure → warn), original-workspace liveness check |
| `workspace.create` | `{label?,cwd?,focus:true}` | restore recreate |
| `workspace.close` | `{workspace_id}` | restore apply-failure cleanup |
| `layout.export` | `{tab_id}` | capture |
| `layout.apply` | `{root,tab_label,focus:true,workspace_id\|tab_id}` (exactly one id) | restore |
| `tab.close` | `{tab_id}` | archive (`confirmation_required` = worktree group) |
| `plugin.pane.open` | `{plugin_id:"herdr-archive",entrypoint[,height,env]}` | open-picker / open-archive |
| `notification.show` | `{title:"herdr-archive",body}` | all notifications |

Plus a verification item: whether `pane.list`/`pane.get` payloads expose an
agent-reported resume argv (U6) — this decides if discovery leg 2 (§7.1) can
exist.

## 9. Crates, toolchain, picker approach

Dependencies (all versions verified present in the local cargo cache this
session; pin with `=` + `--locked` builds for offline reproducibility):

| crate | version (cached) | use |
|---|---|---|
| `serde` + `derive` | 1.0.229 | JSON-RPC + record/config/activity (de)serialization |
| `serde_json` | 1.0.151 | same |
| `crossterm` | 0.29.0 | picker TUI + `read_key` raw mode, alternate screen, mouse |
| `libc` | 0.2.190 | `flock(2)` for `FileLock` (sweep/activity/migrate/delete locks) |

Everything else is `std` + hand-rolled: CLI args (`std::env::args`,
subcommand match), error types (hand enum, no anyhow/thiserror),
ISO-8601 (hand parser/formatter — no chrono), tempdirs in tests (hand
`std::env::temp_dir` + pid/counter helper — `tempfile` is **not** in the
cache), globbing (hand recursive walk — no glob crate needed).

- Edition 2024, `rust-version = "1.85"` (edition-2024 floor; maximizes the
  chance the source-fallback build works on older user toolchains). Verify
  in CI with a pinned-1.85 check; if crossterm 0.29 or a transitive dep
  needs newer, raise to 1.96 to match herdr-file-viewer (U11).
- `Cargo.lock` committed; `[[build]]` uses `--locked`.
- Picker: crossterm-only, no ratatui — the UI is a filter list (~200 lines
  on top of the pure `picker.rs`), and staying light keeps offline builds
  fast. Alternate screen + raw mode + mouse capture, `ESCDELAY`-equivalent
  25 ms, hidden cursor, same key/mouse map as curses (§5). `ratatui 0.30`
  (cached) is the documented upgrade path if the UI ever outgrows hand
  rendering.
- `confirm.read_key`: crossterm raw-mode single-event read with
  pre-typed-key drain and guaranteed mode restore (same contract as the
  termios version, incl. pty-testable behavior).
- Signal handling (`SIGINT` ignore during restore, `SIGHUP` in confirm):
  via `libc::signal` — no signal-hook crate.

## 10. Manifest sketch (`herdr-plugin.toml`)

```toml
id = "herdr-archive"
name = "Archive"
version = "0.1.0"
min_herdr_version = "0.9.0"
description = "Archive agent tabs that have been inactive for days, and restore them with their conversation resumed."
platforms = ["linux", "macos"]

# v0: direct source build. Graduate to scripts/fetch-or-build.sh
# (prebuilt + SHA-256, cargo fallback) before announcing — see §12.
[[build]]
command = ["cargo", "build", "--release", "--locked"]

[[startup]]
command = ["./target/release/herdr-archive", "sweep", "--if-due"]

[[events]]
on = "pane.agent_status_changed"
command = ["./target/release/herdr-archive", "track"]

[[events]]
on = "pane.agent_detected"
command = ["./target/release/herdr-archive", "track"]

[[events]]
on = "workspace.focused"
command = ["./target/release/herdr-archive", "sweep", "--if-due"]

[[actions]]
id = "restore"
title = "Archive: restore an archived tab"
contexts = ["global"]
command = ["./target/release/herdr-archive", "open-picker"]

[[actions]]
id = "archive-tab"
title = "Archive: archive this tab"
contexts = ["tab"]
command = ["./target/release/herdr-archive", "open-archive"]

[[actions]]
id = "sweep-now"
title = "Archive: sweep now"
contexts = ["global"]
command = ["./target/release/herdr-archive", "sweep"]

[[panes]]
id = "picker"
title = "Archive"
placement = "popup"
width = "80%"
height = 18
command = ["./target/release/herdr-archive", "pick"]

[[panes]]
id = "archive-confirm"
title = "Archive"
placement = "popup"
width = 64
height = 8
command = ["./target/release/herdr-archive", "confirm-archive"]
```

Notes: relative `./target/release/herdr-archive` is the proven unix pattern
(file-viewer, tilt, testrun); `plugin link` never runs `[[build]]`, so dev
flow is `cargo build --release` then `plugin link .`. Version starts at
0.1.0 (new plugin lineage — do NOT continue the Python 0.5.x line, which
would confuse the marketplace + `--ref` pins). Keep `Cargo.toml` version ==
manifest version (CI check, tilt-style).

## 11. Test plan

Parity is the point, so the test suite ports the Python suite's *cases*,
not just its shape. Rust fake-herdr stub (`tests/common/fakeherdr.rs`):
Unix-socket server speaking the JSON-line protocol, scripted
method→response handlers, `close()` failing the test on handler errors
(same contract as `tests/fakeherdr.py`).

Unit tests (in-module `#[cfg(test)]`, ported case-for-case):

- `agents`: table-vs-herdr-`plan()` assertion for all 24 rows (fails the
  build if a row drifts), letta `default:` form, override merge/drop rules,
  strip rules incl. new subcommand pairs, argv0 rewrite, prompt guard +
  warn-once, value validation + caps, shell quoting.
- `activity`: touch/60 s-skip/future, status-change rules, `effective`
  matrix, store locking, `track` event shapes/retries.
- `sweep`: decide matrix, activity lines, assess blocks/warnings/dedup,
  dry-run/live, presence/prune/restart-survival, recheck isolation,
  `archive_now` confirmed/unconfirmed/shifted-id/empty-terminals,
  `preview` race, due/installed/last_sweep semantics.
- `archive`/`restore`: record contents, shell fallbacks, layout 24/16
  limits, close definite/indefinite/worktree/mismatch paths, traversal
  rejection, id validation, overrides, `resume_argv` replay + invalid
  fallback, live-session guard, label fallback, log-before-delete.
- `config`/`session`/`migrate`/`util`: same matrices as
  `test_config/session/migrate/util.py` (incl. never-logs `sessions_for_gate`,
  destination-lock merge, flock contention/killed-holder/wait).
- `picker` (pure): grouping/sort/cursor/moves/filter/delete-confirm/refresh/
  render/markers/too-small/empty/DST — the full `test_picker.py` matrix
  against `State/reduce/render/apply` with zero terminal I/O.
- `confirm`: `lines()` order/wrap/sanitize; `read_key` byte/escape/flush/
  restore (pty test, port of `test_confirm.py`).
- `manual` + each scanner: parameterized over all 6 scanner kinds —
  default-pick/number/paste/shell/quit/reprompt/abort, unknown-kind y/N,
  plus per-scanner fixture trees (muse/gemini/cline/kiro/maki stores built
  in tempdirs; amp via stubbed `amp` executable on PATH) covering
  match-first/newest-first/missing-base/age-cutoff/caps/corrupt-files.
- `history`: claude/codex port + new-reader cases (filters, newest-mtime,
  traversal rejection, exception-swallowing).

Integration tests (`tests/`):

- End-to-end sweep → archive → list → restore → delete against fakeherdr,
  asserting the exact socket-method sequence and record contents.
- **Cross-compat fixtures**: real `record.json` files captured from the
  Python shelf (checked into `tests/fixtures/`) must load and produce
  identical `build_tree` output in Rust; Rust-written records must contain
  no keys outside the Python schema except the three additive ones (§3.1),
  asserted by key-diff.
- `pick` pty end-to-end (down+enter restores 2nd entry — port of
  `test_picker_tty.py`) and one crossterm key/wheel parsing test.
- Manifest test: parse `herdr-plugin.toml`, assert id/name/version parity
  with `Cargo.toml`, command shapes, platforms.

Gates (every PR + release): `cargo fmt --check`, `cargo clippy --locked -D
warnings`, `cargo test --locked`, `cargo build --release --locked`, `cargo
build --locked --offline` (cache-only check), version-parity script, and —
once set up — a pinned-toolchain `cargo +1.85 check` (U11).

## 12. Publish checklist

1. `plugin link` + `plugin list --json` shows zero warnings; both shelf and
   herdr-archive installed side by side with distinct keybindings (coexist note
   in README).
2. Manifest version == `Cargo.toml` version; `Cargo.lock` current.
3. `fmt` / `clippy -D warnings` / `test` / `build --release` all `--locked`
   green; offline build green.
4. Resume table re-compared against herdr's current `src/agent_resume.rs`;
   new-kind rows verified against the real CLIs (closes U1–U5).
5. Tag `vX.Y.Z`; CI (tilt-style `releasing.md`) validates tag==metadata,
   builds linux/macOS × arm64/x86_64 assets each with sibling `.sha256`,
   creates the release only when all present.
6. `[[build]]` graduated to `scripts/fetch-or-build.sh` (version-matched
   prebuilt + SHA-256 verify, `cargo build --release` fallback with a
   clear rustup error when cargo is absent, `~/.cargo/env` sourced
   guarded) before announcing.
7. Repo public, topic `herdr-plugin`, description set (the ad copy),
   LICENSE present, README per the RUST structure (pitch + no-toolchain
   note → quick start + keybinding snippet → requirements → actions/panes
   → config → updating/reinstall + `--ref` pins → dev → troubleshooting).
8. Clean-machine install test **without cargo on PATH** (prebuilt path)
   and one source-build test (fallback path).

## 13. Unresolved decisions

Status key: OPEN (still needs the next step), ANSWERED (resolved during the
v0 build; row kept for the record).

| # | Status | Question | Resolution / next step | Blocks |
|---|---|---|---|---|
| U1 | OPEN (experimental) | `cline` resume argv: what does the `cline` CLI accept? | NOT VERIFIABLE LOCALLY: no `cline` binary on this machine (checked 2026-10-08). Implemented per spec as placeholder `cline resume {id}` + standalone store layout + manifest-cwd match; row + scanner marked EXPERIMENTAL in code (`agents.rs`, `scan/cline.rs`), README, and covered by fixture tests only. Next: run `cline --help` on a machine with the CLI; check `~/.cline` docs | cline table row + strip rules |
| U2 | OPEN (experimental) | `kiro` resume argv: is it `kiro-cli chat --resume-id <uuid>`? | NOT VERIFIABLE LOCALLY: no `kiro`/`kiro-cli` binary (checked 2026-10-08). Implemented as `kiro-cli chat --resume-id {id}` + `~/.kiro/sessions/cli/<uuid>.json` metadata scan; marked EXPERIMENTAL, fixture-tested. Next: `kiro-cli chat --help`; verify id == metadata stem | kiro table row |
| U3 | OPEN (experimental) | `gemini` resume flag (`--resume <uuid>`?) + `session-*.json` schema | NOT VERIFIABLE LOCALLY: no `gemini` binary (checked 2026-10-08). Implemented as `gemini --resume {id}` + chat-file scan with candidate id/cwd fields; marked EXPERIMENTAL, fixture-tested. Next: read a real `session-*.json`; run `gemini --help` | gemini table row + scanner |
| U4 | OPEN (experimental) | `maki` session JSONL schema (is cwd recorded?) + resume flag (`--resume` vs `--session`) | NOT VERIFIABLE LOCALLY: no `maki` binary (checked 2026-10-08). Implemented as `maki --resume {id}` + JSONL cwd-field scan with newest-first fallback; marked EXPERIMENTAL, fixture-tested. Next: read a real session JSONL; run `maki --help` | maki scanner match key + table row |
| U5 | OPEN (experimental) | `amp threads list --json` schema + `threads continue <id>` form + failure UX | NOT VERIFIABLE LOCALLY: no `amp` binary (checked 2026-10-08). Implemented as `amp threads continue {id}` + CLI-backed scan (5 s timeout, all failures → no candidates); marked EXPERIMENTAL, stub-CLI-tested. Next: run against a logged-in `amp`; capture logged-out/offline modes | amp table row + CLI-backed scanner |
| U6 | ANSWERED (negative) | Does herdr expose agent-reported `resume_argv` in `pane.list`/`pane.get`? | NO on herdr 0.9.3 (checked 2026-10-08 via `herdr api schema --json`: `resume_argv` appears only in the `PaneReportAgent*Params` inputs; `PaneInfo`/`AgentInfo` expose `agent_session` only; live `api snapshot` carries no `agent_resume`/`resume_argv`). Discovery leg 2 stays OUT per §7.1. Records still accept + replay a `resume_argv` verbatim on restore (forward-compat, validated per herdr's rules); nothing writes the field today | Discovery leg 2 (§7.1); stays out |
| U7 | OPEN (deferred) | Should the cline scanner fall back to VS Code `globalStorage/…/tasks/`? | Decided for v0: NO — standalone layout only (scanner already experimental; multi-host probing deferred until the CLI form is verified). Revisit with U1 | cline scanner scope |
| U8 | OPEN (v1: claude-only) | Extend `keep_transcripts` copying beyond claude? | v1: claude-only (parity), as specified. Revisit after scanners stabilize | Transcript coverage |
| U9 | ANSWERED (partial) | Which new `history` readers ship in v1? | Shipped cheap readers for gemini / muse / kiro / maki (timestamped JSON/JSONL + mtime fallback), all marked experimental alongside their scanners; claude/codex unchanged (parity floor). No readers for cline (unverified manifest `ts`) or amp (server-side) | Activity accuracy for scanner kinds |
| U10 | OPEN (v1: manual copy) | Python→Rust state import (`import` subcommand or one-shot migration)? | v1: manual archive-dir copy only (schema-identical, §3.1; covered by a cross-compat test). Design `import` later if asked | Cross-plugin migration UX |
| U11 | OPEN | MSRV 1.85 vs 1.96: does crossterm 0.29 + transitive deps build on 1.85? | `Cargo.toml` declares `rust-version = "1.85"`; lockfile pins highest 1.85-compatible versions; built here with rustc 1.89. Still needs the CI job with pinned 1.85; raise to 1.96 (file-viewer parity) on failure | `rust-version` field |
| U12 | OPEN (v0: single override) | Cline data-dir overrides on all platforms (`CLINE_SESSION_DATA_DIR` / `CLINE_DATA_DIR` / `CLINE_DIR` env chain; macOS `~/Library` vs XDG?) | v0 honors `CLINE_SESSION_DATA_DIR` only, else `~/.cline/data/sessions` (marked experimental). Full chain + macOS paths still need verification against cline docs/source | cline scanner base-path logic |
| U13 | ANSWERED (yes) | Amp in auto-sweep: CLI-backed lookup needs auth+network — always manual-only? | YES: scanners never run during auto-sweep — manual confirm flow only (`manual.rs`; `decide` still refuses missing sessions). Network-in-sweep hang risk avoided by construction | amp sweep behavior |
| U14 | OPEN (process) | Re-verify all 24 rows against herdr `src/agent_resume.rs` + real CLIs before each release | Release-checklist step (§12.4) + the table test (`agents.rs`: all 24 plain relaunches) as a tripwire. Baseline set 2026-10-08: rows 1-18 mirror herdr; row 19 (muse) verified against the installed `muse` CLI (`muse resume <session-ref>`); rows 20-24 experimental | Release correctness |

Carried-over context (not blocking): the HERDR report's unreconciled items
(codex lifecycle wiring vs docs, the stale "Resume commands need Herdr
0.10.0+" note, untraced `is_reserved_native_state_source`) affect activity
accuracy at the margins; shelf consumes herdr state as-is either way.



