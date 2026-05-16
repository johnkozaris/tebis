#Requires -Version 5.1
<#
.SYNOPSIS
    One-shot installer for tebis on Windows.

.DESCRIPTION
    Downloads the latest tebis binary from GitHub Releases, verifies
    its SHA-256, installs it to %LOCALAPPDATA%\Programs\tebis\, and
    appends that directory to the user PATH.

.PARAMETER Version
    Release tag to install (e.g. "v0.1.0"). Defaults to "latest".

.PARAMETER InstallDir
    Installation directory. Defaults to %LOCALAPPDATA%\Programs\tebis.

.EXAMPLE
    iwr -useb https://github.com/johnkozaris/tebis/releases/latest/download/install.ps1 | iex

.EXAMPLE
    # Pin to a specific tag:
    & ([scriptblock]::Create((iwr -useb https://github.com/johnkozaris/tebis/releases/latest/download/install.ps1).Content)) -Version v0.1.0
#>

[CmdletBinding()]
param(
    [string] $Version = $(if ($env:TEBIS_VERSION) { $env:TEBIS_VERSION } else { 'latest' }),
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\tebis')
)

$ErrorActionPreference = 'Stop'
# Force TLS 1.2 on PS 5.1 — older defaults break the GitHub Releases
# CDN handshake on Windows Server boxes. PS 7+ already defaults to 1.2/1.3.
[Net.ServicePointManager]::SecurityProtocol = `
    [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13

$Repo = 'johnkozaris/tebis'
$BinName = 'tebis.exe'

# ── Pretty output ────────────────────────────────────────────────────
function Write-Step($msg)  { Write-Host ('▶  ' + $msg) -ForegroundColor Cyan }
function Write-Ok($msg)    { Write-Host ('✓  ' + $msg) -ForegroundColor Green }
function Write-Warn2($msg) { Write-Host ('⚠  ' + $msg) -ForegroundColor Yellow }
function Die($msg)         { Write-Host ('✗  ' + $msg) -ForegroundColor Red; exit 1 }

# ── Platform detection ───────────────────────────────────────────────
# We only ship x86_64 Windows for now. ARM64 Windows is on the
# roadmap but isn't in the release matrix yet.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64') {
    Die "unsupported Windows arch: $arch (only x86_64/AMD64 is published)"
}
$target = 'x86_64-pc-windows-msvc'
$asset  = "tebis-${target}.exe"

# ── Resolve URL ──────────────────────────────────────────────────────
if ($Version -eq 'latest') {
    $baseUrl = "https://github.com/${Repo}/releases/latest/download"
} else {
    $baseUrl = "https://github.com/${Repo}/releases/download/${Version}"
}
$binUrl = "$baseUrl/$asset"
$shaUrl = "$baseUrl/$asset.sha256"

Write-Step 'Installing tebis'
Write-Host ('    target:      ' + $target) -ForegroundColor DarkGray
Write-Host ('    tag:         ' + $Version) -ForegroundColor DarkGray
Write-Host ('    install dir: ' + $InstallDir) -ForegroundColor DarkGray
Write-Host ('    url:         ' + $binUrl) -ForegroundColor DarkGray

# ── Download to scratch ──────────────────────────────────────────────
$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("tebis-install-" + [Guid]::NewGuid().ToString('N'))
[void] (New-Item -ItemType Directory -Path $tmpDir -Force)
$tmpBin = Join-Path $tmpDir $asset
$tmpSha = Join-Path $tmpDir "$asset.sha256"

try {
    Write-Step 'Downloading binary…'
    Invoke-WebRequest -Uri $binUrl -OutFile $tmpBin -UseBasicParsing

    Write-Step 'Downloading checksum…'
    Invoke-WebRequest -Uri $shaUrl -OutFile $tmpSha -UseBasicParsing

    # ── Verify checksum ──────────────────────────────────────────────
    # Sidecar format mirrors POSIX `shasum -a 256` output:
    # "<hex>  <filename>" — we take the first token only.
    $expected = ((Get-Content -Path $tmpSha -TotalCount 1) -split '\s+')[0].ToLower()
    $got      = (Get-FileHash -Path $tmpBin -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $got) {
        Die "checksum mismatch`n    expected: $expected`n    got:      $got"
    }
    Write-Ok 'Checksum verified.'

    # ── Install ──────────────────────────────────────────────────────
    if (-not (Test-Path $InstallDir)) {
        [void] (New-Item -ItemType Directory -Path $InstallDir -Force)
    }
    $installPath = Join-Path $InstallDir $BinName

    # If a previous tebis.exe is currently running (foreground or
    # service-managed), `Move-Item` over it will fail with sharing
    # violation. Surface a clear error in that case rather than the
    # cryptic .NET "being used by another process" stack.
    if (Test-Path $installPath) {
        try {
            Remove-Item -Path $installPath -Force -ErrorAction Stop
        } catch {
            Die @"
$installPath is locked by another process.
Stop the running tebis first, then re-run the installer:
    tebis stop          # if installed as a service
    Get-Process tebis -ErrorAction SilentlyContinue | Stop-Process -Force
"@
        }
    }
    Move-Item -Path $tmpBin -Destination $installPath -Force
    Write-Ok ("Installed tebis to " + $installPath)

    # ── PATH (user scope) ────────────────────────────────────────────
    # Append to the User PATH idempotently. Surgical edit via the
    # .NET API rather than `setx` — `setx` truncates at 1024 chars
    # which would corrupt PATHs on machines with many user-scope
    # entries.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $pathEntries = if ($userPath) { $userPath -split ';' } else { @() }
    $alreadyOnPath = $pathEntries | Where-Object { $_ -ieq $InstallDir }
    if (-not $alreadyOnPath) {
        $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Ok "Appended $InstallDir to user PATH"
        Write-Warn2 'Open a new terminal for the PATH change to take effect.'
    }

    # ── Next steps ───────────────────────────────────────────────────
    Write-Host ''
    Write-Host 'Next steps' -ForegroundColor White
    Write-Host '    tebis setup              run the interactive config wizard' -ForegroundColor DarkGray
    Write-Host '    tebis install            install as a Task Scheduler job'  -ForegroundColor DarkGray
    Write-Host '    tebis --help             see all commands'                 -ForegroundColor DarkGray
    Write-Host ''
}
finally {
    Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
