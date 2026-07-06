# csx-pick — optional standalone fish picker for csx.
#
# If you use the ccswitch plugin (github.com/mysqto/ccswitch), you already have
# this as `ccswitch search` — use that and ignore this file. `csx-pick` is the
# same picker for people who want it WITHOUT the ccswitch account-switcher, so
# it deliberately uses its own name and helpers (no collision with ccswitch).
#
# csx stays decoupled from any tool's resume mechanism; this is the thin,
# user-side glue that turns a csx session pick into a resume command.
#
#   csx-pick [--tool <id>] [scope flags...]   pick a session and resume it
#
# Requires `csx` and `fzf` on PATH; `jq` optional (nicer picker labels).

function csx-pick --description 'Fuzzy-pick and resume an AI-coding session via csx'
    if not type -q csx
        echo 'csx-pick: csx not found on PATH' >&2
        return 127
    end
    if not type -q fzf
        echo 'csx-pick: fzf not found on PATH' >&2
        return 127
    end

    # default the scope to the active tool unless the caller passed --tool
    set -l tool
    if contains -- --tool $argv
        for i in (seq (count $argv))
            test "$argv[$i]" = --tool; and set tool $argv[(math $i + 1)]
        end
    else if type -q jq
        set tool (csx current --json 2>/dev/null | jq -r '.[0].tool // empty' 2>/dev/null)
    end

    set -l scope $argv
    if not contains -- --tool $argv; and test -n "$tool"
        set scope --tool $tool $scope
    end

    set -l rows
    if type -q jq
        set rows (csx sessions --json $scope 2>/dev/null | jq -r '.[] | "\(.session_id)\t\(.tool // "-")  \(.project_name // "-")  \(.git_branch // "-")  (\(.msg_count) msgs)"')
    else
        set rows (csx sessions $scope 2>/dev/null | string match -r -v '^(LAST|no )')
    end
    if test -z "$rows"
        echo 'csx-pick: no sessions matched' >&2
        return 1
    end

    set -l picked (printf '%s\n' $rows | fzf --with-nth=2.. --delimiter='\t' \
        --preview 'csx show {1}' --preview-window='right,60%,wrap' --prompt='session> ')
    or return $status
    set -l id (printf '%s' $picked | string split -f1 \t)
    test -z "$id"; and return 1

    __csx_pick_resume "$tool" "$id"
end

# Map a tool id onto its resume command. csx never runs these itself.
function __csx_pick_resume --argument-names tool id
    switch "$tool"
        case claude-code ''
            command claude --resume $id
        case codex
            command codex resume $id
        case '*'
            echo "csx-pick: don't know how to resume tool '$tool' (session $id)" >&2
            return 2
    end
end
