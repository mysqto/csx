# Resume Closed Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** List recent Claude Code, Codex and Devin sessions that are not open in any tab at the bottom of the AI sessions list in Alfred and Raycast, and resume one in a new tab of a chosen terminal.

**Architecture:** csx gains `csx tabs --closed` (closed sessions from its index, after the open rows), `csx tabs terminals` (installed target terminals) and `csx tabs open` (new tab, `cd`, resume command), plus a background index refresh. Each plugin keeps its own saved terminal and picker and calls those commands.

**Tech Stack:** Rust (clap, rusqlite, serde), JXA through `osascript`, `kitten @`, `wezterm cli`. Alfred workflow plist and sh. Raycast extension in TypeScript.

**Spec:** `docs/superpowers/specs/2026-10-28-resume-closed-sessions-design.md`

## Global Constraints

- Branch `feat/raycast` in `/Volumes/Library/iWork/csx`. Never push, merge or tag.
- AGENTS.md rules: real side effects only behind traits with `*_shim.rs` adapters. Reading files under a root (temp dirs in tests) needs no shim.
- Coverage ≥98% line AND region: `PATH="$HOME/.cargo/bin:$PATH" cargo llvm-cov --all-features --ignore-filename-regex '(_shim\.rs$|/main\.rs$)' --summary-only`
- `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test` green. No new crates.
- Raycast extension (`contrib/raycast/csx-tabs/`): `npm run build`, `npm test`, `npx tsc -p tsconfig.json`, `npx eslint src test`, `npx prettier --check .` green.
- The four-letter company code name in AGENTS.md never appears anywhere. Fixtures use `/Users/me/work/acme`-style paths and synthetic text.
- Commits authored `mysqto <my@mysq.to>`, message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Never `--no-verify`.
- A plugin names a session only (`<tool> <session id>`). csx builds the folder and command from its index. No path accepts a command string from a plugin.
- Tests never open, focus or close real tabs. Live checks that open a tab need the owner's go-ahead (the controller asks).
- Resumable tools: `claude-code`, `codex`, `devin`. Target terminals in this order: `ghostty`, `iterm2`, `terminal`, `kitty`, `wezterm`.

## Review Focus

1. A closed session whose id has a quote, space or `;` (a corrupt index row): it is not listed and `tabs open` refuses it, because `resume_command` returns None. Test in Task 1 and Task 3.
2. A folder path with a single quote or spaces, such as `/Users/me/it's here`: the typed line quotes it with `shell_quote`. Test in Task 1 (`resume_line`) and Task 3 (JXA string escaping for Ghostty and iTerm2).
3. The target terminal is installed but not running: csx starts it, waits, and opens the tab. If the terminal never answers, csx prints one plain error line. Test in Task 3.
4. The same session open in a tab that `--terminal` filters out: still not listed as closed. Test in Task 1.
5. The Raycast or Alfred saved terminal is no longer installed: the open fails with csx's message, and the picker is offered again. Test in Task 5 (Raycast pure helper) and Task 4 (Alfred script branch).

---

### Task 1: Closed sessions in the tabs report

**Files:**
- Modify: `src/tabs/mod.rs` (types), `src/tabs/app.rs` (`live_sessions`), `src/agent/mod.rs` (resume builders), `src/agent/claude.rs` and `src/agent/devin.rs` (make helpers `pub(crate)`), `src/db/mod.rs` (queries), `src/cli.rs` (flags), `src/cli_shim.rs`, `src/tabs/render.rs` (human and JSON)
- Create: `src/tabs/closed.rs`
- Modify: `contrib/raycast/csx-tabs/test/fixtures/report.json` (add `closed`)

