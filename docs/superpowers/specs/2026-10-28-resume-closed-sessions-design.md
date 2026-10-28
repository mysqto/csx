# Resume closed sessions from Alfred and Raycast

Date: 2026-10-28. Status: draft for owner review.

## Intent

`csx tabs` finds the tab of a running AI session. The owner also wants to
reach sessions that are no longer open in any tab, and resume them in one
step from the same place. They pick a closed session in Alfred or Raycast.
A new tab opens in their terminal, in the session's folder, and the resume
command runs there.

## Decisions the owner made

- Closed sessions appear in the existing AI sessions list: Alfred keyword
  `ct` and its hotkey, and the Raycast command Search AI Sessions. Open tabs
  come first. Closed sessions come at the bottom.
- A closed session opens in a new tab of a terminal that the user chose once.
  If the user has not chosen one, a picker asks, saves the choice, and opens
  the session.
- A second action lets the user pick a terminal for one session without
  changing the saved choice.
- The saved terminal and the picker belong to each plugin. csx stores no
  terminal preference. Alfred and Raycast each do it in their own way.
- csx does the mechanical part: it lists closed sessions, lists installed
  terminals, and opens the new tab.

## Goals

- From the AI sessions list, resume any recent Claude Code, Codex or Devin
  session that is not open, in Ghostty, iTerm2, Terminal, kitty or WezTerm.
- The resumed session starts in its own folder. This is required: Claude
  Code finds a session by the folder it starts in.
- A session that is open in a tab appears only once, as that tab. Return
  focuses it and does not start a second copy.

## Non-goals

- Resuming tools that have no resume command (the generic agents such as
  Gemini CLI or aider). Their closed sessions are not listed.
- tmux as a target. tmux stays an inner layer for listing and focusing.
- Splits, new windows on purpose, or terminal profiles.
- Linux and other platforms. `csx tabs` is macOS only today.
- A terminal preference stored by csx.

## Anti-goals

- A plugin must never be able to make csx run an arbitrary command. The
  plugin names a session, and csx builds the folder and the command from its
  own index.
- The AI sessions list must not get noticeably slower to open.

## csx changes

### Listing closed sessions

`csx tabs` gains a flag that adds closed sessions to the report. Proposed:
`--closed`, with `--closed-limit <n>` (default 50). The JSON report gains a
top-level `closed` array next to `rows` and `warnings`. Each entry has:

- `tool`: `claude-code`, `codex` or `devin`
- `session_id`
- `title`: the session summary from the index, else the first user prompt
- `cwd`: the session's project path
- `last_prompt`: the last user message, or null if the index has none. The
  `messages` table holds no text, so this joins `messages_text` for the last
  `user` row of each listed session. My call: keep it only if the whole
  closed listing stays under 50 ms on the owner's index (290 sessions).
  Otherwise drop the field and its part of the subtitle.
- `updated_at`: last activity, unix milliseconds
- `resume_command`: always `cd '<cwd>' && <command>`, for copying. The open
  rows of Claude Code and Devin already use this form. Codex open rows use a
  bare `codex resume <id>` today. Closed entries add the `cd` for all three
  tools, because a copied command must work from any folder.

Without `--closed`, the report has no `closed` key, so v0.2.0 consumers are
unaffected.

Rules:

- Source: the csx index (`sessions` and `messages` tables), newest
  `last_ts` first, only the three tools above.
- A session is open, and is left out of `closed`, if any agent process that
  csx detects has that session id. This check uses every detected agent, not
  only the rows shown. Rows hidden by `--terminal` or a detached tmux pane
  still count as open. My call: an agent whose session id csx cannot detect
  does not hide anything. `csx tabs open` has a second guard for that case
  (see below).
- My call: a session whose folder no longer exists is left out, because the
  resume fails.
- Query words (`csx tabs --agents --closed fix login`) match closed sessions
  too, over the title, folder, last prompt, tool and id. Matching runs over
  all indexed sessions, then the limit applies, so a search can find an old
  session.
- `--alfred` output puts closed items after the open ones (see Alfred).

### Keeping the index fresh

An incremental `csx sync` takes about 2 s on the owner's Mac, too slow to run
on every list. My call: `csx tabs --closed` lists from the index as it is.
If the last sync is older than 60 s, csx also starts `csx sync` in the
background. It does not wait for that sync. The next list is then fresh.

The index has no record of the last sync today. My call: every successful
`csx sync`, and every `csx serve` sync pass, updates the modification time
of a stamp file next to the index (`~/.csx/last-sync`). No schema change.
A background sync takes a lock file (`~/.csx/sync.lock`, created with
exclusive create). If that file exists and is younger than 5 minutes, csx
starts no second sync. SQLite WAL and the busy timeout already handle a
sync that overlaps `csx serve`.
Users who run `csx serve` always have a fresh index. Starting the detached
process is a side effect and goes behind a port with a `*_shim.rs` adapter.

### Opening a session

New subcommand: `csx tabs open --terminal <id> <tool> <session id>`.

- csx looks the session up in its index and takes the folder from there.
  csx builds the resume command itself. It uses the id checks and quoting
  that the agents use today. For example, Devin ids must match
  `^[a-z0-9]+(-[a-z0-9]+)*$`, and Codex ids must be UUIDs.
