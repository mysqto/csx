# csx

**Fast local full-text + hybrid search over your AI-coding session transcripts.**

`csx` indexes the transcripts that Claude Code and the Codex CLI leave on disk
and makes them searchable from your terminal: full-text (BM25), code-substring
(trigram), and hybrid semantic retrieval — all scoped by account, org, repo,
tool, branch, working directory, role, and time. It runs entirely on your
machine against an embedded SQLite database; nothing leaves your laptop unless
you explicitly turn on the retrieval-augmented `ask` command.

- **Multi-tool.** One index spans every AI-coding tool you use. Adapters
  normalize each tool's on-disk format into a single message schema.
- **Scoped.** Every query narrows by any combination of scope flags.
- **Local and fast.** Embedded SQLite with FTS5 + trigram; no server required.
- **Optional daemon, MCP, and RAG.** Keep the index warm with a file-watching
  daemon, expose search to agents over MCP, or ask grounded questions.

---

## Install

### Homebrew (macOS & Linux)

```sh
brew tap mysqto/csx https://github.com/mysqto/csx
brew trust mysqto/csx        # newer Homebrew requires trusting a third-party tap
brew install --cask csx
```

The cask is regenerated with real checksums by each release
(`.github/workflows/release.yml`), so it tracks the latest tag.

> Recent Homebrew refuses to load a cask from an untrusted tap until you run
> `brew trust <tap>` (a one-time consent). Skipping it yields "Refusing to load …
> from untrusted tap"; `brew trust mysqto/csx` fixes it.

### Prebuilt binary

Download the tarball for your platform from the [latest release][releases],
extract it, and put `csx` on your `PATH`. Assets are named
`csx-<tag>-<target>.tar.gz`, each with a matching `.sha256`:

| Platform | Target |
| --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |

### From source

```sh
cargo install --git https://github.com/mysqto/csx   # or, in a clone: cargo install --path .
```

Every method produces a single `csx` binary. SQLite (with the FTS5 and trigram
extensions) is compiled in via `rusqlite`'s `bundled` feature, so there is no
system SQLite dependency.

[releases]: https://github.com/mysqto/csx/releases/latest

### Configuration via environment

| Variable             | Purpose                                                        | Default               |
| -------------------- | -------------------------------------------------------------- | --------------------- |
| `CSX_DB`             | Path to the index database.                                    | `~/.csx/index.sqlite` |
| `CLAUDE_CONFIG_DIR`  | Claude Code config root to index.                              | `~/.claude`           |
| `CODEX_HOME`         | Codex CLI config root to index.                                | `~/.codex`            |
| `ANTHROPIC_API_KEY`  | Enables the `ask` command (RAG answers).                       | unset (ask disabled)  |
| `CSX_CHAT_MODEL`     | Chat model `ask` uses when `ANTHROPIC_API_KEY` is set.         | `claude-opus-4-8`     |
| `VOYAGE_API_KEY`     | Adds dense embeddings to `ask` for hybrid retrieval.           | unset (BM25 only)     |
| `CSX_EMBED_MODEL`    | Embedding model id used when `VOYAGE_API_KEY` is set.          | `voyage-3`            |

When `ANTHROPIC_API_KEY` is unset, `ask` reports that it is not configured
rather than fabricating an answer; every other command works fully offline.

---

## Usage tour

Point `csx` at your transcripts once with `sync`, then query. Re-run `sync`
whenever you want to pick up new sessions, or run the daemon (`serve`) to keep
the index warm automatically.

### `csx sync`

Walks the configured sources, incrementally parses any new transcript content
(watermarked per file, so re-syncs are cheap), resolves each session's repo id
via `git`, and upserts messages into the FTS + trigram indexes.

```sh
csx sync          # human summary
csx sync --json   # { "files_seen", "messages_added", "sessions_touched", ... }
```

### `csx query <text>`

Full-text search across all indexed messages, newest-relevant first.

```sh
csx query "flaky retry backoff"
csx query "TODO(perf)" --code            # trigram substring match (exact, code-aware)
csx query "panic" --tool codex --limit 5 --json
```

- `--code` switches from the BM25 full-text index to the trigram substring
  index — use it for literal code fragments, identifiers, and punctuation that
  a tokenizer would drop.
- `--limit N` caps the number of hits (default 20).
- `--json` emits machine-readable hits.

### `csx list`

Summarizes the indexed sources: each account/profile per tool, with message and
session counts. Answers "whose transcripts, from which tool, are in here?"

```sh
csx list
csx list --json
```

### `csx sessions`

Lists sessions (most recent last-activity first), honoring every scope flag.
This is the command the `ccswitch` integration pipes into a fuzzy finder.

```sh
csx sessions --tool claude-code --repo github.com/acme/api --limit 20
csx sessions --branch main --since 1719792000 --json
```

