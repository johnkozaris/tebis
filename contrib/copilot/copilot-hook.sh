#!/usr/bin/env bash
#
# GitHub Copilot CLI hook for tebis. Dispatches on the event name found
# in the stdin JSON (`hook_event_name` is the snake_case form Copilot
# sends for `_vsCodeCompat` hooks; native hooks send `eventName`).
#
# Events handled (verified against @github/copilot 1.0.48 app.js, May 2026):
#   userPromptSubmitted → inject "conclude with a summary" context
#   agentStop           → forward tail of last assistant message
#                         (added Copilot CLI v1.0.45, fires on task_complete)
#   subagentStop        → forward tail of sub-agent's last assistant message
#   notification        → forward permission / completion notifications
#                         (idle pings dropped — see Notification branch)
#
# sessionStart / sessionEnd intentionally not handled — same
# rationale as Claude: the agent reply itself proves the session is
# up, so ship-state pings are noise on a single-user bot.
#
# Safety:
# - Exits 0 on every path so Copilot never blocks on a failed delivery.
# - Never echoes transcript content to stdout (would leak to Copilot's
#   log); userPromptSubmitted writes hookSpecificOutput JSON which is
#   the documented contract.

set -u
set -o pipefail

MAX_CHARS=1500

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

tail_trim() {
    local s="$1"
    jq -rn --arg s "$s" --argjson max "$MAX_CHARS" '
        if ($s | length) == 0 then empty
        elif ($s | length) > $max then ("…" + $s[-$max:])
        else $s end
    '
}

# Read Copilot's events.jsonl and tail-trim the last `assistant.message`.
# `data.content` is the model's text. Subagent messages carry an `agentId`
# field; main-agent messages omit it. For agentStop we want the last
# main-agent message; for subagentStop we want the last subagent message.
# Caller passes "main" or "sub" as the second arg.
tail_of_last_assistant() {
    local transcript="$1"
    local scope="$2"
    [[ -f "$transcript" ]] || return 0
    local filter
    if [[ "$scope" == "sub" ]]; then
        filter='select(.type == "assistant.message" and (.agentId // "") != "")'
    else
        filter='select(.type == "assistant.message" and (.agentId // "") == "")'
    fi
    jq -rs --argjson max "$MAX_CHARS" "
        (map($filter) | last // empty | .data.content // \"\") as \$s |
        if (\$s | length) == 0 then empty
        elif (\$s | length) > \$max then (\"…\" + \$s[-\$max:])
        else \$s end
    " "$transcript" 2>/dev/null
}

forward() {
    local text="$1"
    local kind="$2"
    local cwd="$3"
    local session="$4"

    if [[ ! -S "$SOCKET" ]]; then
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

EVENT="$(
    jq -r '(.hook_event_name // .eventName // "") | ascii_downcase' <<<"$INPUT"
)"

CWD="$(jq -r '.cwd // ""' <<<"$INPUT")"
SESSION="$(jq -r '.sessionId // .session_id // ""' <<<"$INPUT")"
TRANSCRIPT="$(jq -r '.transcriptPath // .transcript_path // ""' <<<"$INPUT")"

case "$EVENT" in

    userpromptsubmitted | userpromptsubmit)
        jq -nc --arg ctx "$WRAP_INSTRUCTION" '{
            hookSpecificOutput: {
                hookEventName: "userPromptSubmitted",
                additionalContext: $ctx
            }
        }'
        exit 0
        ;;

    agentstop)
        if [[ -n "$TRANSCRIPT" ]]; then
            SUMMARY="$(tail_of_last_assistant "$TRANSCRIPT" main)"
            if [[ -n "$SUMMARY" ]]; then
                forward "$SUMMARY" "stop" "$CWD" "$SESSION"
            fi
        fi
        ;;

    subagentstop)
        if [[ -n "$TRANSCRIPT" ]]; then
            SUMMARY="$(tail_of_last_assistant "$TRANSCRIPT" sub)"
            if [[ -n "$SUMMARY" ]]; then
                forward "$SUMMARY" "subagent_stop" "$CWD" "$SESSION"
            fi
        fi
        ;;

    notification)
        MSG="$(jq -r '.message // ""' <<<"$INPUT")"
        KIND="$(jq -r '.notificationType // .notification_type // "notification"' <<<"$INPUT")"
        # Drop idle/"waiting for input" pings — they fire on every turn
        # end and duplicate the agentStop signal.
        case "$KIND" in
            idle | idle_*) exit 0 ;;
        esac
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