- Unknown session, invalid id, missing folder or unknown terminal: exit
  non-zero with one plain line on stderr. The plugins show that line.
- Second guard against a duplicate: before it opens a tab, csx checks the
  detected agents again. If one runs this session, csx focuses that tab and
  opens nothing.
- Account folder: the index records a config folder per source. If the
  session's folder is not the tool's default, csx sets the tool's variable
  for the new shell: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or `XDG_DATA_HOME`
  for Devin (the parent of `devin/cli`). On the owner's Mac every source uses
  the default folder, so this changes nothing there today.
- The text typed into the new tab is `cd '<folder>' && <command>`, from the
  same builder as `resume_command`. The terminals that take a starting
  folder get it too.
- It opens a new tab and brings the terminal to the front. If the terminal
  is not running, csx starts it first.
- The command runs in the user's login shell. When the agent exits, the
  shell stays open.

How each terminal does it. Facts marked "checked" were read on this Mac. The
builder must make sure that the others are correct.

- Ghostty 1.3.1 (checked in its scripting dictionary): make a `new surface
  configuration`. Set `initial working directory` to the folder. Set
  `initial input` to the command and a newline. Then run `new tab` in the
  front window with that configuration. If there is no window, run `new
  window` instead.
- iTerm2: create a tab in the current window (or a window), then `write
  text` with `cd '<folder>' && <command>`.
- Terminal: AppleScript has no "new tab". My call: `do script` opens the
  command in a new window, and the docs say so. Do not use System Events
  keystrokes, because they need the Accessibility permission.
- kitty: `kitten @ launch --type=tab --cwd <folder>`, then send the command
  as text to the new window. Socket discovery as `csx tabs` does today.
- WezTerm: `wezterm cli spawn --cwd <folder>` returns a pane id, then
  `wezterm cli send-text --no-paste --pane-id <id>` with the command and a
  newline.

### Installed terminals

New subcommand: `csx tabs terminals --json` prints the terminals that can be
targets and are installed, in a fixed order: Ghostty, iTerm2, Terminal,
kitty, WezTerm. Each entry has `id` (the same ids `--terminal` takes) and
`name`. The plugins build their pickers from it. How "installed" is decided
is left to the builder (bundle id lookup, or the binary on PATH for kitty
and WezTerm).

## Alfred

- Closed items come after the open ones. Subtitle: `closed · <tool> ·
  <folder> · “<last prompt>”`, with the folder shortened against `HOME` as
  today.
- Return on a closed item: if the workflow variable `terminal` is set, run
  `csx tabs open --terminal $terminal <tool> <id>`. If it is not set, show
  a picker built from `csx tabs terminals`. Selecting a terminal saves it to
  the `terminal` variable and opens the session.
- Ctrl+Return on a closed item: the same picker, for this session only. It
  does not save.
- Cmd+Return copies the session id and Option+Return copies the resume
  command, as for open rows.
- Saving the variable: the builder picks the mechanism (Alfred's "set
  configuration" AppleScript, or a workflow "Set Configuration" utility).
  The workflow is the one `csx alfred install` writes, so the change lives
  in `contrib/alfred/csx-tabs/info.plist` and its scripts.
- `csx alfred install` must keep a user's saved `terminal` variable, as it
  keeps hotkeys today.
- An open-tab row still focuses on Return. Ctrl+Return on an open row does
  nothing new.

## Raycast

- Search AI Sessions shows a section "Closed Sessions" after the open tabs.
  Search Terminal Tabs does not change.
- Primary action on a closed session: "Resume in <Terminal>" with the saved
  terminal. With no saved terminal: "Resume Session…", which pushes a picker
  list. Choosing saves and opens.
- Raycast extensions cannot write their own preferences, so the saved
  terminal lives in the extension's LocalStorage. An action "Change Default
  Terminal…" re-opens the picker.
- Second action: "Resume in Another Terminal…" opens the picker for this
  session only.
- Copy actions as for open rows (resume command, session id, folder).
- The shared JSON fixture (`test/fixtures/report.json`) gains a `closed`
  entry, and the Rust contract test covers it.

## Constraints

- csx rules in AGENTS.md apply. Side effects go only behind ports with
  `*_shim.rs` adapters. Line and region coverage stay at 98% or more. fmt
  and clippy stay clean. Commits use the repo's author and trailer. The
  company code name never appears anywhere.
- Opening a tab is user-visible and moves windows. Tests use fake ports. Any
  live check that opens a real tab needs the owner's go-ahead first.
- The Raycast extension keeps passing build, tests, type check, eslint and
  prettier, and stays ready for the Store.
- Build on branch `feat/raycast` (it sits on `feat/devin`).

## Left to the builder

- Exact flag and subcommand spelling. A better fit for the CLI is allowed.
- Make sure that the Codex id in the index is the id that `codex resume`
  takes. The index keeps the id from the rollout's `session_meta` payload.
  The owner's index has one Codex session to check against.
- How "installed" is detected, how a not-running terminal is started and
  how long to wait for it.
- Alfred object layout for the picker and saving the variable.
- Raycast picker layout and icons.
- The title to show if both the summary and the first prompt are empty.
