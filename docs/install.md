# Install, upgrade, uninstall

`tebis` ships a self-contained binary for macOS, Linux, and Windows. Two
installer scripts wrap the GitHub Releases artifacts so the common
case is a single command.

## Supported targets

| OS       | Arch          | Asset name                              |
|----------|---------------|-----------------------------------------|
| Linux    | x86_64        | `tebis-x86_64-unknown-linux-gnu`        |
| Linux    | aarch64       | `tebis-aarch64-unknown-linux-gnu`       |
| macOS    | Apple Silicon | `tebis-aarch64-apple-darwin`            |
| macOS    | Intel         | `tebis-x86_64-apple-darwin`             |
| Windows  | x86_64        | `tebis-x86_64-pc-windows-msvc.exe`      |

Each release also ships a matching `.sha256` sidecar containing one
line in `shasum -a 256` format. The installer scripts and `tebis
upgrade` both verify against it.

## One-shot install

### macOS / Linux

```sh
curl -fsSL https://github.com/johnkozaris/tebis/releases/latest/download/install.sh | sh
```

The script:

1. Detects your OS + arch, picks the right asset.
2. Downloads the binary and its `.sha256` to a scratch directory.
3. Verifies SHA-256 (`shasum -a 256` or `sha256sum`, whichever is on
   PATH).
4. Moves the binary to `~/.local/bin/tebis` and `chmod 0755`.
5. On macOS, strips `com.apple.quarantine` so Gatekeeper does not
   prompt on first run.
6. Prints the `export PATH=...` line if `~/.local/bin` is not on your
   PATH (you paste it into `~/.zshrc` or equivalent).

Options:

```sh
# Pin a specific version
curl -fsSL .../install.sh | sh -s -- --version v0.2.0

# Override install dir
curl -fsSL .../install.sh | TEBIS_INSTALL_DIR=/opt/tebis/bin sh
```

### Windows (PowerShell 5.1+)

```powershell
irm https://github.com/johnkozaris/tebis/releases/latest/download/install.ps1 | iex
```

The script:

1. Forces TLS 1.2/1.3 (PS 5.1 default does not negotiate cleanly with
   the GitHub CDN on Windows Server).
