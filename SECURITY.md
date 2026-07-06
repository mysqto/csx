# Security

## What csx touches

csx reads your local AI-coding **session transcripts** (from `~/.claude` and
`~/.codex`, or the roots you point it at) and each tool's local account-identity
file, and writes a **local** SQLite index (default `~/.csx/index.sqlite`). All
indexing, search, and the daemon run entirely on your machine — nothing leaves
it.

Your transcripts can contain secrets, tokens, proprietary code, and personal
data. Treat the index database with the same care as the transcripts: it lives
under your home directory; don't sync it to a shared or public location
unencrypted.

## When data leaves your machine

Only the optional `ask` (RAG) path makes network calls, and only when you opt
in by setting the relevant keys:

- `ANTHROPIC_API_KEY` — `csx ask` sends the retrieved transcript excerpts and
  your question to the Anthropic Messages API to compose a cited answer.
- `VOYAGE_API_KEY` — enables dense embeddings; message text is sent to Voyage to
  be embedded for hybrid retrieval.

With neither key set, `ask` reports that it is not configured and every other
command works fully offline. The `serve` (daemon) and `mcp` surfaces do not make
outbound network calls on their own.

## Repository hygiene

- No secrets, tokens, or real transcripts are committed. Test fixtures use
  synthetic data only.
- The release workflow signs nothing secret into artifacts; the Homebrew cask
  ships published release tarballs verified by SHA-256.

## Reporting a vulnerability

Please report suspected vulnerabilities privately via a
[GitHub security advisory](https://github.com/mysqto/csx/security/advisories/new)
rather than a public issue. Include reproduction steps and impact; you'll get an
acknowledgement and a fix timeline.
