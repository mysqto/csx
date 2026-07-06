# AGENTS.md — working in the `csx` repo

A guide for coding agents. Read it before touching this tree. `csx` is a local
full-text + hybrid search engine over AI-coding session transcripts (Claude
Code, Codex CLI). The whole design exists to keep one invariant true:

> **Every real OS/network side effect sits behind a trait whose only real
> implementation lives in a `*_shim.rs` file. All decision logic lives outside
> shims and is unit-tested. Coverage target: ≥98% line AND region.**

If you internalize one thing, make it that.

---

## Build / test / lint

```sh
cargo build
cargo test
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
```

All four must be green before you finish. `cargo fmt` and a clippy run with
`-D warnings` are non-negotiable.

Dependencies are lean and pinned in `Cargo.toml`; do not add crates casually.
The sanctioned set: `rusqlite` (feature `bundled` — ships SQLite incl. FTS5 +
trigram), `ureq` (blocking HTTP), `notify` (FS watch), std `UnixListener`
(sockets), `serde`/`serde_json`, `clap` (derive), `sha2`, `walkdir`,
`thiserror`/`anyhow`. Do **not** use `sqlite-vec`; embeddings are `f32` BLOBs
compared with cosine in Rust.

There is a hard project rule: one specific company code-name (the four-letter
string used internally at the author's employer) must never appear anywhere in
this repo — code, comments, tests, fixtures, docs, or sample data.

---

## Coverage discipline (the important part)

Run coverage with Homebrew LLVM so `llvm-cov`/`llvm-profdata` match the
toolchain:

```sh
LLVM_COV="$(brew --prefix llvm)/bin/llvm-cov" \
LLVM_PROFDATA="$(brew --prefix llvm)/bin/llvm-profdata" \
cargo llvm-cov \
  --all-features \
  --ignore-filename-regex '(_shim\.rs$|/main\.rs$)' \
  --summary-only
```

**Ignore regex:** `(_shim\.rs$|/main\.rs$)`. That is the coverage boundary — the
`*_shim.rs` adapters plus the trivial `main.rs` entry point (which only
delegates to `csx::run`). Everything else must be covered.

Rules that keep coverage reachable:

- A `*_shim.rs` file holds a trait's **real** adapter and *nothing a test needs
  to reach*: no branching, no parsing, no decisions. If you find yourself
  writing an `if`/`match` that matters inside a shim, it belongs in a non-shim
  module behind the trait.
- Reading files/dirs under a **temp root** is testable — do it directly, no
  shim. Only the live FS *watcher* and the network/sockets/stdio/`git`-binary
  need shims.
- Every non-shim module carries its own `#[cfg(test)] mod tests` exercising all
  branches with fakes, `tempfile`-style temp dirs, and `:memory:` SQLite.

---

## Architecture / module map

Ports (traits) are consumed by pure logic; the matching `*_shim.rs` supplies the
one real adapter.

| Module              | Role                                                                 | Port(s) → shim                          |
| ------------------- | -------------------------------------------------------------------- | --------------------------------------- |
| `model.rs`          | Canonical domain types: `Tool`, `Role`, `Account`, `MessageRecord`, `SessionMeta`. | —                          |
| `error.rs`          | `Error` / `Result`.                                                  | —                                       |
| `db/mod.rs`         | `Db` over `rusqlite`; owns schema (FTS5 + trigram), upserts, scoped `search`. Tested on `:memory:`. | — (no shim needed)   |
| `source/mod.rs`     | `SessionSource` trait + `SessionFile` / `ParsedDelta`.               | `SessionSource` (adapters, no shim — read temp roots) |
| `source/claude.rs`  | `ClaudeSource` adapter (`~/.claude`).                                | —                                       |
| `source/codex.rs`   | `CodexSource` adapter (`~/.codex`).                                  | —                                       |
| `source/jsonl.rs`   | JSONL read helpers (whole-file + offset).                            | —                                       |
| `source/fileid.rs`  | Stable file keys, size/mtime.                                        | —                                       |
| `index.rs`          | `sync` — the incremental indexer (watermarks, repo resolution).      | —                                       |
| `repo.rs`           | Stable repo-id resolution from git output.                           | consumes `GitRunner`                    |
| `git_shim.rs`       | Real `git` subprocess adapter.                                       | `GitRunner` → **shim**                  |
| `output.rs`         | Pure `Format` (Human/JSON) rendering of every result type.           | —                                       |
| `cli.rs`            | clap types, per-command handlers, `dispatch`, `AskEngine` port.      | —                                       |
| `cli_shim.rs`       | `run` — the one impure entry point (real env, stdout, on-disk db).   | **shim**                                |
| `daemon.rs`         | `Clock`, `Debouncer`, `watch_loop`, request handler / `serve_*`.     | consumes `Clock`, `FileWatcher`, `Listener`/`Conn` |
| `clock_shim.rs`     | `SystemClock` (monotonic `Instant`).                                 | `Clock` → **shim**                      |
| `watch_shim.rs`     | `NotifyWatcher` (the `notify` crate).                                | `FileWatcher` → **shim**                |
| `transport_shim.rs` | `UnixSocketListener` / `UnixConn`.                                   | `Listener` / `Conn` → **shim**          |
| `mcp.rs`            | Pure JSON-RPC handler: `initialize`, `tools/list`, `tools/call`.     | —                                       |
| `mcp_shim.rs`       | Line-delimited stdio transport pumping the handler.                  | **shim**                                |
| `embed.rs`          | `Embedder` port, `cosine`, `rrf`, `EmbeddingStore` (BLOB vectors).   | consumes `Embedder`                     |
| `embed_shim.rs`     | `VoyageEmbedder` HTTP adapter.                                       | `Embedder` → **shim**                   |
| `rag.rs`            | `ChatClient` port, hybrid `retrieve`, cited prompt assembly, `RagEngine` (`AskEngine` impl). | consumes `ChatClient`, `Embedder` |
| `chat_shim.rs`      | `AnthropicChat` — the Messages HTTP adapter.                         | `ChatClient` → **shim**                 |
| `analyze.rs`        | Session analysis (`Analyzer`, entities, prompt hashing) + store.     | consumes `Embedder`/`ChatClient` ports  |

Data flow: **sources → `index::sync` → `Db` (FTS5/trigram + `embeddings`
BLOBs) → `search` / `rag::retrieve` → `output` / `AskEngine` → CLI, daemon, or
MCP surface.**

---

## The canonical `MessageRecord` contract

Every adapter must normalize to exactly this (`src/model.rs`). Do not add
tool-specific fields; put tool quirks in the adapter, not the schema.

```rust
pub struct MessageRecord {
    pub session_id: String,   // session this message belongs to
    pub tool: Tool,           // Tool::ClaudeCode | Tool::Codex
    pub seq: u64,             // 0-based monotonic order within the session
    pub ts: i64,              // unix seconds
    pub role: Role,           // User | Assistant | Tool | System
    pub tool_name: Option<String>, // set when role == Tool
    pub uuid: Option<String>, // source-provided id, when available
    pub text: String,         // extracted plain-text body (what gets indexed)
    pub cwd: Option<String>,  // working dir recorded for the message, if any
}
```

Session-level aggregate is `SessionMeta` (`session_id`, `tool`,
`project_path`, `repo_id`, `project_name`, `git_branch`, `account`, `first_ts`,
`last_ts`). `Tool` and `Role` have stable lowercase string forms
(`as_str`/`from_str`) — `claude-code`, `codex`; `user`, `assistant`, `tool`,
`system` — and those strings are what land in storage and CLI flags. Keep them
stable.

---

## Adding a new `SessionSource` adapter

1. Create `src/source/<tool>.rs`. Implement `SessionSource`:
   - `tool(&self) -> Tool` — add a variant to `Tool` in `model.rs` first
     (extend `as_str`/`from_str` and their roundtrip test).
   - `discover(&self) -> Result<Vec<SessionFile>>` — walk files under an
     **injectable root** (`Self::new(root)` for tests, `Self::from_env()` for
     production). Never hard-code `~`.
   - `parse(&self, f, from_watermark) -> Result<ParsedDelta>` — parse only
     content past the watermark; return normalized `MessageRecord`s, a
     `SessionMeta`, and the `new_watermark`.
   - Optionally override `account()` and `config_dir()`.
2. Reuse `source/jsonl.rs` (read-whole / read-from-offset) and
   `source/fileid.rs` (file keys, size/mtime) — do not re-implement them.
3. Because discovery/parsing read a temp-rootable directory, **no shim is
   needed**: unit-test the adapter directly against a temp dir with fixture
   transcripts.
4. Re-export from `source/mod.rs` and (if part of the public surface) `lib.rs`.
5. Wire the new source into `default_sources()` in `cli_shim.rs` (the impure
   wiring point) — that file has no logic to test, so this stays covered.
6. Add fixtures (below) and tests covering: empty file, watermark resumption,
   malformed lines, and account attribution (or its absence).

Nothing else needs to change: index, search, RAG, MCP, and the daemon are
tool-agnostic by construction.

---

## Fixtures

Test fixtures live under `tests/fixtures/`, mirroring each tool's real on-disk
layout:

- `tests/fixtures/claude/.claude.json` — account attribution source.
- `tests/fixtures/claude/projects/<project>/…/*.jsonl` — Claude transcripts.
  Real Claude Code writes them as `projects/<encoded-cwd>/<uuid>.jsonl` (directly
  in the project dir); discovery accepts any `*.jsonl` under `projects/`, so a
  `sessions/` subdir works too.
- `tests/fixtures/codex/sessions/YYYY/MM/DD/*.jsonl` — Codex rollouts.

Add fixtures for a new adapter under `tests/fixtures/<tool>/…` matching its real
directory shape. Keep fixtures small and synthetic, and observe the naming
rule above (the forbidden company code-name) in every fixture and sample.
