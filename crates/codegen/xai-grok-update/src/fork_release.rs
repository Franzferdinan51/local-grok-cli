//! Install grok-local binaries from Franzferdinan51/local-grok-cli GitHub Releases.
//! Official `grok` artifacts from x.ai are never downloaded here.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::auto_update::detect_platform;
use crate::local_sync;

const RELEASE_REPO: &str = "Franzferdinan51/local-grok-cli";
const USER_AGENT: &str = "grok-local";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkReleaseStatus {
    pub grok_local: String,
    pub grok_build: String,
    pub latest_grok_local: Option<String>,
    pub latest_release_url: Option<String>,
    pub update_available: bool,
    pub asset: Option<String>,
    pub release_notes: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    #[serde(default)]
    size: u64,
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn verify_asset_digest(bytes: &[u8], digest: Option<&String>) -> Result<()> {
    let Some(digest) = digest else {
        bail!("release asset has no SHA-256 digest; refusing to install")
    };
    let Some(expected) = digest.strip_prefix("sha256:") else {
        bail!("release asset digest is not SHA-256: {digest}")
    };
    let actual = hex_encode(&sha256(bytes));
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!("release asset SHA-256 mismatch: expected {expected}, got {actual}")
    }
    Ok(())
}

fn artifact_name(os: &str, arch: &str) -> Result<String> {
    Ok(match (os, arch) {
        ("linux", "x86_64") => "grok-local-linux-x86_64".into(),
        ("linux", "aarch64") => "grok-local-linux-aarch64".into(),
        ("macos", "aarch64") => "grok-local-macos-aarch64".into(),
        ("macos", "x86_64") => "grok-local-macos-x86_64".into(),
        ("windows", "x86_64") => "grok-local-windows-x86_64.exe".into(),
        (os, arch) => bail!("no grok-local GitHub asset for {os}-{arch}"),
    })
}

fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

fn release_is_newer(current: &str, latest: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(latest),
    ) {
        (Ok(current), Ok(latest)) => latest > current,
        _ => current != latest,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticUpdateSource {
    ForkRelease,
    UpstreamInstaller,
}

pub fn automatic_update_source(executable_name: &str) -> AutomaticUpdateSource {
    if executable_name == "grok-local" || executable_name == "grok-local.exe" {
        AutomaticUpdateSource::ForkRelease
    } else {
        AutomaticUpdateSource::UpstreamInstaller
    }
}

/// Update the active fork binary without routing it through xAI's installer.
pub async fn run_fork_auto_update_if_available() -> Result<bool> {
    let status = check_fork_release().await;
    if !status.update_available {
        return Ok(false);
    }
    install_fork_release(None, false).await?;
    Ok(true)
}

fn api_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()?)
}

async fn fetch_release(tag: Option<&str>) -> Result<GhRelease> {
    let client = api_client()?;
    let url = match tag {
        Some(t) => {
            let t = if t.starts_with('v') {
                t.to_string()
            } else {
                format!("v{t}")
            };
            format!("https://api.github.com/repos/{RELEASE_REPO}/releases/tags/{t}")
        }
        None => format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest"),
    };
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GitHub releases HTTP {} from {url}", resp.status());
    }
    Ok(resp.json().await?)
}

pub async fn check_fork_release() -> ForkReleaseStatus {
    let grok_local = xai_grok_version::LOCAL_VERSION.to_string();
    let grok_build = xai_grok_version::VERSION.to_string();
    match fetch_release(None).await {
        Ok(rel) => {
            let latest = strip_v(&rel.tag_name).to_string();
            let (os, arch) = detect_platform().unwrap_or(("linux", "x86_64"));
            let want = artifact_name(os, arch).ok();
            ForkReleaseStatus {
                update_available: release_is_newer(&grok_local, &latest),
                latest_grok_local: Some(latest),
                latest_release_url: Some(rel.html_url),
                asset: want,
                release_notes: rel.body,
                grok_local,
                grok_build,
                error: None,
            }
        }
        Err(e) => ForkReleaseStatus {
            grok_local,
            grok_build,
            latest_grok_local: None,
            latest_release_url: None,
            update_available: false,
            asset: None,
            release_notes: None,
            error: Some(e.to_string()),
        },
    }
}