**Interfaces:**
- Produces:
  - `pub struct ClosedSession { pub tool: String, pub session_id: String, pub title: Option<String>, pub cwd: String, pub last_prompt: Option<String>, pub updated_at: Option<i64>, pub resume_command: String }` (Serialize, Default, Clone, PartialEq, Eq, Debug) in `src/tabs/mod.rs`.
  - `TabsReport` gains `#[serde(skip_serializing_if = "Option::is_none")] pub closed: Option<Vec<ClosedSession>>` and `#[serde(skip)] pub live_sessions: Vec<String>` (session ids of every detected agent, before any filter).
  - `pub const RESUMABLE_TOOLS: &[&str] = &["claude-code", "codex", "devin"];` in `src/agent/mod.rs`.
  - `pub fn resume_command(tool: &str, session_id: &str) -> Option<String>`: `claude --resume <id>` (id must pass `is_uuid`), `codex resume <id>` (`is_uuid`), `devin -r <id>` (`is_session_name`), else None.
  - `pub fn resume_line(tool: &str, session_id: &str, cwd: &str, env: &[(&str, String)]) -> Option<String>`: `cd <shell_quote(cwd)> && [NAME=<shell_quote(value)> ...]<resume_command>`.
  - `pub fn session_env(tool: &str, config_dir: Option<&str>, home: &Path) -> Vec<(&'static str, String)>`: empty when `config_dir` is None or equals the default (`<home>/.claude`, `<home>/.codex`, `<home>/.local/share/devin/cli`). Else `CLAUDE_CONFIG_DIR=<dir>`, `CODEX_HOME=<dir>`, or `XDG_DATA_HOME=<dir without the trailing /devin/cli>`. Lives in `src/agent/mod.rs`.
  - `pub struct ResumableSession { pub session_id: String, pub tool: String, pub project_path: Option<String>, pub project_name: Option<String>, pub last_ts: Option<i64>, pub config_dir: Option<String> }` in `src/db/mod.rs`.
  - `Db::resumable_sessions(&self, tools: &[&str]) -> Result<Vec<ResumableSession>>`: sessions joined to `sources.config_dir`, `last_ts` desc (unix seconds).
  - `Db::resumable_session(&self, tool: &str, session_id: &str) -> Result<Option<ResumableSession>>`.
  - `Db::user_prompts(&self, session_id: &str) -> Result<(Option<String>, Option<String>)>`: first and last `role = 'user'` message text by `seq`, from `messages_text`, skipping texts whose trimmed form starts with `<` (injected context such as `<environment_context>`).
  - `pub struct ClosedOptions { pub limit: usize, pub query: Vec<String> }` and `pub fn list_closed(db: &Db, live: &[String], opts: &ClosedOptions, home: &Path) -> Result<Vec<ClosedSession>>` in `src/tabs/closed.rs`.
  - `ListOptions` gains `pub closed: Option<usize>` (the limit. None = no closed list).
  - CLI: `csx tabs --closed [--closed-limit <n>]`, default limit 50. `--closed` with `--alfred` is handled in Task 4. Here `--alfred` ignores `closed`.

Ruling recorded from the spec review: the index `summary` column holds the project name, not a title. Title order is therefore: for `claude-code`, the transcript's custom title, else its AI title (read `<config_dir>/projects/<slug(cwd)>/<id>.jsonl` with the existing `read_tail` and `scan_transcript`). Then the first user prompt (one line, cut to 80 chars). Then `project_name`. `last_prompt`: for `claude-code` the transcript's `last_prompt`, else `Db::user_prompts().1`. `updated_at` = `last_ts * 1000`.

`list_closed` algorithm: walk `resumable_sessions(RESUMABLE_TOOLS)` in order. Skip ids in `live`, rows without a `project_path`, folders that do not exist (`Path::is_dir`), and ids where `resume_command` is None. Build the entry lazily (title and prompts only for rows that pass the cheap checks). Keep it when `matches_closed(&entry, &opts.query)` (case-insensitive substring over title, cwd, last_prompt, tool, session_id, all words must match). Stop at `opts.limit`. `resume_command` field = `resume_line(tool, id, cwd, &session_env(...))`.

