# Changelog

All notable changes to csx are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Devin CLI sessions are indexed.** `csx sync`, `serve` and `mcp` read
  `~/.local/share/devin/cli/sessions.db` (or `$XDG_DATA_HOME/devin/cli`) with
  tool id `devin`, so `--tool devin` scopes queries to them. csx opens the
  database read-only and never writes to it. Messages that context compaction
  or a retry copies are indexed once, and system prompts are skipped.
  User-role messages that Devin did not mark `is_user_input` are stored with
  role `system`.
- **`csx tabs` recognizes Devin.** A running `devin` process shows its session
  id, title, last prompt, working directory and last activity, and a resume
  command (`cd '<cwd>' && devin -r <id>`).
- `csx-pick.fish` resumes Devin sessions with `devin -r <id>`.
- **Raycast extension.** `contrib/raycast/csx-tabs/` adds Search AI Sessions
  and Search Terminal Tabs to Raycast. Both list rows from `csx tabs --json`,
  focus the tab on Return, and copy the resume command, session id or folder.
- **Resume closed sessions.** `csx tabs --closed [--closed-limit N]` lists
  closed Claude Code, Codex and Devin sessions from the index after the open
  rows, and `--json` adds a `closed` array. csx skips a session that runs, has
  a missing or unsafe folder, or has a folder path that is not absolute. If
  the process list is unreadable, csx hides the list and warns.
- **`csx tabs terminals [--json]`** lists the installed terminals.
  **`csx tabs open --terminal <id> <tool> <session id>`** opens a new tab in
  the session folder and runs the resume command. If the session already runs
  in a tab, csx focuses it. Terminal.app opens a new window.
- **Background index refresh.** With `--closed`, csx starts `csx sync` in the
  background if `~/.csx/last-sync` is older than 60 seconds and no fresh
  `~/.csx/sync.lock` exists. `csx sync` and `csx serve` update the stamp.
- **Alfred and Raycast resume closed sessions.** Closed sessions come last in
  the AI sessions list. Return opens one in the saved terminal, or asks for a
  terminal and saves it. In Alfred, Ctrl+Return picks a terminal for one
  session. In Raycast, use Resume in Another Terminal. The Alfred workflow
  gets a "Terminal" setting.

## [0.2.0] - 2026-10-27

### Added

- **`csx tabs`** lists the tabs of Ghostty, iTerm2, Terminal.app, kitty and
  WezTerm, and the tmux panes inside them. It shows the AI agent that runs in
  each tab (Claude Code, Codex and other tools), with session title, last
  prompt and busy or idle status. Options: `--agents`, `--terminal`, `--json`
  (an object with `rows` and `warnings`), `--alfred`, `--kitty-socket` and
  query words.
- **`csx tabs focus <ref>`** brings a tab to the front.
- **`csx alfred install`** installs the Alfred workflow from the binary, keeps
  the keyword of the older Claude Tabs workflow and removes that workflow.
- **`--tabs-hotkey <SPEC>` and `--agents-hotkey <SPEC>`** on `csx alfred install`
  set the hotkeys of the "Terminal tabs" and "AI sessions" triggers, for
  example `--agents-hotkey alt+a`. A hotkey needs at least one modifier.
  Reinstalling keeps a hotkey you set in Alfred.
- kitty sockets: `--kitty-socket` (also on `focus`), then `KITTY_LISTEN_ON`, then
  `<TMPDIR>/kitty-<pid>` of each running kitty.
- Agents without a tty (such as `codex app-server`) are not listed. A resumed
  Codex session (`codex resume <id>`) gets its title from the rollout with that id.
- macOS only. On other systems `csx tabs` prints an empty list.
- The iTerm2, kitty and WezTerm backends are built from documentation and are
  not tested live.

## [0.1.1] — 2026-07-06

### Changed

- **`serve` and `mcp` are now runnable.** The daemon (file-watching + socket
  query server) and the MCP stdio server were present as tested logic in 0.1.0
  but their CLI commands were placeholders; both are now wired end-to-end.
  `csx serve` runs the daemon; `csx mcp` speaks MCP JSON-RPC over stdio.

## [0.1.0] — 2026-07-06

First release.

### Added

- **Index** — reads AI-coding session transcripts into a local SQLite database
  with two FTS5 indexes (a word tokenizer for prose, a trigram tokenizer for
  substring/code search).
- **Adapters** — `SessionSource` implementations for **Claude Code** and
  **Codex CLI** (both JSONL), sharing one canonical `MessageRecord`.
- **Scoped query** — BM25 + recency ranking with snippets, filterable by
  account, org, profile, tool, repo, cwd, branch, session, role, tool-call, and
  time range; `--code` selects the trigram index.
- **Repo-level grouping** — sessions are keyed by a stable `repo_id`
  (normalized origin remote → root-commit SHA → path), so one repo is one unit
  across worktrees, machines, and accounts.
- **Incremental indexing** — per-file watermarks (byte offset for JSONL) and
  file-identity dedup; re-sync cost scales with new conversation, not history.
- **Daemon** — a resident query + index service with a file watcher and a
  socket query server.
- **Hybrid search + RAG** — vector cosine + reciprocal-rank fusion over the
  keyword results, and `csx ask` for cited answers.
- **MCP server** — exposes `search_sessions`, `get_session`, and `ask_sessions`
  so an MCP client can search your history.
- **CLI** — `sync`, `query`, `list`, `sessions`, `show`, `current`, `ask`,
  `serve`, `mcp`, with human and `--json` output.
- **Docs & tooling** — README, `AGENTS.md`, a `csx-pick` fish integration under
  `contrib/fish/`, a GitHub release workflow, and a Homebrew cask.

### Notes

- Test suite covers logic to ≥98% line and region; OS/network I/O lives in
  `*_shim.rs` files behind traits (see `AGENTS.md`).

[0.1.0]: https://github.com/mysqto/csx/releases/tag/v0.1.0
