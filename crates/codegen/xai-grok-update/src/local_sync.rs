//! Overlay-preserving grok-build source updates for grok-local.
//!
//! Never downloads the official `grok` binary. `grok-local update` fetches
//! https://github.com/xai-org/grok-build and three-way-merges it onto this
//! fork's overlay via `scripts/sync-upstream.py`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::process::Command;

const UPSTREAM_SOURCE_REV_URL: &str =
    "https://raw.githubusercontent.com/xai-org/grok-build/main/SOURCE_REV";
const UPSTREAM_VERSION_TOML_URL: &str =
    "https://raw.githubusercontent.com/xai-org/grok-build/main/crates/codegen/xai-grok-version/Cargo.toml";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayUpdateStatus {
    pub grok_local: String,
    pub grok_build: String,
    pub source_rev: String,
    pub latest_source_rev: Option<String>,
    pub latest_grok_build: Option<String>,
    pub update_available: bool,
    pub source_tree: Option<String>,
    pub error: Option<String>,
}

pub fn find_source_tree() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("GROK_LOCAL_SRC") {
        let p = PathBuf::from(p);
        if is_source_tree(&p) {
            return Some(p);
        }
    }
    if let Ok(mut cur) = std::env::current_dir() {
        for _ in 0..12 {
            if is_source_tree(&cur) {
                return Some(cur);
            }
            if !cur.pop() {
                break;
            }
        }
    }
    if let Some(home) = xai_dirs::home_dir() {
        for candidate in [
            home.join("Desktop/Local-Grok-Cli"),
            home.join("local-grok-cli"),
            home.join(".grok-local/src"),
        ] {
            if is_source_tree(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_source_tree(path: &Path) -> bool {
    path.join("SOURCE_REV").is_file() && path.join("scripts/sync-upstream.py").is_file()
}

pub async fn check_overlay_status() -> OverlayUpdateStatus {
    let grok_local = xai_grok_version::LOCAL_VERSION.to_string();
    let grok_build = xai_grok_version::VERSION.to_string();
    let source_rev = xai_grok_version::source_rev().to_string();
    let source_tree = find_source_tree().map(|p| p.display().to_string());

    match fetch_upstream_identity().await {
        Ok((latest_source_rev, latest_grok_build)) => {
            let update_available = latest_source_rev != source_rev;
            OverlayUpdateStatus {
                grok_local,
                grok_build,
                source_rev,
                latest_source_rev: Some(latest_source_rev),
                latest_grok_build: Some(latest_grok_build),
                update_available,
                source_tree,
                error: None,
            }
        }
        Err(e) => OverlayUpdateStatus {
            grok_local,
            grok_build,
            source_rev,
            latest_source_rev: None,
            latest_grok_build: None,
            update_available: false,
            source_tree,
            error: Some(e.to_string()),
        },
    }
}

async fn fetch_upstream_identity() -> Result<(String, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let source_rev = client
        .get(UPSTREAM_SOURCE_REV_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let source_rev = source_rev.trim().to_string();
    let toml = client
        .get(UPSTREAM_VERSION_TOML_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let version = toml
        .lines()
        .find_map(|ln| {
            ln.trim()
                .strip_prefix("version")
                .and_then(|rest| rest.trim().strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    Ok((source_rev, version))
}

pub fn print_overlay_status(status: &OverlayUpdateStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(status)?);
        return Ok(());
    }
    println!(
        "Grok Local {}  (grok-build {} / {})",
        status.grok_local, status.grok_build, status.source_rev
    );
    if let Some(error) = status.error.as_deref() {
        println!("Update check failed: {error}");
        return Ok(());
    }
    match (
        status.latest_grok_build.as_deref(),
        status.latest_source_rev.as_deref(),
    ) {
        (Some(latest), Some(rev)) if status.update_available => {
            println!("A new grok-build snapshot is available: {} -> {latest} ({rev})", status.grok_build);
            println!("Run `grok-local update --upstream` in the source tree to overlay-merge without replacing fork patches.");
        }
        (Some(latest), Some(_)) => {
            println!("Already on latest grok-build {latest} (overlay merge).");
        }
        _ => {}
    }
    if let Some(tree) = status.source_tree.as_deref() {
        println!("Source tree: {tree}");
    } else {
        println!("No grok-local source tree found. Set GROK_LOCAL_SRC or run from the clone.");
    }
    Ok(())
}

pub async fn run_overlay_update(rebuild: bool) -> Result<()> {
    let Some(src) = find_source_tree() else {
        bail!(
            "Cannot overlay-merge grok-build: grok-local source tree not found.\n\
             Clone https://github.com/Franzferdinan51/local-grok-cli and run from that directory,\n\
             or set GROK_LOCAL_SRC. Official grok binaries are never installed (they would overwrite this fork)."
        );
    };
    let script = src.join("scripts/sync-upstream.py");
    eprintln!("  Overlay-merging grok-build into {} ...", src.display());
    let status = Command::new("python3")
        .arg(&script)
        .arg("--local")
        .arg(&src)
        .current_dir(&src)
        .stdin(Stdio::null())
        .status()
        .await
        .with_context(|| format!("failed to spawn {}", script.display()))?;
    if !status.success() {
        bail!(
            "overlay merge exited {}. Resolve <<<<<<< grok-local markers, then re-run `grok-local update`.",
            status.code().unwrap_or(-1)
        );
    }
    if rebuild {
        eprintln!("  Building grok-local (release) ...");
        let status = Command::new("cargo")
            .args(["build", "-p", "xai-grok-pager-bin", "--release"])
            .current_dir(&src)
            .stdin(Stdio::null())
            .status()
            .await
            .context("failed to spawn cargo")?;
        if !status.success() {
            bail!("cargo build failed (exit {})", status.code().unwrap_or(-1));
        }
        let built = src.join("target/release/grok-local");
        let dest = install_destination();
        if built.exists() {
            if let Some(dest) = dest {
                tokio::fs::copy(&built, &dest)
                    .await
                    .with_context(|| format!("copy {} -> {}", built.display(), dest.display()))?;
                eprintln!("  Installed {}", dest.display());
            } else {
                eprintln!("  Built {}", built.display());
            }
        }
    }
    eprintln!("  grok-local overlay update complete.");
    Ok(())
}

fn install_destination() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let name = exe.file_name()?.to_string_lossy();
        if name == "grok-local" || name == "grok-local.exe" {
            return Some(exe);
        }
    }
    let home = xai_dirs::home_dir()?;
    let dest = home.join(".local/bin/grok-local");
    dest.exists().then_some(dest)
}
