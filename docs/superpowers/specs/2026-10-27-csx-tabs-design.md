# `csx tabs`: find and focus terminal tabs, and the AI sessions in them

Date: 2026-10-27. Status: draft, waiting for approval. Calls marked "(My call.)" were settled after a builder review.

## Intent

The user works in many terminal tabs at once. Several of them run AI coding
sessions (mostly Claude Code). Finding the tab for a given session is slow:
`csx sessions` finds the session, but not the tab it runs in.

`csx tabs` lists the tabs of every supported terminal app and focuses one. It
labels each tab that runs an AI coding tool with what that session is about.
An Alfred workflow, shipped and installed by csx, puts both on a hotkey.

A first version exists outside csx as an Alfred workflow in the user's config
repository (`app/alfred/claude-tabs/cctab.js`, JXA, Ghostty and Claude Code
only). It works and proves the Ghostty mechanism below. This design moves that
into csx, in Rust, and widens it.

## Goals

1. `csx tabs` lists the tabs of all running supported terminal apps.
2. `csx tabs --agents` lists only the tabs that run an AI coding tool.
3. `csx tabs focus <ref>` brings one tab to the front, in any supported app.
4. Each AI tab shows the tool, and for tools with local session data, the
   session title, the last prompt and the session id.
5. An Alfred workflow with two entries: "all terminal tabs" and "AI sessions".
   Each has a keyword and a hotkey slot. Enter focuses the tab.
6. `csx alfred install` installs or updates that workflow in the user's Alfred
   preferences, whatever way csx itself was installed.

## Non-goals

- Indexing transcripts of new AI tools (`SessionSource` adapters for Gemini
  CLI, OpenCode, and others). That is the next, separate project. This
  project only detects those tools in a tab.
- Terminals that cannot be scripted from outside: Warp (no scripting
  interface), Alacritty (no tabs), the VS Code integrated terminal.
- Linux and Windows. `csx tabs` is macOS only. On other platforms it prints
  that it is not supported and exits 0 with an empty list.
