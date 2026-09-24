//! First-run onboarding wizard (`grok-local onboard`).
//!
//! Runs automatically on the first interactive launch when no config file
//! exists yet (skippable via `--skip-onboarding`, `GROK_LOCAL_SKIP_ONBOARDING=1`,
//! or `GROK_LOCAL_ONBOARDING=0`), and on demand via `grok-local onboard`.
//!
//! Design notes:
//! - Every check is advisory: warnings never block, and the wizard never
//!   loads, unloads, or switches models — it only *detects* what is there.
//! - Idempotent: every step is a read-modify-write of
//!   `$GROK_HOME/config.toml` (`~/.grok-local/config.toml` by default);
//!   re-running rewrites the same keys.
//! - Headless: no TTY (or `--yes`) means accept defaults / skip with a note —
//!   never block on input.
//! - No model IDs are hard-coded anywhere: the model question only chooses
//!   between "let SystemOne decide" (`model_selection = "auto"`) and
//!   "use the session's active model" (`"pinned"`).

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use xai_grok_systemone::config::SystemOneConfig;

/// Escape hatch: `0`/`false`/`off`/`no` disables the first-run trigger.
pub const ENV_ONBOARDING: &str = "GROK_LOCAL_ONBOARDING";
/// Escape hatch: `1`/`true`/`on`/`yes` disables the first-run trigger.
pub const ENV_SKIP_ONBOARDING: &str = "GROK_LOCAL_SKIP_ONBOARDING";

/// `grok-local onboard` arguments.
#[derive(Debug, Clone, Parser)]
pub struct OnboardArgs {
    /// Accept all defaults without prompting (safe for scripts/CI).
    #[arg(long)]
    pub yes: bool,
    /// Only run the requirements check; write nothing.
    #[arg(long)]
    pub check: bool,
}

/// Options for the testable core ([`run_with_io`]).
#[derive(Debug, Clone, Copy)]
pub struct OnboardOptions {
    /// Answer every prompt with its default (implies non-interactive).
    pub accept_defaults: bool,
    /// Run only the requirements check; never write the config file.
    pub check_only: bool,
}

/// Outcome of one requirement check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

/// One line of the requirements report.
#[derive(Debug, Clone)]
pub struct ReqCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

