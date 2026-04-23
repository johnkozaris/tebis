# GitHub Copilot CLI hook for tebis, Windows edition.
#
# PowerShell sibling of contrib/copilot/copilot-hook.sh. Same event
# dispatch + wire format; transport is a Named Pipe instead of a Unix
# socket.
#
# Events handled:
#
#   userPromptSubmitted → inject summarize-at-end instruction
#   agentStop           → forward tail of last assistant message
#   subagentStop        → forward subagent tail tagged by agent name
#   notification        → forward the message text
#
# Safety: same as .sh — never blocks Copilot on delivery failure; never
# echoes transcript content except in the hookSpecificOutput contract;
# reads the JSONL transcript file, not the terminal.

$ErrorActionPreference = 'Continue'

$MaxChars = 1500

$WrapInstruction = @'
[tebis] When replying, conclude your final message with a concise summary (max 1500 characters) describing what you did and any decisions the user needs to make. If the reply is short or trivial, skip the summary and answer directly. This summary is forwarded to a phone notification.
'@

function Resolve-PipeName {
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

function Tail-Trim {
    param([string]$Text)
    if (-not $Text) { return $null }
    if ($Text.Length -le $MaxChars) { return $Text }
    return '…' + $Text.Substring($Text.Length - $MaxChars)
}

# Copilot transcript entries have slightly different shapes across CLI
# versions: `role: "assistant"` + `content: "..."` is the common case,
# but older versions use `type: "assistant"` and nested
# `message.content` arrays. Match all of them.
function Extract-Text {
    param($Block)
    if ($null -eq $Block) { return '' }
    if ($Block -is [string]) { return $Block }
    if ($Block -is [array]) {
        $parts = @()
        foreach ($item in $Block) {
            if ($item.type -eq 'text' -and $item.text) { $parts += $item.text }
        }
        return ($parts -join "`n`n")
    }
    if ($Block.content) { return (Extract-Text $Block.content) }
    return ''
}

function Tail-Of-Last-Assistant {
    param([string]$TranscriptPath)
    if (-not (Test-Path $TranscriptPath)) { return $null }

    $lastText = $null
    foreach ($line in (Get-Content -LiteralPath $TranscriptPath -ErrorAction SilentlyContinue)) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        try {
            $entry = $line | ConvertFrom-Json -ErrorAction Stop
        } catch { continue }
        $isAssistant = ($entry.role -eq 'assistant') -or ($entry.type -eq 'assistant')
        if (-not $isAssistant) { continue }
        $source = $entry.content
        if (-not $source) { $source = $entry.message.content }
        if (-not $source) { continue }
        $text = Extract-Text $source
        if ($text) { $lastText = $text }
    }
    return (Tail-Trim $lastText)
}

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

# Prefer hook_event_name (v1.0.21+); fall back to eventName.
$event = if ($in.hook_event_name) {
    $in.hook_event_name
} elseif ($in.eventName) {
    $in.eventName
} else {
    ''
}
$event = $event.ToLowerInvariant()

$transcript = if ($in.transcriptPath)  { $in.transcriptPath }
              elseif ($in.transcript_path) { $in.transcript_path }
              else { '' }
$cwd     = if ($in.cwd) { $in.cwd } else { '' }
$session = if ($in.sessionId)  { $in.sessionId }
           elseif ($in.session_id) { $in.session_id }
           else { '' }

switch ($event) {

    { $_ -in 'userpromptsubmitted', 'userpromptsubmit' } {
        $out = [pscustomobject]@{
            hookSpecificOutput = [pscustomobject]@{
                hookEventName     = 'userPromptSubmitted'
                additionalContext = $WrapInstruction
            }
        } | ConvertTo-Json -Compress -Depth 4
        Write-Output $out
        exit 0
    }

    { $_ -in 'agentstop', 'stop' } {
        if (-not $transcript -or -not (Test-Path $transcript)) { exit 0 }
        $summary = Tail-Of-Last-Assistant $transcript
        if ($summary) {
            Forward -Text $summary -Kind 'stop' -Cwd $cwd -Session $session
        }
    }

    'subagentstop' {
        $agentName = if ($in.agentName) { $in.agentName }
                     elseif ($in.agent_name) { $in.agent_name }
                     else { 'subagent' }
        if (-not $transcript -or -not (Test-Path $transcript)) { exit 0 }
        $summary = Tail-Of-Last-Assistant $transcript
        if ($summary) {
            Forward -Text $summary -Kind 'subagent_stop' -Cwd $cwd -Session $agentName
        }
    }

    'notification' {
        $msg = if ($in.message) { $in.message } else { '' }
        $kind = if ($in.notificationType) { $in.notificationType }
                elseif ($in.notification_type) { $in.notification_type }
                else { 'notification' }
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
