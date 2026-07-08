# Changelog

All notable changes to csx are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

_Nothing yet._

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