- [ ] **Step 1: Write failing tests**
  - `agent::tests::resume_command_per_tool`: `resume_command("claude-code", U)` == `Some("claude --resume U")` for a valid UUID `U`. Codex → `codex resume U`; `("devin","swamp-movie")` → `devin -r swamp-movie`; `("devin","Bad Id")`, `("claude-code","x;rm")`, `("gemini", U)` → None.
  - `agent::tests::resume_line_quotes_folder_and_env`: `resume_line("claude-code", U, "/Users/me/it's here", &[("CLAUDE_CONFIG_DIR", "/Users/me/.claude-b".into())])` == `Some("cd '/Users/me/it'\\''s here' && CLAUDE_CONFIG_DIR='/Users/me/.claude-b' claude --resume U")` (use whatever `shell_quote` produces for the quote. Assert the round trip through `sh -c 'printf %s ...'` is not needed).
  - `agent::tests::session_env_defaults_and_overrides`: default dirs → empty; `("claude-code", Some("/h/.claude-b"), "/h")` → `[("CLAUDE_CONFIG_DIR","/h/.claude-b")]`; `("devin", Some("/x/devin/cli"), "/h")` → `[("XDG_DATA_HOME","/x")]`; `("codex", None, "/h")` → empty.
  - `db::tests::resumable_sessions_order_and_config_dir`, `db::tests::user_prompts_first_last_skips_injected`.
  - `tabs::closed::tests`: skips live ids. Skips missing folder (temp dir removed). Skips invalid id. Limit applies after query match (an old session found by query while the newest 1 is not). Claude title from a transcript fixture in a temp `config_dir`. Falls back to first prompt then project name. Env prefix only for a non-default config dir.
  - `tabs::app::tests::live_sessions_include_filtered_rows`: an agent on a tab that `--terminal kitty` filters out still appears in `report.live_sessions`.
  - `tabs::render::tests::render_json_matches_raycast_fixture` extended: the report gets `closed: Some(vec![...one claude-code entry...])` and the fixture gains the matching `"closed"` array. Plus `render_json` with `closed: None` has no `"closed"` key.
  - `render_human`: closed entries print after the rows under a `closed sessions` line, one per line with tool, title and cwd.
  - CLI dispatch test: `csx tabs --agents --closed --closed-limit 5 --json` passes `ListOptions { closed: Some(5), .. }` to the fake runner.
- [ ] **Step 2: Run** `cargo test` and see the new tests fail to compile or fail.
- [ ] **Step 3: Implement** the interfaces above. `RealTabs::list` (cli_shim) calls `tabs::app::list`, then, when `opts.closed` is Some, `list_closed(db, &report.live_sessions, ...)` with the dispatch query words, and sets `report.closed`.
- [ ] **Step 4: Run** all checks in Global Constraints, including coverage. Measure: `time ./target/release/csx tabs --agents --closed --json >/dev/null` against the owner's index. Record the closed-listing share. If the closed part takes more than 50 ms, drop `last_prompt` for non-Claude tools and note it in the report (spec rule).
- [ ] **Step 5: Commit** `feat(tabs): list closed sessions with --closed`.

### Task 2: Background index refresh

**Files:**
- Create: `src/refresh.rs`, `src/refresh_shim.rs`
- Modify: `src/cli.rs` (`handle_sync` touches the stamp), `src/daemon.rs` (each sync pass touches the stamp), `src/cli_shim.rs` and `src/main.rs` wiring, `src/lib.rs` module list

**Interfaces:**
- Produces:
  - `pub trait Spawner { fn spawn_detached(&self, program: &Path, args: &[&str]) -> Result<()>; }` in `src/refresh.rs`; `RealSpawner` in `src/refresh_shim.rs` (stdin/stdout/stderr null, no wait).
  - `pub const STAMP: &str = "last-sync"; pub const LOCK: &str = "sync.lock";` files in the index's folder (`~/.csx/`).
  - `pub fn touch_stamp(dir: &Path) -> Result<()>` (creates or updates mtime) and `pub fn release_lock(dir: &Path)` (remove, ignore missing).
  - `pub fn maybe_refresh(dir: &Path, now: SystemTime, exe: &Path, spawner: &dyn Spawner) -> Result<bool>`: returns false without spawning when the stamp is younger than 60 s, or the lock exists and is younger than 300 s. Otherwise creates the lock with exclusive create (`create_new`), replacing a stale one, spawns `<exe> sync`, returns true.
- Consumes: Task 1's `RealTabs::list` path. Call `maybe_refresh` there only when `opts.closed` is Some. A refresh error becomes a warning in the report, never a failure.

`handle_sync` and the daemon pass call `touch_stamp` then `release_lock` after a successful sync.