- A custom picker UI. Alfred is the UI (the user's choice). The CLI output is
  plain text and JSON, so fzf or other launchers can use it too.
- Sessions on remote hosts (inside ssh). Only local processes count.

## Anti-goals

- Changing anything the user can see in a tab beyond a title flash that lasts
  well under a second (Ghostty only, see below). The original title must come
  back.
- Typing into, or sending keys to, any terminal.
- Slow listing. With about 20 tabs, `csx tabs --alfred` must finish in well
  under one second, or Alfred feels broken.

## Constraints

- Follow `AGENTS.md`: every real side effect (running `osascript`, `ps`,
  `kitten`, `wezterm`, `tmux`, `defaults`, writing to a tty) sits behind a
  trait whose only real implementation is in a `*_shim.rs` file. All parsing
  and decisions are outside shims and unit-tested. Coverage stays at 98% line
  and region or more. `cargo fmt` and `clippy -D warnings` stay clean.
- No new crates unless clearly needed. The sanctioned set covers this work.
- The four-letter company code name rule in `AGENTS.md` applies, including in
  fixtures and the Alfred workflow.
- Commits are authored `mysqto <my@mysq.to>`.

## Design decisions (the user's)

- Build it in csx as `csx tabs`, not as a script in the config repository.
- Alfred is the only UI that matters. A single hotkey opens the list.
- Move the Alfred workflow into csx. The config repository stops shipping its
  own copy.
- Support popular terminal apps beyond Ghostty.
- Support popular AI tools beyond Claude Code, in a plugin-like way.

## How it works

### Data model

A `Tab` (one per terminal surface, so a Ghostty split or an iTerm2 pane is
its own row):

- `terminal`: app id (`ghostty`, `iterm2`, `terminal`, `kitty`, `wezterm`),
  plus `tmux` as an inner layer (below)
- `ref`: an opaque string that `csx tabs focus` accepts. Examples:
  `ghostty:<terminal id>`, `iterm2:<session id>`, `kitty:<window id>`. The
  form `tty:<ttysNNN>` names a tab known only by its tty (see below).
- `title`, `cwd` (when the app reports it), `tty` (when the app reports it)
- `window` and `index` for display ordering
- `agent`: optional `AgentInfo`

`AgentInfo`: `tool` (stable id, below), `pid`, `tty`, `cwd`, `session_id`,
`title`, `last_prompt`, `status` (busy, idle, or unknown), `updated_at`.

"AI sessions" (`--agents`) is a list of live agent processes, not a filter
over tabs. Each agent gets a row. Sometimes csx cannot link the agent to a
tab. Examples are the VS Code terminal, Warp, and a tmux pane with no
client. If such an agent has a tty, its row has the `ref` `tty:<ttysNNN>`.
Otherwise the row has no `ref`.
`focus` on a `tty:` ref asks each running backend to find the tab with that
tty (Ghostty through the title marker). A row without a `ref` is not
focusable in Alfred, but its Cmd and Option copy actions still work. This
keeps every session that the JXA workflow lists today. (My call.)

### Terminal backends

A `TerminalBackend` trait: `id()`, `is_running()`, `list() -> Vec<Tab>`,
`focus(&Tab)`. The backends only build commands and parse output. They talk
to the OS through two ports: an `OsaRunner` (runs a JXA or AppleScript
snippet, returns stdout) and a `CommandRunner` (runs a program with
arguments, returns stdout). Each port has one real implementation in a shim.

| App | List | Focus | tty |
| --- | --- | --- | --- |
| Ghostty 1.3+ | AppleScript: windows, tabs, terminals (`id`, `name`, `working directory`) | `focus` the terminal, then `activate` | not reported, see the title marker below |
| iTerm2 | AppleScript: windows, tabs, sessions (`id`, `name`, `tty`, `path` variable) | `select` the session, its tab and window, then `activate` | yes |
| Terminal.app | AppleScript: windows, tabs (`tty`, `custom title`, processes) | set the tab `selected`, the window `index` to 1, then `activate` | yes |
| kitty | `kitten @ ls` JSON (needs `allow_remote_control` and `listen_on`. csx finds the socket from `KITTY_LISTEN_ON` or a configured path.) | `kitten @ focus-window --match id:<n>`, then activate the app | from the foreground process pids |
| WezTerm | `wezterm cli list --format json` | `wezterm cli activate-pane --pane-id <n>`, then activate the app | yes (`tty_name`) |

If an app is not running, its backend returns nothing after one cheap
check. Sometimes an app runs but cannot be queried, for example kitty
without remote control, or a refused AppleScript permission. Then the
backend reports a one-line warning and does not exit with an error. In
Alfred, the warning is a row that says what to turn on.

tmux is an inner layer, not a terminal app. If a tmux server runs, csx
lists its panes with `tmux list-panes -a` (tty, path, title). Each pane goes
under the outer tab whose tty is the tty of a tmux client
(`tmux list-clients`). Focusing a tmux pane switches that client to the pane
(`switch-client`, `select-window`, `select-pane`) and then focuses the outer
tab. A pane with no attached client is listed but not focusable.

### Ghostty: the title marker

Ghostty's AppleScript does not report a tty, so the link between a process
and its terminal comes from the terminal's title. csx finds the tab that
owns tty `T` in these steps:

1. Read the titles of all terminals.
2. Write a unique marker title to `/dev/T` (`OSC 2`).
3. Read the terminal list again until one terminal shows the marker.
4. Focus that terminal.
5. If the marker still shows, write the saved title back.

A busy agent repaints its title several times a second. So csx writes the
marker again before each read, with up to 6 attempts and a growing delay
(50 ms, 100 ms, and so on). The JXA version proves this method.

csx writes markers only in `csx tabs focus`, never while it lists. Alfred
runs the list again on each keystroke. Markers there flash titles and cost
up to about 1 s each time. (My call.)

While listing, csx links an agent to a Ghostty tab only by title and cwd.
For Claude Code, the session title appears in the tab title, so most Claude
rows get a `ghostty:` ref. If the match is missing or ambiguous, the agent
row gets a `tty:` ref, and `focus` uses the marker. In the all-tabs list, a
Ghostty tab that matches no agent shows no agent label.

### Agents (the plugin point)

An `Agent` trait, one implementation per AI tool:

- `tool()`: stable id
- `matches(process) -> bool`: recognizes the tool's process from its command
  name and arguments
- `enrich(pid, cwd, ...) -> AgentInfo`: fills in the session id, title and
  last prompt from the tool's local session data. Some tools have none.

Process facts come from one `ps` call per `csx tabs` run (pid, ppid, tty,
process group, command, arguments) through the `CommandRunner` port. The
agent for a tab is the matching process on that tab's tty, preferring the
foreground process group.

Initial agents:

| Tool id | Detect | Enrich |
| --- | --- | --- |
| `claude-code` | `claude` | `<config dir>/sessions/<pid>.json` (sessionId, cwd, status, name, updatedAt), then the transcript tail: last `custom-title`, else last `ai-title`, and last `last-prompt`. Config dirs: `~/.claude`, every Claude `config_dir` recorded in the `sources` table, and `CLAUDE_CONFIG_DIR` (when it has a value). Alfred does not pass the shell environment, so the `sources` rows are how extra account dirs work from Alfred. (My call.) |
| `codex` | `codex` | newest rollout under `~/.codex/sessions` whose `session_meta` cwd equals the process cwd and whose start time is after the process start. The title comes from the first user message. |
| `gemini`, `opencode`, `copilot`, `cursor-agent`, `aider`, `qwen`, `goose`, `amp`, `crush`, `droid` | process name (node-based tools: the script name in the arguments) | none for now: tool name, cwd and tab title only |

Adding a tool is one new file in `src/agent/` plus one line in the registry.
When the transcript-adapter project adds a `SessionSource` for a tool, its
agent can gain an `enrich` that reuses that parser.

`Tool` in `model.rs` stays the indexed-transcript enum. Agents use their own
string ids, so detecting a tool does not require indexing it.

### CLI

```
csx tabs [--agents] [--terminal <id>]... [--json | --alfred] [<query words>...]
csx tabs focus <ref>
csx alfred install [--prefs <Alfred.alfredpreferences path>]
```

- Human output: one line per tab, `terminal  title  cwd`. If an agent runs
  in the tab, the line also shows its tool and status.
- `--json`: an array of `Tab` objects.
- `--alfred`: Alfred script filter JSON. Every query word must appear in the
  title, cwd, terminal, tool, session title, last prompt or session id. Rows
  sort with agent rows by most recent activity, then the other tabs by
  window and index. In each row, `arg` is the `ref`. Cmd copies the session
  id (agent rows) or the cwd. For agent rows whose tool supports it, Option
  copies a resume command (`claude --resume <id>`, `codex resume <id>`).
- If the tab is gone, `focus` exits non-zero with a clear message.

### Alfred workflow

Source in `contrib/alfred/csx-tabs/` (`info.plist`, `icon.png`). Two script
filters:

- "Terminal tabs": keyword default `tt`, runs `csx tabs --alfred "$1"`.
- "AI sessions": keyword default `ct`, runs `csx tabs --agents --alfred "$1"`.

Each has a hotkey trigger with no key assigned, so the user picks one key in
Alfred. One run-script action runs `csx tabs focus "$1"` (Cmd and Option rows
go to a copy-to-clipboard output). Alfred does not load the user's shell PATH, and the
workflow syncs to every Mac through the Alfred sync folder. So the scripts
find `csx` in this order: the optional workflow variable `csx_path` (empty
by default, for the user to set), then `/opt/homebrew/bin`,
`/usr/local/bin`, `/opt/local/bin`, `~/.cargo/bin` and `~/.local/bin`.
If none has `csx`, the row says so. `csx alfred install` does not write a
machine path into the workflow. (My call.)

`csx alfred install` embeds the workflow files in the binary
(`include_bytes!`). It installs them in these steps:

1. Find the Alfred preferences folder. That is the `syncfolder` key of
   `com.runningwithcrayons.Alfred-Preferences`, read through a shim. If the
   key is not set, it is
   `~/Library/Application Support/Alfred/Alfred.alfredpreferences`.
2. Write the files to `workflows/user.workflow.csx-tabs/` in that folder.
   Keep an existing `prefs.plist`, because Alfred stores the user's keyword
   and hotkey choices in it.
3. If `workflows/user.workflow.claude-tabs/` exists and its bundle id is
   `to.mysq.claude-tabs` (the JXA workflow), remove it. If the new workflow
   has no `prefs.plist` yet, first copy the old keyword choice into it. Then
   the user's `ct` keyword and hotkey are kept and do not collide.
   (My call.)
4. Print where it installed. Also print that macOS asks once to allow
   Alfred to control each terminal app.

### Config repository follow-up

These changes are made in the config repository, not in csx. They land
together with the csx release, so the old workflow is not installed again
by setup:

- Remove `app/alfred/claude-tabs/` and `install_alfred_workflows`.
- If csx is installed, make `configure_alfred` run `csx alfred install`.

## Left to the builder

- Module layout under `src/tabs/` and `src/agent/`, and shim names.
- Exact JXA snippets per app. One `osascript -l JavaScript` call per app per
  run is the target, to stay fast.
- How the kitty socket is configured (a flag, an env var, or both).
- Human output formatting and column widths.
- The workflow icon: generate a simple neutral one. Do not use any
  terminal's or AI tool's logo.
- How to test backends without the apps: fixture outputs fed through fake
  ports. Live checks on this Mac are possible for Ghostty and Terminal.app
  only. The others are built from their documentation and must say so in
  the README.
- Whether the Ghostty title-and-cwd match needs path normalization (the
  user's home has a symlinked work folder, so `/Users/x/iWork` and
  `/Volumes/Library/iWork` name the same place).
