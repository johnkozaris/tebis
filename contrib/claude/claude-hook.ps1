# Multi-event Claude Code hook for tebis (Telegram-to-multiplexer bridge), Windows edition.
#
# PowerShell sibling of contrib/claude/claude-hook.sh. Same event dispatch
# and wire format; transport is a Named Pipe instead of a Unix socket.
#
# Dispatches on `hook_event_name`:
#
#   UserPromptSubmit → inject summarize-at-end instruction into context
#   Stop             → forward tail of last assistant message
#   SubagentStop     → forward tail of pre-extracted last_assistant_message
#   Notification     → forward the notification message
#
# Safety (same as .sh):
# - Always exits 0; never blocks Claude stopping.
# - Never echoes transcript content to stdout/stderr (would leak to
#   Claude's log); the UserPromptSubmit branch writes JSON to stdout,
#   which is the documented hook contract.
# - Reads the JSONL transcript, not the tmux pane.
# - Guards on stop_hook_active to prevent recursive dispatch.

$ErrorActionPreference = 'Continue'

$MaxChars = 1500

$WrapInstruction = @'
[tebis] When replying, conclude your final message with a concise summary (max 1500 characters) describing what you did and any decisions the user needs to make. If the reply is short or trivial, skip the summary and answer directly. This summary is forwarded to a phone notification.
'@

# --------- pipe name resolution ----------

function Resolve-PipeName {
    # Accept an override in either pipe-name form (`tebis-john-notify`) or
    # full path form (`\\.\pipe\tebis-john-notify`) — the Rust side
    # advertises the latter in the default config.
    $override = $env:NOTIFY_SOCKET_PATH
    if ($override) {
        if ($override -match '^\\\\\.\\pipe\\(.+)$') {
            return $Matches[1]
        }
        return $override
    }
    $user = if ($env:USERNAME) { $env:USERNAME } else { 'user' }
    return "tebis-$user-notify"
}

$PipeName = Resolve-PipeName

# --------- helpers ----------

# Tail-trim to last $MaxChars chars, prepending "…" if cut.
function Tail-Trim {
    param([string]$Text)
    if (-not $Text) { return $null }
    if ($Text.Length -le $MaxChars) { return $Text }
    return '…' + $Text.Substring($Text.Length - $MaxChars)
}

# Extract last assistant text from a Claude JSONL transcript, then tail-trim.
function Tail-Of-Last-Assistant {
    param([string]$TranscriptPath)
    if (-not (Test-Path $TranscriptPath)) { return $null }

    $lastText = $null
    foreach ($line in (Get-Content -LiteralPath $TranscriptPath -ErrorAction SilentlyContinue)) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        try {
            $entry = $line | ConvertFrom-Json -ErrorAction Stop
        } catch { continue }
        if ($entry.type -ne 'assistant') { continue }
        $content = $entry.message.content
        if (-not $content) { continue }
        # content is an array of {type,text,...} blocks. Join all text blocks.
        $texts = @()
        foreach ($block in $content) {
            if ($block.type -eq 'text' -and $block.text) {
                $texts += $block.text
            }
        }
        if ($texts.Count -gt 0) {
            $lastText = ($texts -join "`n`n")
        }
    }
    return (Tail-Trim $lastText)
}

# Build payload and write it over the named pipe as one newline-terminated JSON line.
# Silent on every failure — hook must never go noisy when the bridge is down.
function Forward {
    param(
        [string]$Text,
        [string]$Kind,
        [string]$Cwd,
        [string]$Session
    )
    if (-not $Text) { return }

    $payload = [pscustomobject]@{
        text    = $Text
        kind    = $Kind
        cwd     = $Cwd
        session = $Session
    } | ConvertTo-Json -Compress -Depth 4

    try {
        $pipe = New-Object System.IO.Pipes.NamedPipeClientStream(
            '.',
            $PipeName,
            [System.IO.Pipes.PipeDirection]::InOut,
            [System.IO.Pipes.PipeOptions]::None,
            [System.Security.Principal.TokenImpersonationLevel]::Anonymous
        )
        try {
            # 2000 ms matches the `-w 2` timeout in the .sh branch.
            $pipe.Connect(2000)
        } catch {
            return
        }
        $writer = New-Object System.IO.StreamWriter($pipe)
        $writer.NewLine = "`n"
        $writer.WriteLine($payload)
        $writer.Flush()
        $writer.Dispose()
        $pipe.Dispose()
    } catch {
        return
    }
}

# --------- dispatch ----------

$rawInput = [Console]::In.ReadToEnd()
if (-not $rawInput) { exit 0 }

try {
    $in = $rawInput | ConvertFrom-Json -ErrorAction Stop
} catch {
    exit 0
}

$event = $in.hook_event_name
if (-not $event) { exit 0 }

switch ($event) {

    'UserPromptSubmit' {
        $out = [pscustomobject]@{
            hookSpecificOutput = [pscustomobject]@{
                hookEventName     = 'UserPromptSubmit'
                additionalContext = $WrapInstruction
            }
        } | ConvertTo-Json -Compress -Depth 4
        Write-Output $out
        exit 0
    }

    'Stop' {
        if ($in.stop_hook_active -eq $true) { exit 0 }

        $transcript = $in.transcript_path
        $cwd        = if ($in.cwd) { $in.cwd } else { '' }
        $session    = if ($in.session_id) { $in.session_id } else { '' }

        if (-not $transcript -or -not (Test-Path $transcript)) { exit 0 }

        $summary = Tail-Of-Last-Assistant $transcript
        if ($summary) {
            Forward -Text $summary -Kind 'stop' -Cwd $cwd -Session $session
        }
    }

    'SubagentStop' {
        $raw     = if ($in.last_assistant_message) { $in.last_assistant_message } else { '' }
        $cwd     = if ($in.cwd) { $in.cwd } else { '' }
        $session = if ($in.session_id) { $in.session_id } else { '' }

        if ($raw) {
            $summary = Tail-Trim $raw
            if ($summary) {
                Forward -Text $summary -Kind 'subagent_stop' -Cwd $cwd -Session $session
            }
        }
    }

    'Notification' {
        $msg     = if ($in.message) { $in.message } else { '' }
        $kind    = if ($in.notification_type) { $in.notification_type } else { 'notification' }
        $cwd     = if ($in.cwd) { $in.cwd } else { '' }
        $session = if ($in.session_id) { $in.session_id } else { '' }

        if ($msg) {
            $trimmed = Tail-Trim $msg
            if ($trimmed) {
                Forward -Text $trimmed -Kind $kind -Cwd $cwd -Session $session
            }
        }
    }

    default { exit 0 }
}

exit 0