- [ ] **Step 1: Write failing tests** in `src/refresh.rs` with a temp dir and a recording fake `Spawner`: fresh stamp → no spawn. Old stamp, no lock → one spawn with args `["sync"]` and the lock now exists. Old stamp, fresh lock → no spawn. Old stamp, stale lock (mtime 301 s ago, set with `File::set_modified`) → spawn. Missing stamp → spawn. Spawner error → `Err` and the lock is removed. `cli` test: `handle_sync` creates the stamp and removes a lock in the db folder.
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** all Rust checks and coverage.
- [ ] **Step 5: Commit** `feat: refresh the index in the background for closed sessions`.

### Task 3: Installed terminals and `csx tabs open`

**Files:**
- Create: `src/tabs/open.rs`
- Modify: `src/tabs/mod.rs` (`TerminalBackend::open_tab`, `TerminalInfo`), `src/tabs/ghostty.rs`, `iterm2.rs`, `terminal_app.rs`, `kitty.rs`, `wezterm.rs`, `src/cli.rs` (`TabsAction::Open`, `TabsAction::Terminals`, `TabsRunner::open`, `TabsRunner::terminals`), `src/cli_shim.rs`

**Interfaces:**
- Produces:
  - `pub struct TerminalInfo { pub id: &'static str, pub name: &'static str, pub bundle_id: &'static str }` (Serialize skips `bundle_id`) and `pub const TERMINALS: &[TerminalInfo]` in the target order with bundle ids `com.mitchellh.ghostty`, `com.googlecode.iterm2`, `com.apple.Terminal`, `net.kovidgoyal.kitty`, `com.github.wez.wezterm`.
  - `pub fn installed_terminals(cmd: &dyn CommandRunner) -> Vec<&'static TerminalInfo>`: `mdfind kMDItemCFBundleIdentifier == '<id>'` per terminal, non-empty output = installed; `terminal` always counts as installed.
  - `TerminalBackend::open_tab(&self, ports: &Ports, cwd: &str, line: &str) -> Result<()>` with a default that returns `Err("<id> cannot open tabs")`. Implemented by all five backends.
  - `pub enum OpenOutcome { Focused(String), Opened }` and `pub fn open_session(ports: &Ports, backends: &[Box<dyn TerminalBackend>], agents: &[Box<dyn Agent>], ctx: &AgentContext, db: &Db, terminal: &str, tool: &str, session_id: &str) -> Result<OpenOutcome>` in `src/tabs/open.rs`.
  - CLI: `csx tabs open --terminal <id> <tool> <session id> [--kitty-socket <s>]` prints nothing on success and `focused <ref>` when the guard focused a live tab; `csx tabs terminals [--json]` prints `[{"id":"ghostty","name":"Ghostty"},...]` with `--json`, else one `id<TAB>name` per line.

`open_session` order: unknown terminal id → error `unknown terminal: <id>`; `tool` not in `RESUMABLE_TOOLS` → error; `db.resumable_session` None → `no such session: <tool> <id>`. Folder missing → `the folder <path> no longer exists`; `resume_line` None → `invalid session id: <id>`. Then load agents (as `list` does) and, if an agent has this session id and a focusable ref in a fresh `list`, focus it and return `Focused(ref)`. Else `backend.open_tab(ports, cwd, line)` and `Opened`.

Per backend (`line` is the full `cd ... && ...` text. JXA strings built with the existing JSON-escaping helper the backends use):
- Ghostty: if not running, `open -b com.mitchellh.ghostty` through `CommandRunner`, then poll `Application("Ghostty").running()` up to 20 × 250 ms via `Sleeper`. JXA: `const c = g.newSurfaceConfiguration(); c.initialWorkingDirectory = <cwd>; c.initialInput = <line> + "\n";` then `g.newTab({in: g.frontWindow(), withConfiguration: c})`, or `g.newWindow({withConfiguration: c})` when there is no window. Then `g.activate()`. The builder makes sure that the JXA property and parameter names match the dictionary (`sdef /Applications/Ghostty.app`), using a scratch JXA run that only reads (`Application("Ghostty").version()`), never one that creates a tab.
- iTerm2: launch the same way. JXA `currentWindow` → `createTabWithDefaultProfile()` or `createWindowWithDefaultProfile()`; `currentSession.write({text: line})`. Activate.
- Terminal: `Application("Terminal").doScript(line)`. Activate (new window. Spec).
- kitty: `kitten @ --to <socket> launch --type=tab --cwd <cwd>` returns the window id; `kitten @ --to <socket> send-text --match id:<n> <line>\r`. Socket as `list` resolves it. Not running → error `kitty is not running with remote control`.
- WezTerm: `wezterm cli spawn --cwd <cwd>` returns the pane id; `wezterm cli send-text --no-paste --pane-id <n> <line>\n`. Not running → `open -b com.github.wez.wezterm`, poll `wezterm cli list` up to 20 × 250 ms.
- Startup timeout error: `<Name> did not start`.

