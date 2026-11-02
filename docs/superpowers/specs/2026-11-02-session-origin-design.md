# Session origin: show the sessions the person drove

Date: 2026-11-02. Status: approved by the owner in chat ("do the changes").

## Intent

The AI sessions list in Alfred, Raycast and `csx tabs` mixes two kinds of
session. The owner started and drove some of them. A program or another agent
started the others. On the owner's Mac, 227 of 305 Claude Code sessions were
started by a program, mostly automated security reviews. They fill most of
the 50 closed-session slots. The owner wants to see only the sessions that
they drove. They want the others only on request.

A second problem has the same cause. Claude Code writes each subagent to its
own file, with the parent's session id. csx merges that file into the parent
session and stores the subagent's task prompt as a user prompt. In one of the
owner's sessions the owner typed 116 prompts, and the index holds 485 user
messages. Titles, last prompts and search then show prompts that the owner
did not type.

## Decisions the owner made

- The rule covers every tool that csx knows, not only Claude Code. New tools
  must be easy to add.
- By default, Alfred, Raycast and `csx tabs` show only the owner's sessions.
  A flag and a plugin setting show the others too.
- A session that csx cannot classify counts as the owner's. csx never hides a
  session because data is missing.
- Subagent messages stop counting as the owner's prompts, for all indexed
  tools.

## The origin

Every session and every running agent gets an origin:

| Origin      | JSON value    | Meaning                                                      |
|-------------|---------------|--------------------------------------------------------------|
| user        | `"user"`      | The person started it in a terminal or an editor.            |
| automated   | `"automated"` | A script or a tool started it with no person typing: print mode, an SDK, `codex exec`. |
| subagent    | `"subagent"`  | Another AI agent started it: a subagent, or a job that an agent ran. |
| unknown     | `null`        | The tool records nothing that csx can use.                   |

"Shown by default" means origin `user` or unknown.

## Indexed sessions

Each session source sets `SessionMeta.origin` while it parses. Some parses cannot see the
marker. One example is a later part of a Codex file, after the `session_meta`
line. Such a parse returns `None`, and the index keeps the stored value
(`COALESCE`). The index stores the origin in a new `sessions.origin` column.

| Tool        | Marker                                                         | Origin                       |
|-------------|----------------------------------------------------------------|------------------------------|
| Claude Code | `entrypoint` on each event, in the main transcript              | `cli`, `claude-vscode`, `claude-desktop` and other values: user. A value that starts with `sdk-`: automated. |
| Claude Code | `isSidechain: true` (subagent files under `<session>/subagents/`) | No origin from these files. User-role text from them is stored with role `system`. |
| Codex       | `session_meta.payload.source`                                   | `cli`, `vscode`: user. `exec`, `mcp`: automated. An object with a `subagent` key: subagent. |
| Codex       | If `source` is missing: `session_meta.payload.originator`       | `codex_exec`: automated. Other values: unknown. |
| Devin       | None found. Devin keeps subagents inside the parent session, and csx already stores only `is_user_input` prompts as user messages. | Unknown. |

These facts were checked on the owner's Mac:

- the Claude `entrypoint` values `cli`, `sdk-py` and `sdk-cli`
- the `isSidechain` flag in subagent files
- the Codex values `source: cli` and `originator: codex-tui`
- the Devin schema

 The Codex
values `exec`, `mcp` and the `subagent` object come from Codex's code and are
not on this Mac.

### Re-reading old sessions

The upgrade must fix sessions that are already in the index without a gap in
the list. A full re-index takes about 105 seconds on the owner's index.

- `SessionSource` gets `parser_version() -> u32`, default 0. A source bumps
  its version whenever it changes what it stores. Claude Code and Codex start
  at 1. Devin stays at 0.
- The `files` table gets a `parser_version` column, default 0. The indexer
  writes the source's version for every file it indexes.
- During `csx sync`, a file is stale when its stored version is lower than
  the source's version. A stale file is read again, also with no change in
  size and time.
- csx re-reads stale files per session: in one transaction, it deletes the
  session's messages (with their full-text rows and embeddings), resets the
  message count, and parses every file of that session from the start. Until
  that transaction commits, readers see the old rows. So the list never has a
  gap.
- New columns come from a helper that adds a missing column to an existing
  table. No other schema change.

## Running agents

The process list classifies running agents for all tools, in two layers.

1. Ancestry, for every tool. An agent process that has another agent process
   as an ancestor is a subagent job. One example is the security review that a
   Claude Code hook starts. There is one exception. A direct parent of the same
   tool that runs under an interpreter is a launcher, not a job starter. An
   example is the npm launcher `node .../codex`.
2. Command-line markers, per tool. The `Agent` trait gets
   `origin(&self, p: &Process) -> Option<Origin>`, default `None`.
   Claude Code: `-p` or `--print`. Codex: subcommand `exec` or `e`. Devin:
   `-p` or `--print`. All three were checked in the installed CLIs' help.
   The generic tools get a `print_markers` list in `NamedTool`, empty for now,
   because none of those tools is installed to check against.

Ancestry wins over markers. A process with neither is unknown, so it shows.
The `live_sessions` set that hides running sessions from the closed list still
uses every detected agent, also the hidden ones.

`AgentInfo` gets `origin`. The detection keeps one agent per tool and tty. It
now prefers a shown agent over a hidden one on the same tty. Then a hidden job
never takes the place of the session that the person drives.

## Output and flags

- `csx tabs` and `csx tabs --closed` hide rows and closed sessions whose
  origin is `automated` or `subagent`. The new flag `--automated` shows them.
- JSON: `agent.origin` and `closed[].origin`, each `"user"`, `"automated"`,
  `"subagent"` or `null`.
- Alfred and the human table mark a shown automated or subagent row with the
  origin word in the subtitle.
- Alfred: a workflow checkbox setting "Show automated sessions" adds
  `--automated`. `csx alfred install` keeps it, as it keeps the terminal.
- Raycast: an extension preference "Show automated sessions" adds
  `--automated`. Raycast shows the origin as a tag on the right.
- `csx search`, `show` and `mcp` do not filter by origin. They gain only the
  subagent role fix.

## Adding a tool later

- An indexed tool: set `SessionMeta.origin` in its parser. If the parser
  changes what it stores, bump `parser_version()`.
- A running tool: implement `Agent::origin`, or fill `print_markers` for a
  generic tool. Ancestry needs no work.

## Non-goals

- Filtering search results by origin.
- Guessing an origin from prompt text or folder names.

## Constraints

- csx rules in AGENTS.md apply: side effects only behind `*_shim.rs` ports,
  line and region coverage at 98% or more, fmt and clippy clean, the repo's
  commit author and trailer, and the company code name never appears.
- csx never writes to a tool's own files or databases.
- Build on branch `feat/session-origin`.
