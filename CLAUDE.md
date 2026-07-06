# CLAUDE.md

This repository is guided by [`AGENTS.md`](AGENTS.md) — read it first. It has
the module map, the build/test/lint commands, the coverage command (and the
Homebrew-LLVM requirement), the ports-and-adapters + `*_shim.rs` coverage
discipline, the canonical `MessageRecord` contract, and how to add a new
`SessionSource` adapter.

Two rules that are easy to get wrong:

- **Keep logic out of `*_shim.rs`.** Those files hold the one real adapter for a
  trait (filesystem watcher, sockets, HTTP, MCP stdio, the `git` binary) and are
  excluded from coverage. Any branch a test needs belongs in a non-shim module.
- **Coverage must stay ≥98% line and region.** Run it exactly as `AGENTS.md`
  documents (it needs an `llvm-cov`/`llvm-profdata` matching the toolchain's
  LLVM), and add tests for anything you touch.
