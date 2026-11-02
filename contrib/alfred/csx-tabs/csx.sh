#!/bin/sh
# Run csx from the first place it is found. Used by the csx Tabs workflow.
PATH="/opt/homebrew/bin:/usr/local/bin:/opt/local/bin:$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
export PATH
# The workflow setting "Show automated sessions" (variable show_automated, 1
# when ticked) adds --automated to the tab lists, not to the subcommands.
if [ "${1:-}" = tabs ]; then
  case "${2:-}:${show_automated:-}" in
  -*:1 | -*:true)
    shift
    set -- tabs --automated "$@"
    ;;
  esac
fi
for bin in "${csx_path:-}" /opt/homebrew/bin/csx /usr/local/bin/csx /opt/local/bin/csx "$HOME/.cargo/bin/csx" "$HOME/.local/bin/csx"; do
  if [ -n "$bin" ] && [ -x "$bin" ]; then
    exec "$bin" "$@"
  fi
done
printf '%s\n' '{"items":[{"title":"csx is not installed","subtitle":"Install csx, then run: csx alfred install","valid":false}]}'
exit 0