- [ ] **Step 1: Write failing tests** with the existing fake ports (`src/tabs/test_support.rs`): each backend's `open_tab` sends the expected script or commands for a cwd with a quote (`/Users/me/it's here`) and a line with `"` and `\` (assert the JXA string literal is escaped). Ghostty and WezTerm launch-then-poll paths including the timeout error. Kitty not running error. `open::tests`: each error message above. Guard focuses a live tab and does not call `open_tab`. Success calls `open_tab` on the named backend with `resume_line` output. Env prefix present for a non-default config dir. `installed_terminals` with a fake `CommandRunner`. CLI dispatch and output tests for both subcommands.
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** all Rust checks and coverage. Read-only live check only: `./target/release/csx tabs terminals --json`. Do not run `csx tabs open`.
- [ ] **Step 5: Commit** `feat(tabs): open a closed session in a new terminal tab`.

### Task 4: Alfred: closed items, terminal choice, picker

**Files:**
- Modify: `src/tabs/render.rs` (Alfred items), `contrib/alfred/csx-tabs/info.plist`, `contrib/alfred/csx-tabs/csx.sh` (or a new `contrib/alfred/csx-tabs/open.sh`, embedded like the others), `src/alfred/mod.rs` (embed new files, keep user values), `src/cli.rs` (`--closed` with `--alfred`)

**Interfaces:**
- Consumes: Task 1 `ClosedSession`, Task 3 CLI.
- Produces, in Alfred JSON: every open row gets `"variables": {"csx_kind": "tab"}`. Each closed entry, after the open rows (and after "No matching tabs" is not shown when closed entries exist):
  - `title`: entry title, else `session_id`; `subtitle`: `closed · <tool> · <cwd shortened against HOME>` plus ` · “<last prompt cut to 80>”` when present.
  - `arg`: session id; `variables`: `{"csx_kind": "closed", "csx_tool": "<tool>"}`.
  - `mods.cmd`: copy session id; `mods.alt`: copy `resume_command`; `mods.ctrl`: `{"arg": id, "subtitle": "Choose a terminal for this session", "valid": true, "variables": {"csx_kind": "closed_pick", "csx_tool": "<tool>"}}`.
