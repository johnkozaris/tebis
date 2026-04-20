#!/usr/bin/env bash
#
# GitHub Copilot CLI hook for tebis. Dispatches on the event name found
# in the stdin JSON (`hook_event_name` for VS Code/snake_case payloads
# since v1.0.21; falls back to inferring from `eventName` field).
#
# Events handled (all confirmed in the Copilot CLI changelog; see
# src/agent_hooks/copilot.rs for the version-added map):
#   userPromptSubmitted → inject "conclude with a summary" context
#   agentStop           → forward tail of last assistant message
#   subagentStop        → forward subagent tail tagged by agentName
#   notification        → forward the message text with kind tag
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
# - Reads the JSONL transcript file, not the terminal.

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

# Extract the last assistant text from a Copilot transcript (JSONL) and
# return the TAIL (not head) of MAX_CHARS codepoints.
tail_of_last_assistant() {
    local transcript="$1"
    # The exact field name for assistant-role entries varies slightly
    # across Copilot CLI versions. Match the common shapes: entries with
    # role "assistant" and either .content (string) or .message.content
    # (string or array of {type:"text",text}).
    jq -rs --argjson max "$MAX_CHARS" '
        def extract_text: (
            if type == "string" then .
            elif type == "array" then
              (map(select(.type == "text") | .text) | join("\n\n"))
            elif type == "object" and (.content // empty) != empty then
              (.content | extract_text)
            else empty end
        );
        (map(select(.role == "assistant" or (.type // "") == "assistant"))
         | last // empty
         | ((.content // .message.content // empty) | extract_text)) as $s |
        if ($s | length) == 0 then empty
        elif ($s | length) > $max then ("…" + $s[-$max:])
        else $s end
    ' "$transcript" 2>/dev/null
}

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

# Transcript + cwd + session id are named consistently across both forms.
TRANSCRIPT="$(jq -r '.transcriptPath // .transcript_path // ""' <<<"$INPUT")"
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

    agentstop | stop)
        if [[ -z "$TRANSCRIPT" || ! -f "$TRANSCRIPT" ]]; then
            exit 0
        fi
        SUMMARY="$(tail_of_last_assistant "$TRANSCRIPT")"
        if [[ -n "$SUMMARY" ]]; then
            forward "$SUMMARY" "stop" "$CWD" "$SESSION"
        fi
        ;;

    subagentstop)
        # Copilot passes us the agent name so the header can distinguish
        # named subagents. The transcript still has the text.
        AGENT_NAME="$(jq -r '.agentName // .agent_name // "subagent"' <<<"$INPUT")"
        if [[ -z "$TRANSCRIPT" || ! -f "$TRANSCRIPT" ]]; then
            exit 0
        fi
        SUMMARY="$(tail_of_last_assistant "$TRANSCRIPT")"
        if [[ -n "$SUMMARY" ]]; then
            forward "$SUMMARY" "subagent_stop" "$CWD" "$AGENT_NAME"
        fi
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