2. Downloads the binary + sidecar.
3. Verifies SHA-256 via `Get-FileHash`.
4. Moves `tebis.exe` to `%LOCALAPPDATA%\Programs\tebis\`.
5. Appends that directory to the **User** PATH (idempotent) via
   `[Environment]::SetEnvironmentVariable('Path', ..., 'User')`. We
   do not use `setx` — it truncates at 1024 chars and would silently
   corrupt a long PATH.

Open a new terminal after the install so PATH takes effect.

Options:

```powershell
# Pin a version
& ([scriptblock]::Create((iwr -useb https://.../install.ps1).Content)) -Version v0.2.0

# Override install dir
& ([scriptblock]::Create((iwr -useb https://.../install.ps1).Content)) -InstallDir C:\Tools\tebis
```

### From source

```sh
git clone https://github.com/johnkozaris/tebis.git
cd tebis
cargo build --release
./target/release/tebis setup
```

You will need a recent Rust (MSRV 1.95), CMake (for `whisper-rs`),
and a C++ toolchain (Xcode CLT on macOS, build-essential on Linux,
Visual Studio Build Tools on Windows).

## First-time configuration

After the binary is on PATH, run:

```sh
tebis setup
```

The wizard collects the bot token, your Telegram user id, an
allowlist, and (optionally) a default agent + hook deps. See
[setup.md](setup.md) for the full walkthrough.

## Upgrade

```sh
tebis upgrade            # check, download, verify, replace
tebis upgrade --restart  # also restart the service after upgrade
```

What happens under the hood:

- The current version is read from `tebis --version`. Latest is fetched
  from the GitHub Releases API (no auth, no token).
- The matching asset for the running host is downloaded into the same
  directory as `current_exe()` so the final rename stays on one
  filesystem.
- SHA-256 is streamed against the sidecar during download (64 MiB
  hard cap).
- On Unix the new binary atomically `rename(2)`s over the running
  one. The loader holds the old inode, so the running daemon keeps
  serving until the next restart picks up the new image.
- On Windows the running `.exe` is renamed to `tebis.exe.old`
  (allowed while running) and the new image is moved into place.
  The `.old` is best-effort unlinked on the next upgrade.

`tebis upgrade --restart` re-execs the freshly-installed binary's
`restart` subcommand. Without `--restart`, the new image only loads
on the next manual restart.

## Uninstall

```sh
tebis uninstall          # remove service only; binary + config remain
tebis uninstall --purge  # also remove binary, config, data, hooks
```

`tebis uninstall` (no flag) stops the daemon and removes the service
unit (`launchctl unload`, `systemctl --user disable`, or `schtasks /Delete`).
That's it — the binary, env file, models cache, and project hooks are
left in place so a re-install is a quick `tebis install` away.

`--purge` is the zero-trace path:

- Iterates the manifest at `<data_dir>/installed.json` and uninstalls
  every project's hook entries (Claude `settings.local.json` block,
  Copilot `tebis.json` sentinel).
- Removes config dir (`~/.config/tebis/` or `%APPDATA%\tebis\`).
- Removes data dir (`$XDG_DATA_HOME/tebis/` or
  `%LOCALAPPDATA%\tebis\`) unless hook cleanup reported failures —
  then the manifest is preserved so a retry can resume.
- Removes the service-installed binary
  (`~/.local/bin/tebis` on Unix; `%LOCALAPPDATA%\Programs\tebis\` on
  Windows via a 30-second self-delete trampoline).
- Windows: surgically removes the install dir from User PATH (the
  entry `install.ps1` appended). Unix: prints the `export PATH=…`
  line you originally added so you can remove it from your rc file —
  we never edit dotfiles.

What `--purge` does NOT touch:

- `tmux`, `jq`, `nc`, `psmux` — those may be in use by other tools.
- Your project repositories themselves (only our hook entries are
  removed from `.claude/settings.local.json` / `.github/hooks/tebis.json`).
- Running multiplexer sessions — they keep going.
- Custom install locations. If you installed the binary somewhere
  other than the service's default (`~/.local/bin/tebis` /
  `%LOCALAPPDATA%\Programs\tebis\tebis.exe`), `--purge` won't find
  it. Remove it manually.

## Doctor

```sh
tebis doctor
```

Reports binary version, config presence, multiplexer status, hook
deps, installed hooks (per project / per agent), service state,
lockfile / daemon status, and Telegram reachability. Same rows on
every OS; the per-OS specifics live inside each check.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `install.sh` exits with "checksum mismatch" | re-run; you may have hit a CDN cache mid-publish |
| macOS first run still shows Gatekeeper prompt | `xattr -d com.apple.quarantine ~/.local/bin/tebis` |
| Windows SmartScreen blocks the binary | click **More info → Run anyway**; binaries aren't code-signed in v0.x. install.ps1 already calls `Unblock-File` to clear the MOTW. |
| `tebis upgrade` says "no compatible asset" | your host triple is not in the release matrix; build from source |
| `tebis upgrade` fails to replace on Windows | another `tebis.exe` is running; stop it and retry |
| `tebis upgrade` fails with "permission denied" | binary was installed system-wide; reinstall under `~/.local/bin` or run upgrade with the same privileges as install |
| You installed with `--dir` / `TEBIS_INSTALL_DIR`, then `tebis install` | the service hard-codes `~/.local/bin/tebis` / `%LOCALAPPDATA%\Programs\tebis\`. You'll end up with two copies; remove the custom-path one manually before / after `--purge`. |
| Post-uninstall on Unix, `export PATH=…` line still in your `.zshrc` | nothing to clean automatically — we never edit dotfiles. Remove the line yourself. |
| Post-uninstall on Windows, `%LOCALAPPDATA%\Programs\tebis` still in PATH | `--purge` removes it; plain `uninstall` does not. Re-run with `--purge` if you want the PATH entry gone. |

## Behind a proxy / offline install

Download the asset + sidecar manually from the Releases page and run
the verification yourself:

```sh
shasum -a 256 -c tebis-<target>.sha256
chmod +x tebis-<target>
mv tebis-<target> ~/.local/bin/tebis
```

On Windows:

```powershell
$h = (Get-FileHash .\tebis-<target>.exe -Algorithm SHA256).Hash.ToLower()
$expected = ((Get-Content .\tebis-<target>.exe.sha256) -split '\s+')[0].ToLower()
if ($h -ne $expected) { throw "checksum mismatch" }
Move-Item .\tebis-<target>.exe "$env:LOCALAPPDATA\Programs\tebis\tebis.exe"
```