- The AI sessions script filter runs `tabs --agents --closed --alfred`. The Terminal tabs filter does not change.
- Workflow behavior (object layout is the builder's choice, spec): `csx_kind=tab` → `tabs focus` as today. `csx_kind=closed` and the workflow configuration `terminal` non-empty → `csx tabs open --terminal "$terminal" "$csx_tool" "$1"`. `csx_kind=closed` with empty `terminal`, or `csx_kind=closed_pick` → a picker script filter fed by `csx tabs terminals --json` rendered as Alfred items. Choosing from the picker after `closed` saves the choice (`set configuration "terminal" to value "<id>" in workflow "to.mysq.csx-tabs" with exportable false` via `osascript`, or a Set Configuration utility) and opens. After `closed_pick` it opens without saving. If `tabs open` fails, show the error with a Large Type or notification output, and clear nothing.
- `terminal` is a `userconfigurationconfig` popup (`Ghostty`, `iTerm2`, `Terminal`, `kitty`, `WezTerm`, plus empty "Ask each time") so it is also settable in Alfred's Configure Workflow panel. Its user value lives in `prefs.plist`, which install already keeps (existing test `install_keeps_prefs_plist` style). Add a test that a `terminal` value in `prefs.plist` survives install.

- [ ] **Step 1: Write failing tests**: render tests for the closed items (variables, mods, subtitle, ordering, hostile title text, `No matching tabs` only when both lists are empty). A plist test that the AI sessions filter script contains `--closed` and the new objects and connections exist (uids unique, every connection target exists). Install test for new embedded files and the kept `terminal` value. A shell test of the open script's branches with a stub `csx` on PATH (pattern of the existing alfred script tests, if any. Otherwise a Rust test that runs `sh` on the embedded script with a temp stub) covering: saved terminal → open. Empty → picker output. Open failure → error text on stdout.
- [ ] **Step 2: Run** and see them fail.
- [ ] **Step 3: Implement.** Validate with `plutil -lint contrib/alfred/csx-tabs/info.plist`.
- [ ] **Step 4: Run** all Rust checks and coverage.
- [ ] **Step 5: Commit** `feat(alfred): resume closed sessions in a chosen terminal`.

### Task 5: Raycast: closed section, picker, saved terminal

**Files:**
- Modify: `contrib/raycast/csx-tabs/src/lib/rows.ts` (`ClosedSession`, parse), `src/lib/csx.ts` (`listTabs` closed flag, `openSession`, `listTerminals`), `src/tabs-list.tsx`
- Create: `src/lib/terminals.ts` (pure: parse terminals JSON, choose action titles), `src/terminal-picker.tsx`, `test/terminals.test.ts`
- Modify: `test/rows.test.ts`, `README.md`, `CHANGELOG.md` of the extension

**Interfaces:**
- Consumes: Task 1 JSON (`closed`), Task 3 CLI.
- Produces:
  - `rows.ts`: `interface ClosedSession { tool; session_id; title: string | null; cwd; last_prompt: string | null; updated_at: number | null; resume_command }`; `Report` gains `closed: ClosedSession[]` (empty when absent); `closedTitle(c)`, `closedKeywords(c)`.
  - `terminals.ts`: `interface TerminalChoice { id: string; name: string }`, `parseTerminals(text: string): TerminalChoice[]`, `resumeTitle(saved: TerminalChoice | undefined): string` → `Resume in <Name>` or `Resume Session…`, `savedChoice(savedId: string | undefined, installed: TerminalChoice[]): TerminalChoice | undefined` (undefined when the saved id is no longer installed).
  - `csx.ts`: `listTabs(agentsOnly)` adds `--closed --closed-limit 200` when `agentsOnly`; `openSession(terminal, tool, id): Promise<string>` runs `csx tabs open --terminal <t> -- <tool> <id>` (returns stdout); `listTerminals(): Promise<TerminalChoice[]>`.
  - LocalStorage key `"terminal"` holds the saved id.
  - UI: Search AI Sessions shows section "Closed Sessions" after the open tabs. Primary action `resumeTitle(saved)`: with a saved choice, open directly (close the window first, HUD on error as for focus). Without, push `TerminalPicker` with `save: true`. Actions "Resume in Another Terminal…" (`TerminalPicker`, `save: false`) and "Change Default Terminal…" (picker that only saves). Copy actions: resume command, session id, folder. Search Terminal Tabs does not change.

- [ ] **Step 1: Write failing tests**: `parseReport` with and without `closed` (fixture from Task 1 now has one). Hostile closed titles; `closedTitle` fallback to session id; `parseTerminals` valid and invalid JSON; `resumeTitle`; `savedChoice` with an uninstalled saved id → undefined.
- [ ] **Step 2: Run** `npm test` and see them fail.
- [ ] **Step 3: Implement.** Update the extension README (Commands and a short "Resume closed sessions" part) and CHANGELOG initial-version list.
- [ ] **Step 4: Run** all extension checks in Global Constraints.
- [ ] **Step 5: Commit** `feat(raycast): resume closed sessions`.

### Task 6: Docs

**Files:** `README.md`, `AGENTS.md`, `CHANGELOG.md` (repo root)

- [ ] **Step 1:** README: in "Terminal tabs, Alfred and Raycast", add `csx tabs --closed`, `csx tabs terminals`, `csx tabs open`, the background refresh (`~/.csx/last-sync`), Alfred Return / Ctrl+Return on closed items and the `terminal` workflow setting, Raycast actions, and the Terminal.app new-window note. AGENTS.md module map rows for `tabs/closed.rs`, `tabs/open.rs`, `refresh.rs`, `refresh_shim.rs`, and the fixture note now covers `closed`. CHANGELOG Unreleased entry.
- [ ] **Step 2:** grep the repo for the company code name: zero hits.
- [ ] **Step 3: Commit** `docs: resume closed sessions`.