/// Structured result of a wizard run (used by tests and `--check` output).
#[derive(Debug, Default)]
pub struct OnboardReport {
    pub checks: Vec<ReqCheck>,
    pub lm_studio_models: Vec<String>,
    pub shim_reachable: bool,
    pub config_written: bool,
    pub skipped: bool,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// `grok-local onboard` implementation.
///
/// Headless (no TTY) without `--yes`: prints a note and exits 0 — never hangs.
pub fn run(args: &OnboardArgs) -> Result<()> {
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal();
    if !interactive && !args.yes && !args.check {
        println!(
            "grok-local onboard: no interactive terminal detected — skipping (nothing was changed)."
        );
        println!("Re-run with a TTY, or pass --yes to accept defaults non-interactively.");
        return Ok(());
    }
    let opts = OnboardOptions {
        accept_defaults: args.yes || !interactive,
        check_only: args.check,
    };
    let home = xai_dirs::grok_home();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let report = run_with_io(&mut input, &mut output, &home, opts)?;
    let _ = writeln!(output);
    if report.skipped {
        writeln!(
            output,
            "Onboarding skipped — run `grok-local onboard` anytime to resume."
        )?;
    } else if opts.check_only {
        writeln!(
            output,
            "Requirements check done — nothing was written (--check)."
        )?;
    } else if report.config_written {
        writeln!(
            output,
            "Setup complete. Config: {}",
            home.join("config.toml").display()
        )?;
        writeln!(output, "Re-run anytime with `grok-local onboard`.")?;
    }
    Ok(())
}

/// First-run trigger, called on interactive launches before the TUI starts.
///
/// Fires only when: not skipped (flag or env), no config file exists yet, and
/// stdin is a TTY. Never fails the launch — wizard errors are reported and
/// the session continues.
pub fn maybe_run_first_run(skip_flag: bool) {
    if skip_flag || onboarding_disabled_by_env() {
        return;
    }
    let home = xai_dirs::grok_home();
    if home.join("config.toml").exists() {
        return;
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return; // headless: stay silent, never block
    }
    println!("Welcome to grok-local! No config found — running quick first-run setup.");
    println!("(Skip with --skip-onboarding, GROK_LOCAL_SKIP_ONBOARDING=1, or 'q' below.)\n");
    let args = OnboardArgs {
        yes: false,
        check: false,
    };
    if let Err(e) = run(&args) {
        eprintln!("onboarding: {e:#} (continuing without setup)");
    }
}

fn onboarding_disabled_by_env() -> bool {
    fn is_off(name: &str) -> bool {
        std::env::var(name).ok().is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
    }
    fn is_on(name: &str) -> bool {
        std::env::var(name).ok().is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
    }
    is_off(ENV_ONBOARDING) || is_on(ENV_SKIP_ONBOARDING)
}

// ---------------------------------------------------------------------------
// Testable core
// ---------------------------------------------------------------------------

/// Runs the wizard against explicit input/output and grok-home paths.
///
/// `reader` returning EOF immediately (empty input) is treated as "accept
/// every default" — this is what makes headless runs terminate.
pub fn run_with_io<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    grok_home: &Path,
    opts: OnboardOptions,
) -> Result<OnboardReport> {
    let mut report = OnboardReport::default();

    writeln!(writer, "=== grok-local setup ===\n")?;
    writeln!(writer, "--- 1. Requirements ---")?;
    run_requirement_checks(writer, grok_home, &mut report);
    writeln!(writer)?;

    if opts.check_only {
        return Ok(report);
    }

    writeln!(writer, "--- 2. Inference ---")?;
    if !step_inference(reader, writer, grok_home, opts, &mut report)? {
        report.skipped = true;
        return Ok(report);
    }
    writeln!(writer)?;

    writeln!(writer, "--- 3. SystemOne routing ---")?;
    if !step_systemone(reader, writer, grok_home, opts, &mut report)? {
        report.skipped = true;
        return Ok(report);
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Prompts (EOF == accept default; "q" == skip the rest of the wizard)
// ---------------------------------------------------------------------------

/// Reads one line; `None` on EOF or IO error (treated as "accept default").
fn prompt_line<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
) -> Option<String> {
    let _ = write!(writer, "{prompt}");
    let _ = writer.flush();
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim().to_string()),
        Err(_) => None,
    }
}

/// Yes/no question. Returns `None` when the user quits (`q`).
fn prompt_yes_no<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    opts: OnboardOptions,
    question: &str,
    default: bool,
) -> Option<bool> {
    if opts.accept_defaults {
        let _ = writeln!(
            writer,
            "{question} [{}] (default)",
            if default { "Y/n" } else { "y/N" }
        );
        return Some(default);
    }
    let hint = if default { "Y/n" } else { "y/N" };
    match prompt_line(
        reader,
        writer,
        &format!("{question} [{hint}] (q to skip): "),
    ) {
        None => Some(default),
        Some(s) if s.eq_ignore_ascii_case("q") || s.eq_ignore_ascii_case("quit") => None,
        Some(s) if s.is_empty() => Some(default),
        Some(s) => Some(matches!(
            s.to_ascii_lowercase().as_str(),
            "y" | "yes" | "1" | "true"
        )),
    }
}

