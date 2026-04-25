#!/usr/bin/env bash
#
# GitHub Copilot CLI hook for tebis. Dispatches on the event name found
# in the stdin JSON (`hook_event_name` for VS Code/snake_case payloads
# since v1.0.21; falls back to inferring from `eventName` field).
#
# Events handled (all confirmed in the Copilot CLI changelog; see
# src/agent_hooks/copilot.rs for the version-added map):
#   userPromptSubmitted → inject "conclude with a summary" context
#   notification        → forward completion / permission / idle messages
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

# -------- socket path resolution -------------------------------------------

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

# -------- helpers ----------------------------------------------------------

tail_trim() {
    local s="$1"
    jq -rn --arg s "$s" --argjson max "$MAX_CHARS" '
        if ($s | length) == 0 then empty
        elif ($s | length) > $max then ("…" + $s[-$max:])
        else $s end
    '
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

# -------- dispatch ---------------------------------------------------------

INPUT="$(cat)"

# Normalize event name: prefer the snake_case hook_event_name (v1.0.21+),
# fall back to the camelCase eventName (legacy).
EVENT="$(
    jq -r '(.hook_event_name // .eventName // "") | ascii_downcase' <<<"$INPUT"
)"

CWD="$(jq -r '.cwd // ""' <<<"$INPUT")"
SESSION="$(jq -r '.sessionId // .session_id // ""' <<<"$INPUT")"

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

    notification)
        MSG="$(jq -r '.message // ""' <<<"$INPUT")"
        KIND="$(jq -r '.notificationType // .notification_type // "notification"' <<<"$INPUT")"
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