pub fn print_fork_release_status(status: &ForkReleaseStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(status)?);
        return Ok(());
    }
    println!(
        "Grok Local {}  (grok-build {})",
        status.grok_local, status.grok_build
    );
    if let Some(error) = status.error.as_deref() {
        println!("Release check failed: {error}");
        return Ok(());
    }
    if let Some(notes) = status.release_notes.as_deref()
        && !notes.trim().is_empty()
    {
        println!("Release notes:\n{notes}");
    }
    match status.latest_grok_local.as_deref() {
        Some(latest) if status.update_available => {
            println!(
                "A new grok-local GitHub release is available: {} -> {latest}",
                status.grok_local
            );
            if let Some(url) = status.latest_release_url.as_deref() {
                println!("{url}");
            }
            println!("Run `grok-local update` to install it.");
            println!(
                "Run `grok-local update --upstream` to overlay-merge xai-org/grok-build instead."
            );
        }
        Some(latest) => {
            println!("Already on latest grok-local GitHub release ({latest}).");
            println!(
                "Use `grok-local update --upstream` to overlay-merge the latest grok-build source."
            );
        }
        None => {}
    }
    Ok(())
}

pub async fn rollback_fork_release() -> Result<()> {
    let dest = install_destination().context("cannot find grok-local install path")?;
    let _lock = InstallLock::acquire(&dest)?;
    let previous = dest.with_extension("previous");
    if !previous.is_file() {
        bail!("no previous grok-local backup found at {}", previous.display());
    }
    let failed = dest.with_extension(format!("failed-{}", std::process::id()));
    if dest.exists() {
        tokio::fs::rename(&dest, &failed).await?;
    }
    if let Err(error) = tokio::fs::rename(&previous, &dest).await {
        if failed.exists() {
            let _ = tokio::fs::rename(&failed, &dest).await;
        }
        return Err(error).with_context(|| format!("restore {}", previous.display()));
    }
    eprintln!("  Rolled back grok-local using {}", previous.display());
    eprintln!("  Failed version retained at {}", failed.display());
    Ok(())
}

pub async fn install_fork_release(target: Option<&str>, force: bool) -> Result<()> {
    let current = xai_grok_version::LOCAL_VERSION;
    let release = fetch_release(target).await?;
    let latest = strip_v(&release.tag_name).to_string();
    if !force && target.is_none() && latest == current {
        eprintln!("Already on grok-local {current}.");
        eprintln!("Use --force-reinstall to download again, or --upstream to merge grok-build.");
        return Ok(());
    }

    let (os, arch) = detect_platform()?;
    let want = artifact_name(os, arch)?;
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == want)
        .with_context(|| {
            format!(
                "release {} has no asset {want} (https://github.com/{RELEASE_REPO}/releases)",
                release.tag_name
            )
        })?;

    let dest = install_destination().context("cannot find grok-local install path")?;
    let _lock = InstallLock::acquire(&dest)?;
    let parent = dest.parent().unwrap_or(std::path::Path::new("."));
    let available = fs2::available_space(parent)
        .with_context(|| format!("check free space in {}", parent.display()))?;
    if available < asset_size_hint(asset) {
        bail!(
            "not enough free space for update in {} ({} bytes available)",
            parent.display(),
            available
        );
    }

    eprintln!("  Downloading {} from {} ...", want, release.tag_name);
    let client = api_client()?;
    let bytes = client
        .get(&asset.browser_download_url)
        .header("Accept", "application/octet-stream")
        .timeout(std::time::Duration::from_secs(20 * 60))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    verify_asset_digest(&bytes, asset.digest.as_ref())?;

    let tmp = dest.with_extension(format!("new-{}", std::process::id()));
    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&tmp).await?.permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&tmp, perms).await?;
    }
    let output = tokio::process::Command::new(&tmp)
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("validate {}", tmp.display()))?;
    let version_output = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !version_output.contains(&format!("grok-local {latest}")) {
        let _ = tokio::fs::remove_file(&tmp).await;
        bail!("downloaded release failed its version health check");
    }
    let backup = dest.with_extension("previous");
    if dest.exists() {
        tokio::fs::copy(&dest, &backup)
            .await
            .with_context(|| format!("backup {}", dest.display()))?;
    }
    #[cfg(windows)]
    if dest.exists() {
        tokio::fs::remove_file(&dest).await?;
    }
    if let Err(error) = tokio::fs::rename(&tmp, &dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error).with_context(|| format!("install {}", dest.display()));
    }
    eprintln!("  Installed grok-local {} -> {}", latest, dest.display());
    eprintln!("  Previous binary saved at {}", backup.display());
    eprintln!("  Restart grok-local to use it.");
    Ok(())
}

