//! Self-upgrade via GitHub Releases.
//!
//! `tebis upgrade` queries the latest release, compares against the
//! current `CARGO_PKG_VERSION`, downloads the appropriate per-target
//! binary, verifies its SHA-256, and atomic-replaces the running
//! executable. The replace is unconditional on the path tebis was
//! invoked from — `env::current_exe()` — which is the user-friendly
//! contract: "wherever I'm running from, upgrade that".
//!
//! Release asset layout (produced by `.github/workflows/release.yml`):
//!
//!   tebis-<target>            # the binary (or `.exe` on Windows)
//!   tebis-<target>.sha256     # hex digest of the binary above
//!
//! Target triples match what `cargo build --target` accepts and what
//! [`current_target`] returns at runtime.

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use console::style;
use ring::digest::{Context as Sha256Ctx, SHA256};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

const RELEASES_URL: &str =
    "https://api.github.com/repos/johnkozaris/tebis/releases/latest";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB hard cap

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
    #[serde(default)]
    html_url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// CLI entry point. Set `restart` to true to also stop+start the
/// service after a successful upgrade.
pub async fn run(restart: bool) -> Result<()> {
    println!();
    println!(
        "{}  Checking for tebis updates…",
        style("▶").cyan().bold()
    );

    let target = current_target().ok_or_else(|| {
        anyhow!(
            "no published binary for this platform ({}/{})",
            env::consts::OS,
            env::consts::ARCH
        )
    })?;

    let client = build_client()?;
    let release = fetch_latest(&client).await?;

    let current = env!("CARGO_PKG_VERSION");
    let latest = release.tag_name.trim_start_matches('v');
    if latest == current {
        println!(
            "{}  Already on the latest version (v{current}).",
            style("✓").green().bold()
        );
        return Ok(());
    }
    println!(
        "    current: v{current}    latest: v{latest}    ({})",
        style(&release.html_url).dim()
    );

    let bin_asset = release
        .assets
        .iter()
        .find(|a| a.name == format!("tebis-{target}") || a.name == format!("tebis-{target}.exe"))
        .ok_or_else(|| {
            anyhow!(
                "no asset matching target `{target}` in release v{latest} \
                 (release published from a different build matrix?)"
            )
        })?;
    let sha_asset = release
        .assets
        .iter()
        .find(|a| a.name == format!("{}.sha256", bin_asset.name))
        .ok_or_else(|| {
            anyhow!(
                "no `{}.sha256` checksum sidecar in release v{latest}",
                bin_asset.name
            )
        })?;

    let cancel = CancellationToken::new();
    let expected_sha = download_text(&client, &sha_asset.browser_download_url, &cancel)
        .await?
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow!("empty .sha256 file at {}", sha_asset.browser_download_url))?;

    // Stage download next to the target so the atomic rename stays on
    // the same filesystem/volume.
    let current_exe = env::current_exe().context("locating current executable")?;
    let stage_dir = current_exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(env::temp_dir);
    fs::create_dir_all(&stage_dir)
        .with_context(|| format!("creating stage dir {}", stage_dir.display()))?;
    let tmp = stage_dir.join(format!("tebis-upgrade-{}.tmp", std::process::id()));

    println!(
        "{}  Downloading {} ({} MiB cap)…",
        style("▶").cyan().bold(),
        bin_asset.name,
        MAX_BINARY_BYTES / (1024 * 1024)
    );
    let got_sha = download_to_file(&client, &bin_asset.browser_download_url, &tmp, &cancel)
        .await
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;

    if got_sha != expected_sha {
        let _ = fs::remove_file(&tmp);
        bail!(
            "checksum mismatch for {}\n  expected: {expected_sha}\n  got:      {got_sha}",
            bin_asset.name
        );
    }
    println!("{}  Checksum verified.", style("✓").green().bold());

    println!(
        "{}  Replacing {} …",
        style("▶").cyan().bold(),
        current_exe.display()
    );
    crate::platform::binary_replace::atomic_replace(&tmp, &current_exe)?;
    println!("{}  Binary updated to v{latest}.", style("✓").green().bold());

    if restart {
        println!();
        println!("{}  Restarting service…", style("▶").cyan().bold());
        // Re-exec into the new binary's restart subcommand. `service::restart`
        // would invoke launchctl/systemctl/schtasks against the still-loaded
        // OLD image; spawning a fresh `tebis` process executes the NEW one.
        let status = std::process::Command::new(&current_exe)
            .arg("restart")
            .status()
            .context("spawning `tebis restart`")?;
        if !status.success() {
            bail!("`tebis restart` exited with {status}");
        }
    } else {
        println!();
        println!(
            "    {}",
            style("Run `tebis restart` to load the new binary into the service.").dim()
        );
    }
    println!();
    Ok(())
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_nodelay(true)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(concat!("tebis-upgrade/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

async fn fetch_latest(client: &reqwest::Client) -> Result<Release> {
    let res = client
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("GET releases/latest")?;
    let status = res.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "no published releases yet — visit https://github.com/johnkozaris/tebis/releases"
        );
    }
    if !status.is_success() {
        bail!(
            "GitHub API returned {status} for {RELEASES_URL} \
             (rate-limited? try again in a few minutes)"
        );
    }
    res.json::<Release>()
        .await
        .context("parsing releases/latest JSON")
}