### `csx show <session-id>`

Prints a single session's full message stream in order.

```sh
csx show 4f2a...c19
csx show 4f2a...c19 --json
```

### `csx current`

Shows the active account per tool (the identity most recently seen indexing),
so you can confirm which login `sync` is attributing sessions to.

```sh
csx current
csx current --json
```

### `csx ask <question>`  (RAG)

Answers a natural-language question grounded in your transcripts. Retrieval
fuses the BM25 ranking with a vector-cosine ranking (reciprocal-rank fusion)
when embeddings are available, assembles the top passages into a cited context,
and asks the model to answer only from that context — with `[n]` citations back
to the sessions used.

```sh
export ANTHROPIC_API_KEY=sk-...      # required for ask
export VOYAGE_API_KEY=pa-...         # optional, enables hybrid (dense) retrieval
csx ask "how did we end up fixing the parser deadlock?" --repo github.com/acme/api
csx ask "what did I try for the CI timeout?" --tool codex --limit 8 --json
```

### `csx serve`  (daemon)

Runs a background daemon that watches the source directories, debounces bursts
of writes into a single incremental re-index, and answers scoped query requests
over a Unix socket. Keeps the index continuously up to date without repeated
manual `sync` runs.

### `csx mcp`  (Model Context Protocol)

Speaks MCP JSON-RPC over stdio so an agent can search your sessions as a tool.
It advertises three tools: `search_sessions` (query + optional scope),
`get_session` (by id), and `ask_sessions` (RAG question + optional scope). Wire
it into any MCP-capable client as a stdio server whose command is `csx mcp`.

### Scope flags

Every scoped command (`query`, `sessions`, `ask`) accepts any combination of
these; they are ANDed together:

| Flag              | Restricts to…                                        |
| ----------------- | ---------------------------------------------------- |
| `--account <uuid>`| a source account UUID                                |
| `--org <uuid>`    | a source organization UUID                           |
| `--profile <name>`| a named profile                                      |
| `--tool <id>`     | a tool: `claude-code` or `codex`                     |
| `--repo <id>`     | a repository id (normalized remote, root-commit, …)  |
| `--cwd <path>`    | a session working directory / project path           |
| `--branch <name>` | a git branch                                         |
| `--session <id>`  | a single session                                     |
| `--role <role>`   | a message role: `user`, `assistant`, `tool`, `system`|
| `--tool-call <n>` | messages that invoked tool call `<n>`                |
| `--since <unix>`  | messages at or after a unix timestamp (seconds)      |
| `--until <unix>`  | messages at or before a unix timestamp (seconds)     |

---

## Terminal tabs and Alfred

`csx tabs` lists the tabs of your terminal apps. It also shows which tab
runs an AI coding agent, such as Claude Code or Codex. You can then jump to
any tab from the command line or from Alfred. This feature works on macOS
only. On other systems `csx tabs` prints an empty list.

```sh
csx tabs                      # every tab of every running terminal
csx tabs --agents             # only tabs that run an agent
csx tabs --terminal ghostty   # only one terminal (repeat the flag for more)
csx tabs fix login            # only rows that match every word
csx tabs --json               # {"rows": [...], "warnings": [...]}
csx tabs --alfred             # Alfred script filter JSON
csx tabs focus <ref>          # bring one tab to the front
csx alfred install            # install or update the Alfred workflow
```

`--json` prints one object. `rows` holds the tabs. `warnings` holds short
messages about terminals that could not be read. `--alfred` and `--json`
cannot be used together. Each row has a `ref`. Pass it to `csx tabs focus`.
A row can have no `ref`. You cannot focus it, but you can still copy from it.

### Supported terminals

| Terminal     | Status                                |
| ------------ | ------------------------------------- |
| Ghostty 1.3+ | Checked live in a later task          |
| Terminal.app | Checked live in a later task          |
| iTerm2       | Built from documentation, not tested live |
| kitty        | Built from documentation, not tested live |
| WezTerm      | Built from documentation, not tested live |

tmux panes are listed under the terminal tab that shows their client.

### kitty setup

kitty must allow remote control. Add these lines to `kitty.conf`, then
restart kitty:

```
allow_remote_control socket-only
listen_on unix:/tmp/kitty
```

csx finds the socket in `KITTY_LISTEN_ON`. You can also pass it with
`--kitty-socket unix:/tmp/kitty`.

### Ghostty title flash

Ghostty does not report the tty of a tab. To focus an agent that csx cannot
match by title, `csx tabs focus` briefly sets the tab title to a marker. It
then puts the old title back. You may see the title flash for up to about
one second. Listing never does this.

### Permission prompt

The first time csx reads or focuses a terminal, macOS asks whether to allow
the app that runs csx (or Alfred) to control that terminal. Click Allow.
macOS asks once for each terminal app. You can change the choice later in
System Settings, under Privacy & Security, then Automation.