/// Numbered choice. Returns `None` when the user quits (`q`).
fn prompt_choice<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    opts: OnboardOptions,
    question: &str,
    choices: &[&str],
    default_idx: usize,
) -> Option<usize> {
    debug_assert!(!choices.is_empty());
    debug_assert!(default_idx < choices.len());
    if opts.accept_defaults {
        let _ = writeln!(writer, "{question}");
        for (i, c) in choices.iter().enumerate() {
            let _ = writeln!(
                writer,
                "  {}) {c}{}",
                i + 1,
                if i == default_idx { " (default)" } else { "" }
            );
        }
        return Some(default_idx);
    }
    let _ = writeln!(writer, "{question}");
    for (i, c) in choices.iter().enumerate() {
        let _ = writeln!(writer, "  {}) {c}", i + 1);
    }
    match prompt_line(
        reader,
        writer,
        &format!("Choice [{}] (q to skip): ", default_idx + 1),
    ) {
        None => Some(default_idx),
        Some(s) if s.eq_ignore_ascii_case("q") || s.eq_ignore_ascii_case("quit") => None,
        Some(s) if s.is_empty() => Some(default_idx),
        Some(s) => match s.parse::<usize>() {
            Ok(n) if (1..=choices.len()).contains(&n) => Some(n - 1),
            _ => Some(default_idx),
        },
    }
}

// ---------------------------------------------------------------------------
// Step 1: requirements (advisory only — warnings never block)
// ---------------------------------------------------------------------------

fn run_requirement_checks<W: Write>(writer: &mut W, grok_home: &Path, report: &mut OnboardReport) {
    // Disk space at the grok home (binary ~250MB, router ~1GB, Jeff-1 ~15GB).
    let disk = disk_free_bytes(grok_home);
    report.checks.push(match disk {
        Some(free) if free >= 2 * 1024 * 1024 * 1024 => ReqCheck {
            name: "disk",
            status: CheckStatus::Pass,
            detail: format!("{} free at {}", gb(free), grok_home.display()),
        },
        Some(free) => ReqCheck {
            name: "disk",
            status: CheckStatus::Warn,
            detail: format!(
                "only {} free at {} — the binary needs ~250MB and the SystemOne router ~1GB (+15GB for Jeff-1)",
                gb(free),
                grok_home.display()
            ),
        },
        None => ReqCheck {
            name: "disk",
            status: CheckStatus::Warn,
            detail: "could not measure free disk space on this platform".to_string(),
        },
    });

    // Python >= 3.10 for the SystemOne shim.
    report.checks.push(match python_version() {
        Some((interp, maj, min)) if maj > 3 || (maj == 3 && min >= 10) => ReqCheck {
            name: "python",
            status: CheckStatus::Pass,
            detail: format!("{interp} {maj}.{min} (>= 3.10, good for the SystemOne shim)"),
        },
        Some((interp, maj, min)) => ReqCheck {
            name: "python",
            status: CheckStatus::Warn,
            detail: format!(
                "{interp} {maj}.{min} is older than 3.10 — the SystemOne shim may not start; routing will fail open"
            ),
        },
        None => ReqCheck {
            name: "python",
            status: CheckStatus::Warn,
            detail: "no python3.11/python3 found — the SystemOne shim needs Python >= 3.10; routing will fail open without it".to_string(),
        },
    });

    // RAM: informational only. The tools are light; the *model* drives RAM/VRAM.
    report.checks.push(match total_ram_bytes() {
        Some(ram) => ReqCheck {
            name: "ram",
            status: CheckStatus::Pass,
            detail: format!(
                "{} detected — grok-local itself is light; your model is what needs RAM/VRAM (a 35B-class model wants tens of GB — check the model card)",
                gb(ram)
            ),
        },
        None => ReqCheck {
            name: "ram",
            status: CheckStatus::Warn,
            detail: "could not detect total RAM on this platform".to_string(),
        },
    });

    for check in &report.checks {
        let tag = match check.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        };
        let _ = writeln!(writer, "[{tag}] {}: {}", check.name, check.detail);
    }
    if report.checks.iter().any(|c| c.status == CheckStatus::Fail) {
        let _ = writeln!(
            writer,
            "Some checks failed — fix them if you can, but setup can continue."
        );
    }
}

