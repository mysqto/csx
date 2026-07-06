# Shell integration for csx

`csx` only searches and prints; it stays decoupled from how any tool resumes a
session. This directory holds the thin, user-side glue that turns a csx session
pick into a resume command.

## If you use [ccswitch](https://github.com/mysqto/ccswitch)

You already have it — `ccswitch search` (alias `s`) is built in:

```fish
ccswitch search                  # sessions for the active tool → fzf → resume
ccswitch search --repo payments  # any csx scope flag passes straight through
```

Nothing to install here.

## If you don't use ccswitch — `csx-pick.fish`

`csx-pick.fish` is the same picker as a standalone function, with its own name
and helpers so it never collides with the ccswitch plugin.

```fish
csx-pick                  # sessions for the active tool → fzf → resume
csx-pick --repo payments  # any csx scope flag passes straight through
```

It runs `csx sessions --json` (scoped to the active tool unless you pass
`--tool`), previews each transcript with `csx show <id>` in `fzf`, and resumes
the pick with the originating tool's command (`claude --resume <id>`,
`codex resume <id>`, …).

### Install

Source it, or drop it into fish's autoload dir:

```fish
source /path/to/csx/contrib/fish/csx-pick.fish
# or
cp contrib/fish/csx-pick.fish ~/.config/fish/functions/csx-pick.fish
```

### Requirements

- `csx` and [`fzf`](https://github.com/junegunn/fzf) on `PATH`.
- [`jq`](https://jqlang.github.io/jq/) optional — nicer `tool  project  branch  (N msgs)` labels; without it, falls back to `csx sessions` plain output.

### Adding a new tool

Resume mapping lives in one place — the `__csx_pick_resume` switch. Add a
`case <tool-id>` arm returning the tool's resume command; the tool id matches
csx's stable identifiers (`claude-code`, `codex`, …).
