//! Install grok-local binaries from Franzferdinan51/local-grok-cli GitHub Releases.
//! Official `grok` artifacts from x.ai are never downloaded here.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::Serialize;

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
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
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

    let dest = install_destination().context("cannot find grok-local install path")?;
    let tmp = dest.with_extension("new");
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
    tokio::fs::rename(&tmp, &dest)
        .await
        .with_context(|| format!("install {}", dest.display()))?;
    eprintln!("  Installed grok-local {} -> {}", latest, dest.display());
    eprintln!("  Restart grok-local to use it.");
    Ok(())
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
}