#[cfg(unix)]
fn disk_free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    Some(vfs.f_bavail as u64 * vfs.f_frsize as u64)
}

#[cfg(not(unix))]
fn disk_free_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn total_ram_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn total_ram_bytes() -> Option<u64> {
    use std::ffi::CString;
    let name = CString::new("hw.memsize").ok()?;
    let mut size: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut size as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(size)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn total_ram_bytes() -> Option<u64> {
    None
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
}

/// Returns `(interpreter, major, minor)` for the first usable Python found.
fn python_version() -> Option<(String, u32, u32)> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("SYSTEMONE_PYTHON") {
        if !p.trim().is_empty() {
            candidates.push(p);
        }
    }
    candidates.push("python3.11".to_string());
    candidates.push("python3".to_string());
    for cand in candidates {
        let out = std::process::Command::new(&cand)
            .arg("--version")
            .output()
            .ok()?;
        if !out.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let text = if stdout.trim().is_empty() {
            String::from_utf8_lossy(&out.stderr).into_owned()
        } else {
            stdout.into_owned()
        };
        if let Some((maj, min)) = parse_python_version(&text) {
            return Some((cand, maj, min));
        }
    }
    None
}

/// Parses `Python 3.11.9` (or `Python 3.11.9\n`) into `(3, 11)`.
fn parse_python_version(s: &str) -> Option<(u32, u32)> {
    let rest = s.trim().strip_prefix("Python")?.trim();
    let mut parts = rest.split('.');
    let maj: u32 = parts.next()?.parse().ok()?;
    let min: u32 = parts.next()?.trim().parse().ok()?;
    Some((maj, min))
}

// ---------------------------------------------------------------------------
// Probes (TCP + minimal HTTP; no new dependencies)
// ---------------------------------------------------------------------------

fn tcp_reachable(port: u16) -> bool {
    let addr: Option<std::net::SocketAddr> = format!("127.0.0.1:{port}").parse().ok();
    match addr {
        Some(a) => std::net::TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok(),
        None => false,
    }
}

/// Minimal blocking HTTP GET over a raw TCP stream. Returns the parsed JSON
/// body, or `None` on any failure (connection refused, timeout, bad JSON).
fn probe_http_json(port: u16, path: &str) -> Option<serde_json::Value> {
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(700)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    {
        use std::io::Read;
        let _ = stream.read_to_end(&mut buf);
    }
    let text = String::from_utf8_lossy(&buf);
    let body = text.split("\r\n\r\n").nth(1)?;
    serde_json::from_str(body).ok()
}

/// Model IDs served by LM Studio on `:1234`, if it is up. Detection only —
/// the wizard never loads, unloads, or switches models.
fn lm_studio_models() -> Option<Vec<String>> {
    let v = probe_http_json(1234, "/v1/models")?;
    let data = v.get("data")?.as_array()?;
    let ids: Vec<String> = data
        .iter()
        .filter_map(|m| m.get("id")?.as_str().map(str::to_string))
        .collect();
    Some(ids)
}

// ---------------------------------------------------------------------------
// Step 2: inference
// ---------------------------------------------------------------------------

