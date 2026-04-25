# GitHub Copilot CLI hook for tebis, Windows edition.
#
# PowerShell sibling of contrib/copilot/copilot-hook.sh. Same event
# dispatch + wire format; transport is a Named Pipe instead of a Unix
# socket.
#
# Events handled:
#
#   userPromptSubmitted → inject summarize-at-end instruction
#   notification        → forward completion / permission / idle messages
#
# Safety: same as .sh — never blocks Copilot on delivery failure; never
# echoes transcript content except in the hookSpecificOutput contract.

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
            [System.Security.Principal.TokenImpersonationLevel]::Identification
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