fn asset_size_hint(asset: &GhAsset) -> u64 {
    asset.size.saturating_mul(2).saturating_add(1024 * 1024)
}

struct InstallLock {
    path: std::path::PathBuf,
}

impl InstallLock {
    fn acquire(dest: &std::path::Path) -> Result<Self> {
        let path = dest.with_extension("update.lock");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("another update is already running ({})", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn install_destination() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let name = exe.file_name()?.to_string_lossy();
        if name == "grok-local" || name == "grok-local.exe" {
            return Some(exe);
        }
    }
    local_sync::find_source_tree()
        .map(|src| {
            let p = src.join("target/release/grok-local");
            if cfg!(windows) {
                src.join("target/release/grok-local.exe")
            } else {
                p
            }
        })
        .or_else(|| {
            let home = xai_dirs::home_dir()?;
            Some(home.join(".local/bin/grok-local"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_names_match_release_workflow() {
        let cases = [
            (("linux", "x86_64"), "grok-local-linux-x86_64"),
            (("linux", "aarch64"), "grok-local-linux-aarch64"),
            (("macos", "aarch64"), "grok-local-macos-aarch64"),
            (("macos", "x86_64"), "grok-local-macos-x86_64"),
            (("windows", "x86_64"), "grok-local-windows-x86_64.exe"),
        ];
        for ((os, arch), expected) in cases {
            assert_eq!(artifact_name(os, arch).unwrap(), expected);
        }
    }

    #[test]
    fn unsupported_platforms_fail_without_guessing_an_asset() {
        let error = artifact_name("linux", "armv7").unwrap_err().to_string();
        assert!(error.contains("no grok-local GitHub asset for linux-armv7"));
    }

    #[test]
    fn release_tags_accept_v_prefix_or_plain_versions() {
        assert_eq!(strip_v("v0.4.1"), "0.4.1");
        assert_eq!(strip_v("0.4.1"), "0.4.1");
    }

    #[test]
    fn release_check_never_offers_a_downgrade() {
        assert!(!release_is_newer("0.4.2", "0.4.0"));
        assert!(!release_is_newer("0.4.2", "0.4.2"));
        assert!(release_is_newer("0.4.1", "0.4.2"));
    }

    #[test]
    fn local_binary_uses_fork_releases_for_automatic_updates() {
        assert_eq!(
            automatic_update_source("grok-local"),
            AutomaticUpdateSource::ForkRelease
        );
        assert_eq!(
            automatic_update_source("grok-local.exe"),
            AutomaticUpdateSource::ForkRelease
        );
        assert_eq!(
            automatic_update_source("grok"),
            AutomaticUpdateSource::UpstreamInstaller
        );
    }

    #[test]
    fn release_asset_digest_must_match_downloaded_bytes() {
        let bytes = b"safe release";
        let digest = format!("sha256:{}", hex_encode(&sha256(bytes)));
        assert!(verify_asset_digest(bytes, Some(&digest)).is_ok());
        assert!(verify_asset_digest(bytes, Some(&"sha256:00".to_string())).is_err());
        assert!(verify_asset_digest(bytes, None).is_err());
    }
}
