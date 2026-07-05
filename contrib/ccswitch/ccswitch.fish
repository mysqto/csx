# ccswitch — optional fish binding for csx.
#
# `csx` stays completely decoupled from any tool's resume mechanism; this file
# is the thin, user-side glue described in the design. It fuzzy-picks a session
# out of `csx` and hands its id to the originating tool's resume command.
#
# Sub-commands:
#   ccswitch search [--tool <id>] [scope flags...]   pick a session and resume it
#
# Requirements: `csx` and `fzf` on PATH. `jq` is used if present for a nicer
# picker list; without it we fall back to `csx sessions` plain output.

function ccswitch --description 'Fuzzy-pick and resume an AI-coding session via csx'
    set -l cmd $argv[1]
    set -e argv[1]

    switch "$cmd"
        case search ''
            __ccswitch_search $argv
        case '*'
            echo "ccswitch: unknown subcommand '$cmd'" >&2
            echo "usage: ccswitch search [--tool <id>] [scope flags...]" >&2
            return 2
    end
end

# Resolve the tool to default the scope to. If the caller passed `--tool`, honor
# it; otherwise fall back to the account/tool csx reports as currently active.
function __ccswitch_default_tool
    for i in (seq (count $argv))
        if test "$argv[$i]" = --tool
            set -l next (math $i + 1)
            echo $argv[$next]
            return 0
        end
    end
    # No explicit --tool: use the first active tool csx knows about.
    if type -q jq
        csx current --json 2>/dev/null | jq -r '.[0].tool // empty' 2>/dev/null
    else
        # Plain output: first token on the first data line.
        csx current 2>/dev/null | string match -r -v '^(TOOL|no )' | head -n1 | string split -f1 ' \t'
    end
end

function __ccswitch_search
    if not type -q csx
        echo 'ccswitch: csx not found on PATH' >&2
        return 127
    end
    if not type -q fzf
        echo 'ccswitch: fzf not found on PATH' >&2
        return 127
    end

    set -l tool (__ccswitch_default_tool $argv)

    # Build the scope: default to the active tool when the caller gave none.
    set -l scope $argv
    if not contains -- --tool $argv; and test -n "$tool"
        set scope --tool $tool $scope
    end

    # Ask csx for the candidate sessions as JSON, then project each row to a
    # "session_id<TAB>human label" line for fzf. The preview and the final
    # selection both key off the first (id) field.
    set -l rows
    if type -q jq
        set rows (csx sessions --json $scope | jq -r '.[] | "\(.session_id)\t\(.tool // "-")  \(.project_name // "-")  \(.git_branch // "-")  (\(.msg_count) msgs)"')
    else
        # Without jq, fall back to `csx sessions` and take the trailing SESSION
        # column; the row itself is the label.
        set rows (csx sessions $scope | string match -r -v '^(LAST|no )')
    end

    if test -z "$rows"
        echo 'ccswitch: no sessions matched' >&2
        return 1
    end

    # Pick one. Preview renders the full session via `csx show <id>`; {1} is the
    # first (tab-delimited) field, i.e. the session id.
    set -l picked (printf '%s\n' $rows | fzf \
        --with-nth=2.. \
        --delimiter='\t' \
        --preview 'csx show {1}' \
        --preview-window='right,60%,wrap' \
        --prompt='session> ')

    or return $status

    set -l id (printf '%s' $picked | string split -f1 \t)
    if test -z "$id"
        return 1
    end

    __ccswitch_resume "$tool" "$id"
end

# Map a tool id onto its resume invocation. csx never runs these itself; the
# binding decides. Extend this switch to teach ccswitch a new tool.
function __ccswitch_resume --argument-names tool id
    switch "$tool"
        case claude-code ''
            claude --resume $id
        case codex
            codex resume $id
        case '*'
            echo "ccswitch: don't know how to resume tool '$tool' (session $id)" >&2
            return 2
    end
end