async fn download_text(
    client: &reqwest::Client,
    url: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    let send = client.get(url).send();
    let res = tokio::select! {
        biased;
        () = cancel.cancelled() => bail!("cancelled"),
        r = tokio::time::timeout(DOWNLOAD_TIMEOUT, send) => r
            .map_err(|_| anyhow!("download timeout"))?
            .context("GET text asset")?,
    };
    if !res.status().is_success() {
        bail!("HTTP {} downloading {url}", res.status());
    }
    res.text().await.context("reading text body")
}

/// Stream-download `url` to `dst`, returning the SHA-256 hex of the
/// bytes written. Caller is responsible for removing `dst` on error.
async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    dst: &Path,
    cancel: &CancellationToken,
) -> Result<String> {
    let send = client.get(url).send();
    let mut res = tokio::select! {
        biased;
        () = cancel.cancelled() => bail!("cancelled"),
        r = tokio::time::timeout(DOWNLOAD_TIMEOUT, send) => r
            .map_err(|_| anyhow!("download timeout"))?
            .context("GET binary asset")?,
    };
    if !res.status().is_success() {
        bail!("HTTP {} downloading {url}", res.status());
    }
    if let Some(len) = res.content_length()
        && len > MAX_BINARY_BYTES
    {
        bail!(
            "advertised size {len} exceeds {MAX_BINARY_BYTES} cap — refusing to download"
        );
    }
    let mut file = fs::File::create(dst)
        .with_context(|| format!("creating {}", dst.display()))?;
    let mut hasher = Sha256Ctx::new(&SHA256);
    let mut written: u64 = 0;
    loop {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => bail!("cancelled"),
            c = res.chunk() => c.context("reading body chunk")?,
        };
        let Some(c) = chunk else { break };
        if written + c.len() as u64 > MAX_BINARY_BYTES {
            bail!("body exceeded {MAX_BINARY_BYTES}-byte cap mid-stream");
        }
        file.write_all(&c)
            .with_context(|| format!("writing {}", dst.display()))?;
        hasher.update(&c);
        written += c.len() as u64;
    }
    file.flush().context("flushing tmp file")?;
    file.sync_all().context("fsync tmp file")?;
    drop(file);
    Ok(hex_encode(hasher.finish().as_ref()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(LUT[(b >> 4) as usize] as char);
        out.push(LUT[(b & 0x0f) as usize] as char);
    }
    out
}

/// Returns the Rust target triple matching the current binary build,
/// or `None` if the build matrix doesn't ship for this OS/arch.
///
/// Matches the asset names in `.github/workflows/release.yml` —
/// adding a target there requires adding it here too.
fn current_target() -> Option<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encode_known_vectors() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn current_target_resolves() {
        // The host running these tests must be one of our published
        // targets, otherwise we have a release-matrix gap.
        assert!(
            current_target().is_some(),
            "host ({}/{}) not in release matrix",
            env::consts::OS,
            env::consts::ARCH
        );
    }
}