/// Returns `false` when the user quits the wizard.
fn step_inference<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    grok_home: &Path,
    opts: OnboardOptions,
    report: &mut OnboardReport,
) -> Result<bool> {
    match lm_studio_models() {
        Some(models) => {
            report.lm_studio_models = models.clone();
            writeln!(writer, "LM Studio detected on :1234.")?;
            if models.is_empty() {
                writeln!(writer, "It reports no loaded models right now.")?;
            } else {
                writeln!(writer, "Models it serves:")?;
                for m in &models {
                    writeln!(writer, "  - {m}")?;
                }
            }
            writeln!(
                writer,
                "grok-local will never load, unload, or switch models on its own — this only sets a preference."
            )?;
        }
        None => {
            writeln!(
                writer,
                "No LM Studio on :1234. grok-local needs somewhere to run models:"
            )?;
            writeln!(
                writer,
                "  - LM Studio locally (OpenAI-compatible, http://localhost:1234), or"
            )?;
            writeln!(
                writer,
                "  - an API key for a hosted provider (configure later)."
            )?;
            writeln!(writer, "Continuing with SystemOne routing setup only.")?;
        }
    }

    let choice = prompt_choice(
        reader,
        writer,
        opts,
        "How should grok-local choose a model for each task?",
        &[
            "Let SystemOne decide per task (recommended — advisory, fail-open)",
            "Always use the session's active model (pinned)",
        ],
        0,
    );
    let Some(idx) = choice else {
        return Ok(false);
    };
    let value = if idx == 0 { "auto" } else { "pinned" };
    if SystemOneConfig::save_systemone_key_at(
        grok_home,
        "model_selection",
        toml::Value::String(value.to_string()),
    ) {
        report.config_written = true;
        writeln!(writer, "Saved: [systemone] model_selection = \"{value}\"")?;
    } else {
        writeln!(
            writer,
            "WARN: could not write the config file — continuing."
        )?;
    }
    Ok(true)
}

