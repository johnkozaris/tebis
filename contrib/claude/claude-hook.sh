#!/usr/bin/env bash
#
# Multi-event Claude Code hook for tebis (Telegram-to-multiplexer bridge).
#
# Dispatches on `hook_event_name`:
#
#   UserPromptSubmit   → inject summarize-at-end instruction into context
#   Stop               → forward tail of last assistant message
#   SubagentStop       → forward tail of pre-extracted last_assistant_message
#   Notification       → forward the notification message (permission asks,
#                        idle prompts, etc.)
#
# SessionStart / SessionEnd are intentionally not handled — tebis is a
# single-user bot where the agent is provisioned BY the user's first
# message, so the reply itself is proof of life; ship-state pings
# duplicate signal.
#
# The UserPromptSubmit wrap plus the Stop tail-extract is the summarization
# strategy: Claude is asked to conclude every non-trivial reply with a short
# summary, and we take the last N chars of the last assistant message — so
# if Claude complied, the tail *is* the summary. If Claude wrote no summary,
# we still get the tail of whatever it said, which is usually the conclusion.
#
# Install: see contrib/claude/claude-settings.example.json.
#
# Dependencies: jq, nc (BSD netcat — on Linux: `apt install netcat-openbsd`).
#
# Safety:
# - Exits 0 on every path (never blocks Claude from stopping).
# - Never echoes transcript content to stdout/stderr (would leak to Claude's
#   log); UserPromptSubmit branch is the one exception — it writes JSON to
#   stdout, which is how the hook contract works.
# - Reads the JSONL transcript, not the tmux pane.
# - Guards on stop_hook_active to prevent recursive Stop-hook dispatch.

set -u
set -o pipefail

MAX_CHARS=1500

# Prompt wrap injected by UserPromptSubmit. Position-independent phrasing
# so it works whether the hook appends vs. prepends context.
WRAP_INSTRUCTION=$(
    cat <<'PROMPT'
[tebis] When replying, conclude your final message with a concise summary (max 1500 characters) describing what you did and any decisions the user needs to make. If the reply is short or trivial, skip the summary and answer directly. This summary is forwarded to a phone notification.
PROMPT
)

resolve_socket() {
    if [[ -n "${NOTIFY_SOCKET_PATH:-}" ]]; then
        printf '%s' "$NOTIFY_SOCKET_PATH"
        return
    fi
    if [[ -n "${XDG_RUNTIME_DIR:-}" ]]; then
        printf '%s/tebis.sock' "$XDG_RUNTIME_DIR"
        return
    fi
    printf '/tmp/tebis-%s.sock' "${USER:-unknown}"
}

SOCKET="$(resolve_socket)"

# Extract last assistant text block from a JSONL transcript and return the
# TAIL (not head) of MAX_CHARS codepoints — prepending an ellipsis when cut.
# Tail-first because Claude's conclusions are at the end of the message.
tail_of_last_assistant() {
    local transcript="$1"
    jq -rs --argjson max "$MAX_CHARS" '
        (map(select(.type == "assistant")) | last // empty |
         (.message.content // []) |
         map(select(.type == "text") | .text) |
         join("\n\n")) as $s |
        if ($s | length) == 0 then empty
        elif ($s | length) > $max then ("…" + $s[-$max:])
        else $s end
    ' "$transcript" 2>/dev/null
}

# Tail-truncate an inline string. Used for SubagentStop's pre-extracted
# last_assistant_message and Notification's message field.
tail_trim() {
    local s="$1"
    jq -rn --arg s "$s" --argjson max "$MAX_CHARS" '
        if ($s | length) == 0 then empty
        elif ($s | length) > $max then ("…" + $s[-$max:])
        else $s end
    '
}

# Build the bridge payload and send it. Newline-framed — no half-close
# needed, so stock macOS `nc` works without the `-N` flag.
forward() {
    local text="$1"
    local kind="$2"
    local cwd="$3"
    local session="$4"

    if [[ ! -S "$SOCKET" ]]; then
        # Bridge not running — nothing to do. Silent so the hook never
        # becomes noise when the bridge is paused.
        return
    fi

    local payload
    payload="$(jq -nc \
        --arg text "$text" \
        --arg kind "$kind" \
        --arg cwd "$cwd" \
        --arg session "$session" \
        '{text: $text, kind: $kind, cwd: $cwd, session: $session}')"

    printf '%s\n' "$payload" | nc -U -w 2 "$SOCKET" >/dev/null 2>&1 || true
}

INPUT="$(cat)"
EVENT="$(jq -r '.hook_event_name // ""' <<<"$INPUT")"

case "$EVENT" in

    UserPromptSubmit)
        jq -nc --arg ctx "$WRAP_INSTRUCTION" '{
            hookSpecificOutput: {
                hookEventName: "UserPromptSubmit",
                additionalContext: $ctx
            }
        }'
        exit 0
        ;;

    Stop)
        # Recursion guard — if a downstream Stop hook already ran, bail.
        STOP_ACTIVE="$(jq -r '.stop_hook_active // false' <<<"$INPUT")"
        if [[ "$STOP_ACTIVE" == "true" ]]; then
            exit 0
        fi

        CWD="$(jq -r '.cwd // ""' <<<"$INPUT")"
        SESSION="$(jq -r '.session_id // ""' <<<"$INPUT")"

        # Prefer inline `last_assistant_message` — Claude's transcript file
        # writes are async, so the on-disk copy lags by one turn on Stop.
        RAW="$(jq -r '.last_assistant_message // ""' <<<"$INPUT")"
        if [[ -n "$RAW" ]]; then
            SUMMARY="$(tail_trim "$RAW")"
        else
            TRANSCRIPT="$(jq -r '.transcript_path // ""' <<<"$INPUT")"
            if [[ -z "$TRANSCRIPT" || ! -f "$TRANSCRIPT" ]]; then
                exit 0
            fi
            SUMMARY="$(tail_of_last_assistant "$TRANSCRIPT")"
        fi
        if [[ -n "$SUMMARY" ]]; then
            forward "$SUMMARY" "stop" "$CWD" "$SESSION"
        fi
        ;;

    SubagentStop)
        RAW="$(jq -r '.last_assistant_message // ""' <<<"$INPUT")"
        CWD="$(jq -r '.cwd // ""' <<<"$INPUT")"
        SESSION="$(jq -r '.session_id // ""' <<<"$INPUT")"

        if [[ -n "$RAW" ]]; then
            SUMMARY="$(tail_trim "$RAW")"
            if [[ -n "$SUMMARY" ]]; then
                forward "$SUMMARY" "subagent_stop" "$CWD" "$SESSION"
            fi
        fi
        ;;

    Notification)
        # Permission prompts, idle prompts, auth-success. The message field
        # is already human-readable; no transcript parsing needed.
        MSG="$(jq -r '.message // ""' <<<"$INPUT")"
        KIND="$(jq -r '.notification_type // "notification"' <<<"$INPUT")"
        CWD="$(jq -r '.cwd // ""' <<<"$INPUT")"
        SESSION="$(jq -r '.session_id // ""' <<<"$INPUT")"

        if [[ -n "$MSG" ]]; then
            TRIMMED="$(tail_trim "$MSG")"
            if [[ -n "$TRIMMED" ]]; then
                forward "$TRIMMED" "$KIND" "$CWD" "$SESSION"
            fi
        fi
        ;;

    *)
        exit 0
        ;;
esac

exit 0