### Alfred workflow

Run this once, and again after you update csx:

```sh
csx alfred install
```

It finds your Alfred preferences folder, including a sync folder, and writes
the workflow there. Use `--prefs <path to Alfred.alfredpreferences>` to
choose the folder yourself. If you used the older Claude Tabs workflow, csx
keeps your keyword and removes the old workflow.

Then set the hotkeys in Alfred:

1. Open Alfred Preferences, then Workflows, then csx tabs.
2. Click the hotkey box of "Terminal tabs" (keyword `tt`) and press a key.
3. Do the same for "AI sessions" (keyword `ct`).

In the result list, press Return to focus the tab. Press Cmd+Return to copy
the session id (or the working directory). Press Option+Return to copy a
resume command, when the agent supports one. Alfred does not read your shell
PATH. The workflow looks for `csx` in the usual install folders. If yours is
elsewhere, set the `csx_path` workflow variable.

## The multi-tool adapter model

`csx` treats each AI-coding tool as a **`SessionSource`** — an adapter that
knows how to (a) discover that tool's transcript files under a config root and
(b) incrementally parse them into the canonical `MessageRecord` /
`SessionMeta` types. Two adapters ship today:

- **`ClaudeSource`** — reads `<root>/projects/<encoded-cwd>/*.jsonl` under
  `~/.claude` (or `CLAUDE_CONFIG_DIR`) — Claude Code writes one
  `<session-uuid>.jsonl` per session directly in the project dir (a `sessions/`
  subdir is also accepted) — with account attribution from `.claude.json`'s
  `oauthAccount`.
- **`CodexSource`** — reads `<root>/sessions/**/*.jsonl` under `~/.codex` (or
  `CODEX_HOME`); Codex rollout lines map `session_meta`, `message`,
  `reasoning`, `function_call`, and `function_call_output` payloads onto the
  same schema. Codex has no account object, so those sessions carry no
  attribution.

Because every source normalizes to one schema, the index, search, RAG, MCP, and
daemon layers are tool-agnostic — adding a new tool is just a new adapter (see
`AGENTS.md`).

---

## The daemon / MCP / RAG features

Both `csx serve` and `csx mcp` are live: the CLI wires the real adapters onto
the pure logic below. `serve` runs the file-watching daemon (an initial `sync`
warms the index, then it watches and re-indexes as sessions change while a
background thread answers socket queries); `mcp` runs the stdio JSON-RPC server.

- **Daemon (`serve`).** A `notify`-backed file watcher over the configured
  source roots feeds a debouncer that coalesces write bursts into one
  incremental re-index; a Unix-socket listener answers scoped query requests on
  a background thread. All the coalescing and request logic is pure and
  unit-tested; only the OS watcher and socket transport are thin adapters. The
  socket path is `$CSX_SOCK`, defaulting to `~/.csx/csx.sock`.
- **MCP (`mcp`).** A pure JSON-RPC handler implements the `initialize`,
  `tools/list`, and `tools/call` methods over a line-delimited stdio transport,
  exposing search / get-session / ask to agents. Point any MCP-capable client
  at `csx mcp` as its stdio command.
- **RAG (`ask`).** Hybrid retrieval (BM25 ⊕ vector cosine via RRF) → cited
  context assembly → a grounded chat completion → answer plus session
  citations. Vectors are stored as little-endian `f32` BLOBs and compared in
  Rust (no vector-database extension).

---

## Building for coverage

The test suite targets ≥98% line **and** region coverage. Coverage runs with
`cargo llvm-cov`, which needs an `llvm-cov`/`llvm-profdata` pair that matches
the Rust toolchain's LLVM. On macOS the simplest match is Homebrew LLVM:

```sh
brew install llvm

LLVM_COV="$(brew --prefix llvm)/bin/llvm-cov" \
LLVM_PROFDATA="$(brew --prefix llvm)/bin/llvm-profdata" \
cargo llvm-cov \
  --all-features \
  --ignore-filename-regex '(_shim\.rs$|/main\.rs$)' \
  --summary-only
```

(`$(brew --prefix llvm)` resolves to `/usr/local/opt/llvm` on Intel and
`/opt/homebrew/opt/llvm` on Apple Silicon.)

The `_shim.rs` files are the ports-and-adapters boundary — the only code that
touches the live filesystem watcher, sockets, HTTP, the MCP stdio transport, or
the `git` binary. Together with the trivial `main.rs` entry point they contain
no branching logic and are excluded from coverage via the ignore regex above. Everything else is decision logic and is
unit-tested with fakes, temp directories, and in-memory SQLite. See `AGENTS.md`
for the full discipline.

---

## Development

```sh
cargo build
cargo test
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
```

## License

MIT — see [`LICENSE`](LICENSE). Copyright (c) 2026 Chen Lei.
