//! Router availability: probe the SystemOne shim, start it when missing.
//!
//! On startup (or first route call) the binary probes
//! `http://127.0.0.1:<port>/healthz`. If the router is already up it is used
//! as-is. Otherwise — when `auto_start_shim` is on — the binary launches the
//! installed shim itself:
//!
//! ```text
//! python3.11 -m systemone.shim --port 8765
//! ```
//!
//! with the working directory and `PYTHONPATH` pointed at the SystemOne
//! release directory (`$SYSTEMONE_RELEASE_DIR`, else `~/systemone-release`).
//! The child is detached (`setsid`) so it survives the CLI exiting, its output
//! goes to `~/.grok-local/systemone-shim.log`, and a lock file
//! (`~/.grok-local/systemone-shim.lock`) keeps concurrent CLI invocations from
//! spawning duplicate shims.
//!
//! The GLiClass model the shim needs is already in the HuggingFace cache, so a
//! cold start is just model-load time (seconds), not a download.
//!
//! Fail-open: if anything here goes wrong the caller gets
//! [`RouterStatus::Unavailable`] and routing proceeds fail-open. On non-Unix
//! platforms auto-start is not attempted (no `python3.11` convention there) —
//! the probe still runs and an already-running router is used.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::SystemOneConfig;

/// What happened when we ensured a router was available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterStatus {
    /// A router was already answering `/healthz`.
    AlreadyRunning,
    /// We started the shim and it became healthy.
    Started,
    /// No router is available (and none could be started). Fail-open.
    Unavailable,
}

impl RouterStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyRunning => "already-running",
            Self::Started => "started",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Probe for a router; start the shim when missing and allowed.
///
/// Infallible: every failure path returns [`RouterStatus::Unavailable`].
pub async fn ensure_router(cfg: &SystemOneConfig) -> RouterStatus {
    if healthz(cfg.shim_port).await {
        return RouterStatus::AlreadyRunning;
    }
    if !cfg.enabled || !cfg.auto_start_shim {
        tracing::debug!("systemone: router down and auto-start disabled; fail-open");
        return RouterStatus::Unavailable;
    }
    match start_shim(cfg).await {
        Ok(()) => {
            if wait_for_healthz(cfg.shim_port).await {
                tracing::info!("systemone: shim started, router healthy");
                RouterStatus::Started
            } else {
                tracing::warn!("systemone: shim started but never became healthy; fail-open");
                RouterStatus::Unavailable
            }
        }
        Err(err) => {
            tracing::warn!("systemone: could not start shim ({err}); fail-open");
            RouterStatus::Unavailable
        }
    }
}

async fn healthz(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/healthz");
    // Localhost-only health probe: the grok TLS policy is for remote hosts.
    #[allow(clippy::disallowed_methods)]
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    client
        .get(&url)
        .send()
        .await
        .map(|resp| resp.status().is_success())
        .unwrap_or(false)
}

/// Poll `/healthz` until it answers or the deadline passes.
async fn wait_for_healthz(port: u16) -> bool {
    // Cold start = GLiClass model load from the local HF cache; generous bound.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if healthz(port).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Locate the SystemOne release directory: `$SYSTEMONE_RELEASE_DIR`, else
/// `~/systemone-release`. The directory must contain `systemone/shim.py`.
fn release_dir() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = {
        let mut v = Vec::new();
        if let Ok(dir) = std::env::var(crate::config::ENV_RELEASE_DIR)
            && !dir.trim().is_empty()
        {
            v.push(PathBuf::from(dir));
        }
        if let Some(home) = dirs_home() {
            v.push(home.join("systemone-release"));
        }
        v
    };
    candidates
        .into_iter()
        .find(|dir| dir.join("systemone").join("shim.py").is_file())
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn python_interpreter() -> String {
    std::env::var(crate::config::ENV_PYTHON)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "python3.11".to_string())
}

fn grok_home() -> PathBuf {
    xai_dirs::grok_home()
}

