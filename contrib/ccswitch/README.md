# ccswitch — optional fish binding for csx

`ccswitch` is the **optional** shell glue described in the csx design. `csx`
itself stays completely decoupled from how any tool resumes a session: it only
searches and prints. `ccswitch` is the thin, user-side layer that turns a csx
session pick into a resume command for the tool that produced it.

```
ccswitch search  ──▶  csx sessions --json --tool <active>   (scoped to the active account/tool)
                 ──▶  fzf  (--preview 'csx show {id}')
                 ──▶  <tool> --resume <selected id>          (e.g. claude --resume <id>)
```

## What it does

`ccswitch search [--tool <id>] [scope flags…]`

1. Defaults the scope to the currently active tool (from `csx current`) unless
   you pass `--tool` yourself. Any other csx scope flag (`--repo`, `--branch`,
   `--since`, …) is passed straight through.
2. Runs `csx sessions --json …` and fuzzy-picks a session in `fzf`, previewing
   the full transcript with `csx show <id>` in the preview pane.
3. Resumes the picked session with the originating tool's own command
   (`claude --resume <id>`, `codex resume <id>`, …).

## Requirements

- `csx` and [`fzf`](https://github.com/junegunn/fzf) on `PATH`.
- [`jq`](https://jqlang.github.io/jq/) is optional but recommended — with it the
  picker shows a clean `tool  project  branch  (N msgs)` label per row; without
  it, `ccswitch` falls back to `csx sessions` plain output.

## Install

### Source it directly

Add to `~/.config/fish/config.fish`:

```fish
source /path/to/csx/contrib/ccswitch/ccswitch.fish
```

or drop the file into fish's autoload directory so it loads on demand:

```fish
cp contrib/ccswitch/ccswitch.fish ~/.config/fish/functions/ccswitch.fish
```

### As a fisher plugin

Point [`fisher`](https://github.com/jorgebucaran/fisher) at the file (or a fork
that vendors it):

```fish
fisher install /path/to/csx/contrib/ccswitch
```

## Extending it to a new tool

Resume mapping lives in one place — the `__ccswitch_resume` switch in
`ccswitch.fish`. Add a `case <tool-id>` arm returning the tool's resume command.
The tool id matches csx's stable identifiers (`claude-code`, `codex`, …).