/// Returns `false` when the user quits the wizard.
fn step_systemone<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    grok_home: &Path,
    opts: OnboardOptions,
    report: &mut OnboardReport,
) -> Result<bool> {
    report.shim_reachable = tcp_reachable(8765);
    if report.shim_reachable {
        writeln!(writer, "SystemOne router is reachable on :8765.")?;
    } else {
        writeln!(
            writer,
            "SystemOne router not running on :8765 — grok-local starts the shim itself when needed (Unix)."
        )?;
    }
    writeln!(
        writer,
        "Routing is advisory and fail-open: if the router is down, grok-local just uses its defaults."
    )?;

    let enabled = prompt_yes_no(
        reader,
        writer,
        opts,
        "Enable SystemOne routing (tier/effort/tool suggestions per task)?",
        true,
    );
    let Some(enabled) = enabled else {
        return Ok(false);
    };
    if SystemOneConfig::save_systemone_bool_at(grok_home, "enabled", enabled) {
        report.config_written = true;
        writeln!(writer, "Saved: [systemone] enabled = {enabled}")?;
    } else {
        writeln!(
            writer,
            "WARN: could not write the config file — continuing."
        )?;
    }

    writeln!(
        writer,
        "Jeff-1 is an optional second decision head: plan ranking plus a second opinion on uncertain routes."
    )?;
    writeln!(
        writer,
        "It runs as a sidecar and never touches your loaded model. Kill switch: SYSTEMONE_JEFF1=0."
    )?;
    let jeff1 = prompt_yes_no(
        reader,
        writer,
        opts,
        "Keep the Jeff-1 second head on?",
        true,
    );
    let Some(jeff1) = jeff1 else {
        return Ok(false);
    };
    if SystemOneConfig::save_systemone_bool_at(grok_home, "jeff1_enabled", jeff1) {
        report.config_written = true;
        writeln!(writer, "Saved: [systemone] jeff1_enabled = {jeff1}")?;
    } else {
        writeln!(
            writer,
            "WARN: could not write the config file — continuing."
        )?;
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn test_opts() -> OnboardOptions {
        OnboardOptions {
            accept_defaults: false,
            check_only: false,
        }
    }

    fn run_with_input(input: &[u8], home: &Path, opts: OnboardOptions) -> (OnboardReport, String) {
        let mut reader = Cursor::new(input);
        let mut output: Vec<u8> = Vec::new();
        let report = run_with_io(&mut reader, &mut output, home, opts).expect("wizard runs");
        (report, String::from_utf8_lossy(&output).into_owned())
    }

    fn read_config(home: &Path) -> String {
        std::fs::read_to_string(home.join("config.toml")).expect("config written")
    }

    /// Empty stdin (EOF on every prompt) must terminate and write defaults —
    /// this is the headless-safety property: the wizard can never hang.
    #[test]
    fn empty_input_accepts_defaults_and_never_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let (report, _out) = run_with_input(b"", dir.path(), test_opts());
        assert!(!report.skipped);
        assert!(report.config_written);
        assert_eq!(report.checks.len(), 3);
        let cfg = read_config(dir.path());
        assert!(cfg.contains("model_selection = \"auto\""), "{cfg}");
        assert!(cfg.contains("enabled = true"), "{cfg}");
        assert!(cfg.contains("jeff1_enabled = true"), "{cfg}");
        // No model IDs anywhere in the wizard's output.
        assert!(!cfg.contains("ornith") && !cfg.contains("grok-"), "{cfg}");
    }

    /// Re-running with the same answers must not change the config file.
    #[test]
    fn double_run_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // Scripted: choose pinned (2), keep routing on (y), keep Jeff-1 on (y).
        let input = b"2\ny\ny\n";
        let (r1, _) = run_with_input(input, dir.path(), test_opts());
        assert!(!r1.skipped);
        let first = read_config(dir.path());
        assert!(first.contains("model_selection = \"pinned\""), "{first}");
        let (r2, _) = run_with_input(input, dir.path(), test_opts());
        assert!(!r2.skipped);
        let second = read_config(dir.path());
        assert_eq!(first, second, "second run must not change the config");
    }

    /// `--check` runs the requirement checks and writes nothing.
    #[test]
    fn check_only_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let opts = OnboardOptions {
            accept_defaults: false,
            check_only: true,
        };
        let (report, out) = run_with_input(b"", dir.path(), opts);
        assert_eq!(report.checks.len(), 3);
        assert!(!report.config_written);
        assert!(!dir.path().join("config.toml").exists());
        assert!(out.contains("PASS") || out.contains("WARN"));
    }

    /// `q` at the first prompt skips the rest and writes nothing.
    #[test]
    fn quit_skips_rest_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (report, _out) = run_with_input(b"q\n", dir.path(), test_opts());
        assert!(report.skipped);
        assert!(!report.config_written);
        assert!(!dir.path().join("config.toml").exists());
    }

    /// `--yes` accepts every default without reading input.
    #[test]
    fn yes_flag_accepts_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let opts = OnboardOptions {
            accept_defaults: true,
            check_only: false,
        };
        let (report, _out) = run_with_input(b"", dir.path(), opts);
        assert!(!report.skipped);
        assert!(report.config_written);
        let cfg = read_config(dir.path());
        assert!(cfg.contains("model_selection = \"auto\""), "{cfg}");
    }

    #[test]
    fn parse_python_version_cases() {
        assert_eq!(parse_python_version("Python 3.11.9"), Some((3, 11)));
        assert_eq!(parse_python_version("Python 3.10.0\n"), Some((3, 10)));
        assert_eq!(parse_python_version("Python 2.7.18"), Some((2, 7)));
        assert_eq!(parse_python_version("not python"), None);
        assert_eq!(parse_python_version("Python"), None);
        assert_eq!(parse_python_version(""), None);
    }

    /// The wizard's own copy must not name a model (regression guard for the
    /// no-hard-coded-model-IDs rule). Banned words are built from fragments
    /// so the test's own source can't trip the scan; only non-test code is
    /// scanned.
    #[test]
    fn wizard_copy_names_no_models() {
        let src = include_str!("onboard_cmd.rs");
        let code = src
            .split("#[cfg(test)]")
            .next()
            .unwrap_or(src)
            .to_ascii_lowercase();
        let banned = [
            ["or", "nith"].concat(),
            ["gr", "ok-4"].concat(),
            ["qw", "en"].concat(),
            ["lla", "ma"].concat(),
        ];
        for b in &banned {
            assert!(
                !code.contains(b.as_str()),
                "wizard copy must not name models (found {b})"
            );
        }
    }
}
