# Contributing to csx

Thanks for helping out. This is a small, focused Rust project; the bar is a
green tree and honest tests.

## Prerequisites

- A Rust toolchain (stable).
- For coverage on macOS: Homebrew LLVM (`brew install llvm`) so `llvm-cov` /
  `llvm-profdata` match the toolchain's LLVM. See **README → Building for
  coverage** for the exact command.

## The loop

```sh
cargo build
cargo test
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
```

All four must be clean before you open a PR. Coverage must stay **≥98% line and
region** — run it as documented in the README / `AGENTS.md`.

## Architecture you must respect

csx is ports-and-adapters. **All** real OS/network I/O — the filesystem
watcher, sockets, HTTP, the MCP stdio transport, and spawning `git` — lives in
`*_shim.rs` files behind a trait, and those files are excluded from coverage.
Every piece of decision logic lives outside a shim and is unit-tested with
fakes, temp directories, and in-memory SQLite. If you find yourself unable to
test a branch, it usually means logic leaked into a shim — move it out.

`AGENTS.md` is the full guide: the module map, the coverage discipline, the
canonical `MessageRecord` contract, and a step-by-step for **adding a new
`SessionSource` adapter** (the most common contribution). Read it first.

## Pull requests

- Keep changes focused; one concern per PR.
- Add or update tests for anything you touch; keep coverage ≥98%.
- Update `CHANGELOG.md` under an `## [Unreleased]` heading.
- Never commit secrets, tokens, or real session transcripts. Fixtures must use
  synthetic data (`example.com`, `acme`, …).

## Releases

Releases are cut by pushing a `v*` tag; `.github/workflows/release.yml`
cross-builds the binaries, publishes the GitHub release, and bumps the Homebrew
cask automatically. Bump the version in `Cargo.toml` and move the changelog's
`[Unreleased]` entries under the new version before tagging.