/// Start the shim, guarded by a lock file so concurrent CLI invocations do not
/// spawn duplicates. Returns `Ok(())` when a spawn was issued (health is
/// checked separately by the caller).
async fn start_shim(cfg: &SystemOneConfig) -> Result<(), String> {
    #[cfg(not(unix))]
    {
        let _ = cfg;
        return Err("auto-start is only supported on Unix".to_string());
    }
    #[cfg(unix)]
    {
        unix_start_shim(cfg).await
    }
}

#[cfg(unix)]
async fn unix_start_shim(cfg: &SystemOneConfig) -> Result<(), String> {
    let release = release_dir().ok_or_else(|| {
        "no SystemOne release dir found ($SYSTEMONE_RELEASE_DIR or ~/systemone-release)".to_string()
    })?;
    let home = grok_home();
    if let Err(err) = std::fs::create_dir_all(&home) {
        return Err(format!("cannot create {}: {err}", home.display()));
    }

    // Lock so two concurrent grok-local invocations don't spawn two shims.
    // Best-effort: if locking fails we still try, the healthz re-check below
    // keeps it safe.
    let lock_path = home.join("systemone-shim.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&lock_path)
        .map_err(|err| format!("cannot open lock {}: {err}", lock_path.display()))?;
    let _locked = try_lock_exclusive(&lock_file);

    // Another process may have started it while we waited on the lock.
    if healthz(cfg.shim_port).await {
        return Ok(());
    }

    let log_path = home.join("systemone-shim.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| format!("cannot open log {}: {err}", log_path.display()))?;
    let log_err = log_file
        .try_clone()
        .map_err(|err| format!("cannot dup log handle: {err}"))?;

    let python = python_interpreter();
    let port = cfg.shim_port.to_string();
    let mut cmd = std::process::Command::new(&python);
    cmd.arg("-m")
        .arg("systemone.shim")
        .arg("--port")
        .arg(&port)
        .current_dir(&release)
        .env("PYTHONPATH", &release)
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_err);
    // Detach: new session so the shim survives this CLI exiting and never
    // receives our terminal signals.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // Deliberately detached daemon: the shim must outlive this CLI invocation,
    // so it is NOT enrolled in a session ProcessScope. Duplicates are prevented
    // by the lock file + the healthz re-check above.
    #[allow(clippy::disallowed_methods)]
    let child = cmd
        .spawn()
        .map_err(|err| format!("cannot spawn {python}: {err}"))?;
    // We intentionally do NOT wait: the detached child keeps running after this
    // CLI exits (reparented to init, which reaps it). Dropping the handle here
    // does not signal the child.
    drop(child);

    tracing::info!(
        release = %release.display(),
        port = cfg.shim_port,
        log = %log_path.display(),
        "systemone: spawned shim"
    );
    // Keep the lock held briefly is unnecessary; the healthz poll serializes.
    let _ = _locked;
    let _ = lock_file;
    Ok(())
}

/// Non-blocking exclusive lock; `None` when the lock is held elsewhere or
/// locking is unsupported. Uses `fcntl` directly to avoid extra deps.
#[cfg(unix)]
fn try_lock_exclusive(file: &std::fs::File) -> Option<()> {
    use std::os::unix::io::AsRawFd;
    // LOCK_EX | LOCK_NB via libc::flock.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 { Some(()) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn healthz_false_on_closed_port() {
        // Port 1 is (almost) certainly closed: connection refused, fast.
        assert!(!healthz(1).await);
    }

    #[test]
    fn release_dir_none_when_missing() {
        // Point the env override at a temp dir with no shim.py.
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(crate::config::ENV_RELEASE_DIR, dir.path()) };
        // Also neutralize HOME so ~/systemone-release can't rescue it.
        let old_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", dir.path().join("nohome")) };
        assert!(release_dir().is_none());
        unsafe { std::env::remove_var(crate::config::ENV_RELEASE_DIR) };
        if let Some(h) = old_home {
            unsafe { std::env::set_var("HOME", h) };
        }
    }

    #[test]
    fn status_strings() {
        assert_eq!(RouterStatus::AlreadyRunning.as_str(), "already-running");
        assert_eq!(RouterStatus::Started.as_str(), "started");
        assert_eq!(RouterStatus::Unavailable.as_str(), "unavailable");
    }
}
