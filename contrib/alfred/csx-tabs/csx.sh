#!/bin/sh
# Run csx from the first place it is found. Used by the csx Tabs workflow.
for bin in "${csx_path:-}" /opt/homebrew/bin/csx /usr/local/bin/csx /opt/local/bin/csx "$HOME/.cargo/bin/csx" "$HOME/.local/bin/csx"; do
  if [ -n "$bin" ] && [ -x "$bin" ]; then
    exec "$bin" "$@"
  fi
done
printf '%s\n' '{"items":[{"title":"csx is not installed","subtitle":"Install csx, then run: csx alfred install","valid":false}]}'
exit 0
