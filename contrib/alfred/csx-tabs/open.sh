#!/bin/sh
# Resume a closed session for the csx Tabs workflow.
#
#   open.sh pick        Alfred items: the installed terminals for the session
#                       in $csx_session of tool $csx_tool. Picking one sets
#                       csx_terminal and csx_tool again, and csx_save=1
#                       unless csx_kind is closed_pick.
#   open.sh open <id>   Open session <id> of tool $csx_tool in $csx_terminal,
#                       else in the saved $terminal. With csx_save=1, save the
#                       terminal after the open works. Print an error, else
#                       nothing.
#
# The workflow passes only a tool, a session id and a terminal id to csx,
# never a command.
here=$(cd "$(dirname "$0")" && pwd)

# One item that cannot be actioned. Callers pass fixed text without quotes.
message() {
  printf '{"items":[{"title":"%s","subtitle":"%s","valid":false}]}\n' "$1" "$2"
}

pick() {
  session=${csx_session:-}
  case $session in
  '' | -* | *[!A-Za-z0-9._-]*)
    message "No session to open" "Choose a closed session in the AI sessions list"
    return
    ;;
  esac
  # The item names csx_tool again: Alfred is not trusted to keep it across
  # the picker. Only the tools csx resumes are passed on.
  tool=${csx_tool:-}
  case $tool in
  claude-code | codex | devin) ;;
  *)
    message "Cannot resume this session" "csx resumes Claude Code, Codex and Devin sessions"
    return
    ;;
  esac
  save=1
  if [ "${csx_kind:-}" = closed_pick ]; then
    save=0
  fi
  json=$("$here/csx.sh" tabs terminals --json 2>/dev/null)
  case $json in
  '['*) ;;
  *)
    message "Could not list terminals" "Check that this works: csx tabs terminals"
    return
    ;;
  esac
  # The list is [{"id":"...","name":"..."},...]. Split on quotes; keep only
  # ids and names of plain characters, so nothing needs escaping.
  printf '%s\n' "$json" | awk -F'"' -v session="$session" -v save="$save" -v tool="$tool" '
    {
      for (i = 1; i < NF; i++) {
        if ($i == "id") { n++; ids[n] = $(i + 2) }
        else if ($i == "name" && n > 0) { names[n] = $(i + 2) }
      }
    }
    END {
      out = ""
      for (k = 1; k <= n; k++) {
        id = ids[k]; name = names[k]
        if (id !~ /^[a-z0-9]+$/ || name !~ /^[A-Za-z0-9 ]+$/) continue
        if (save == "1") subt = "Open in " name " and use it from now on"
        else subt = "Open in " name " this time"
        if (out != "") out = out ","
        out = out "{\"title\":\"" name "\",\"subtitle\":\"" subt "\",\"arg\":\"" session \
          "\",\"valid\":true,\"variables\":{\"csx_terminal\":\"" id "\",\"csx_save\":\"" save "\",\"csx_tool\":\"" tool "\"}}"
      }
      if (out == "") {
        out = "{\"title\":\"No supported terminal is installed\",\"subtitle\":\"Install Ghostty, iTerm2, kitty or WezTerm\",\"valid\":false}"
      }
      print "{\"items\":[" out "]}"
    }'
}

open_session() {
  term=${csx_terminal:-${terminal:-}}
  if [ -z "$term" ]; then
    echo "csx: choose a terminal first (Ctrl+Return on the session)"
    return
  fi
  case $term in
  *[!a-z0-9]*)
    echo "csx: unknown terminal"
    return
    ;;
  esac
  # Success prints nothing or "focused <ref>"; only an error is shown, and
  # then nothing is saved.
  if ! err=$("$here/csx.sh" tabs open --terminal "$term" -- "${csx_tool:-}" "$1" 2>&1 >/dev/null); then
    printf '%s\n%s\n' "$err" "Ctrl+Return to choose another terminal"
    return
  fi
  if [ "${csx_save:-}" = 1 ]; then
    osascript -e 'on run argv' \
      -e 'tell application id "com.runningwithcrayons.Alfred" to set configuration "terminal" to value (item 1 of argv) in workflow "to.mysq.csx-tabs" exportable false' \
      -e 'end run' "$term" >/dev/null 2>&1
  fi
}

case ${1:-} in
pick) pick ;;
open) open_session "${2:-}" ;;
*) echo "csx: unknown mode" ;;
esac
exit 0
