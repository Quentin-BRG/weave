// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Diagnostics (specification section 155).
//!
//! There are two audiences and one implementation.
//!
//! * **Installation checks** ([`install_report`]) answer "is the product on
//!   this machine intact?". They never need a Git repository, so a package's
//!   post-install step can run them, and `weave doctor --install` reports them
//!   on their own.
//! * **Repository checks** ([`repository_report`]) answer "can a session start
//!   here?".
//!
//! `weave doctor` runs both. `weave host` and `weave join` run the subset that
//! must hold before a session starts ([`ensure_ready`]) and stay silent when it
//! does — a user should never have to remember to run `weave doctor` first.
//! Everything is built from the same `check_*` functions so the two paths
//! cannot drift apart.

use crate::error::{Result, WeaveError};
use crate::gitx;
use crate::install;
use crate::model::DEFAULT_MAX_FILE_SIZE;
use crate::path::RepoPath;
use crate::session::Paths;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

/// Which family of checks a report contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Installation and machine only; no repository required.
    Install,
    /// Installation plus this repository.
    Full,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    /// What to do about it, when there is something to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub scope: Scope,
    pub checks: Vec<Check>,
    pub ready: bool,
}

impl Report {
    fn new(scope: Scope, checks: Vec<Check>) -> Report {
        let ready = !checks.iter().any(|c| c.status == CheckStatus::Fail);
        Report {
            scope,
            checks,
            ready,
        }
    }

    /// The first check that makes Weave unusable, if any.
    pub fn first_failure(&self) -> Option<&Check> {
        self.checks.iter().find(|c| c.status == CheckStatus::Fail)
    }
}

fn pass(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Pass,
        detail: detail.into(),
        hint: None,
    }
}
fn warn(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Warn,
        detail: detail.into(),
        hint: None,
    }
}
fn fail(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Fail,
        detail: detail.into(),
        hint: None,
    }
}

impl Check {
    fn with_hint(mut self, hint: impl Into<String>) -> Check {
        self.hint = Some(hint.into());
        self
    }
}

// ---------------------------------------------------------------------------
// Installation checks — no repository required
// ---------------------------------------------------------------------------

/// Everything that must be true of the installed product, independent of any
/// repository. Safe to run from a package's post-install step, including as
/// root: it reads the installation, and touches nothing belonging to a user.
pub fn install_report() -> Report {
    let mut checks = Vec::new();
    checks.push(check_executable());
    checks.push(check_platform());
    checks.extend(check_bundle());
    checks.push(check_git());
    checks.push(check_sqlite());
    Report::new(Scope::Install, checks)
}

/// The running executable is where it says it is, and starts.
fn check_executable() -> Check {
    let Some(exe) = install::exe() else {
        return fail("Weave executable", "The running executable has no path.")
            .with_hint("Reinstall Weave.");
    };
    if !install::is_readable(&exe) {
        return fail(
            "Weave executable",
            format!("{} cannot be read.", exe.display()),
        )
        .with_hint("Reinstall Weave.");
    }
    let version = env!("CARGO_PKG_VERSION");
    if !install::running_as_weave() {
        // A test harness or another binary linking the library. Re-launching it
        // with `--version` would run something else entirely.
        return pass(
            "Weave executable",
            format!("{} (weave {version})", exe.display()),
        );
    }
    match std::process::Command::new(&exe).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if reported.contains(version) {
                pass(
                    "Weave executable",
                    format!("{} ({reported})", exe.display()),
                )
            } else {
                fail(
                    "Weave executable",
                    format!(
                        "{} reports `{reported}`, but this build is {version}.",
                        exe.display()
                    ),
                )
                .with_hint("Reinstall Weave; the installation looks mixed.")
            }
        }
        Ok(out) => fail(
            "Weave executable",
            format!(
                "{} exited with {} when asked for its version.",
                exe.display(),
                out.status
            ),
        )
        .with_hint("Reinstall Weave."),
        Err(e) => fail(
            "Weave executable",
            format!("{} could not be started: {e}", exe.display()),
        )
        .with_hint("Reinstall Weave."),
    }
}

fn check_platform() -> Check {
    let description = install::platform_description();
    if install::supported_platform() {
        pass("Platform", description)
    } else {
        fail(
            "Platform",
            format!("{description} is not a supported Weave platform."),
        )
        .with_hint("Weave supports Windows x86_64, macOS arm64/x86_64 and Linux x86_64/arm64.")
    }
}

/// The bundled runtime dependencies. Two checks: the bundle is where it should
/// be, and the `cloudflared` in it actually runs.
fn check_bundle() -> Vec<Check> {
    let bundle = install::bundle();
    let packaged = bundle.is_some();
    let mut checks = Vec::new();

    match &bundle {
        Some(b) => {
            let weave_version = env!("CARGO_PKG_VERSION");
            if b.weave_version != weave_version {
                checks.push(
                    fail(
                        "Weave package",
                        format!(
                            "{} describes Weave {}, but this executable is {weave_version}.",
                            b.path.display(),
                            b.weave_version
                        ),
                    )
                    .with_hint("Reinstall Weave; the installation is mixed."),
                );
            } else {
                checks.push(pass(
                    "Weave package",
                    format!("{} (weave {weave_version})", b.package),
                ));
            }
        }
        None => checks.push(warn(
            "Weave package",
            "no package manifest; this is a build from source, not an installed package",
        )),
    }

    let found = install::cloudflared();
    let Some(found) = found else {
        let check = if std::env::var_os("WEAVE_CLOUDFLARED").is_some() {
            fail(
                "Bundled cloudflared",
                "WEAVE_CLOUDFLARED points at a file that does not exist.",
            )
            .with_hint("Unset WEAVE_CLOUDFLARED to use the copy bundled with Weave.")
        } else if packaged {
            fail(
                "Bundled cloudflared",
                format!(
                    "no cloudflared in this installation (looked in {}).",
                    install::support_dirs()
                        .iter()
                        .take(3)
                        .map(|d| d.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .with_hint("Reinstall Weave; the package is incomplete.")
        } else {
            warn(
                "Bundled cloudflared",
                "not bundled and none on PATH; remote sessions need it",
            )
            .with_hint("Install a Weave package, or use `weave host --lan`.")
        };
        checks.push(check);
        return checks;
    };

    if packaged && found.source != install::CloudflaredSource::Bundled {
        checks.push(
            warn(
                "Bundled cloudflared",
                format!(
                    "using {} ({}) instead of the bundled copy",
                    found.path.display(),
                    found.source.describe()
                ),
            )
            .with_hint("Unset WEAVE_CLOUDFLARED to use the copy Weave ships."),
        );
    } else {
        checks.push(pass(
            "Bundled cloudflared",
            format!("{} ({})", found.path.display(), found.source.describe()),
        ));
    }

    // Present is not the same as usable: a truncated download, the wrong
    // architecture or a lost executable bit all look fine on disk.
    checks.push(check_cloudflared_runs(&found, bundle.as_ref()));

    if packaged {
        checks.push(check_licenses());
    }
    checks
}

fn check_cloudflared_runs(found: &install::Cloudflared, bundle: Option<&install::Bundle>) -> Check {
    let reported = match run_briefly(&found.path, "--version") {
        Ok(Some(text)) => text,
        Ok(None) => {
            return fail(
                "cloudflared runs",
                format!("{} exited with an error.", found.path.display()),
            )
            .with_hint("Reinstall Weave; the bundled cloudflared is damaged.")
        }
        Err(e) => {
            return fail(
                "cloudflared runs",
                format!("{} could not be started: {e}", found.path.display()),
            )
            .with_hint("Reinstall Weave; the bundled cloudflared is damaged.")
        }
    };

    // The version the package says it shipped must be the version that runs.
    if let Some(bundle) = bundle {
        if found.source == install::CloudflaredSource::Bundled
            && !reported.contains(&bundle.cloudflared_version)
        {
            return fail(
                "cloudflared runs",
                format!(
                    "the package ships cloudflared {}, but the binary reports `{reported}`.",
                    bundle.cloudflared_version
                ),
            )
            .with_hint("Reinstall Weave; the installation is mixed.");
        }
    }
    pass("cloudflared runs", reported)
}

/// The third-party notices a package is required to carry.
fn check_licenses() -> Check {
    match install::cloudflared_notice() {
        Some(path) if install::is_readable(&path) => pass(
            "Third-party licences",
            path.parent().unwrap_or(&path).display().to_string(),
        ),
        Some(path) => fail(
            "Third-party licences",
            format!("{} cannot be read.", path.display()),
        )
        .with_hint("Reinstall Weave."),
        None => fail(
            "Third-party licences",
            "the bundled cloudflared licence and notice are missing.",
        )
        .with_hint("Reinstall Weave; the package is incomplete."),
    }
}

fn check_git() -> Check {
    let out = match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => out,
        _ => {
            return fail("Git", "Git was not found on PATH.")
                .with_hint("Install Git and reopen your terminal.")
        }
    };
    let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match parse_git_version(&reported) {
        Some((major, minor)) if (major, minor) < MIN_GIT => fail(
            "Git",
            format!(
                "{reported} is older than the Git {}.{} Weave needs.",
                MIN_GIT.0, MIN_GIT.1
            ),
        )
        .with_hint("Upgrade Git."),
        Some(_) => pass("Git", reported),
        None => warn("Git", format!("{reported} (version could not be parsed)")),
    }
}

fn check_sqlite() -> Check {
    match rusqlite::Connection::open_in_memory() {
        Ok(conn) => {
            let version: String = conn
                .query_row("SELECT sqlite_version()", [], |r| r.get(0))
                .unwrap_or_else(|_| "unknown".into());
            pass("SQLite", format!("bundled {version}"))
        }
        Err(e) => fail("SQLite", e.to_string()).with_hint("Reinstall Weave."),
    }
}

/// The oldest Git whose plumbing behaves the way Weave relies on.
const MIN_GIT: (u32, u32) = (2, 25);

fn parse_git_version(text: &str) -> Option<(u32, u32)> {
    let numbers = text.split_whitespace().find(|w| {
        w.split('.')
            .next()
            .map(|first| !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
    })?;
    let mut parts = numbers.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()
        .and_then(|p| {
            p.trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()
        })
        .unwrap_or(0);
    Some((major, minor))
}

/// Run a helper binary for one short question. `Ok(None)` means it ran and
/// failed; `Err` means it could not be run at all.
fn run_briefly(program: &Path, arg: &str) -> std::io::Result<Option<String>> {
    use std::process::Stdio;
    let out = std::process::Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    Ok(Some(text.lines().next().unwrap_or_default().to_string()))
}

// ---------------------------------------------------------------------------
// Repository checks
// ---------------------------------------------------------------------------

fn check_repository(start_dir: &Path) -> Vec<Check> {
    let mut checks = Vec::new();

    let paths = match Paths::discover(start_dir) {
        Ok(p) => {
            checks.push(pass("Repository", p.repo_root.display().to_string()));
            p
        }
        Err(e) => {
            checks.push(
                fail("Repository", e.message)
                    .with_hint("Run Weave from inside a Git working tree."),
            );
            return checks;
        }
    };
    let root = paths.repo_root.clone();

    checks.push(check_git_dir(&paths));
    checks.push(check_branch(&root));
    checks.push(check_no_operation(&root));
    checks.push(check_working_tree(&paths));
    checks.extend(check_supported(&root));
    checks.push(check_portable_paths(&root));
    checks.push(check_storage(&paths));
    checks.push(check_disk_space(&paths));
    checks.push(check_working_tree_writable(&paths));
    checks
}

/// Room to work in.
///
/// Weave keeps content twice while a file is live — once in the working tree,
/// once content-addressed in the blob store — and a transfer needs room for a
/// partial on top of that. A session that runs out of space mid-transfer fails
/// safely (nothing partial is ever installed) but repeatedly, so it is worth
/// saying beforehand. Never a hard failure: the number is a snapshot, other
/// programs are using the same disk, and a platform that will not answer must
/// not be read as an empty one.
fn check_disk_space(paths: &Paths) -> Check {
    let Some(free) = crate::util::available_space(&paths.repo_root) else {
        return pass("Disk space", "not reported by this platform");
    };
    let human = crate::util::format_size(free);
    // Three times the default limit: one working copy, one blob, one partial.
    let comfortable = DEFAULT_MAX_FILE_SIZE.saturating_mul(3);
    if free < DEFAULT_MAX_FILE_SIZE {
        return warn(
            "Disk space",
            format!(
                "{human} free, less than the {} a single file may reach",
                crate::util::format_size(DEFAULT_MAX_FILE_SIZE)
            ),
        )
        .with_hint(
            "Free some space, or run the session with a smaller limit: \
             `weave host --max-file-size <size>`.",
        );
    }
    if free < comfortable {
        return warn("Disk space", format!("{human} free"))
            .with_hint("Weave keeps a working copy and a content-addressed copy of every file.");
    }
    pass("Disk space", format!("{human} free"))
}

fn check_git_dir(paths: &Paths) -> Check {
    if paths.git_dir.is_dir() {
        pass("Git directory", paths.git_dir.display().to_string())
    } else {
        fail(
            "Git directory",
            format!("{} is not a usable directory.", paths.git_dir.display()),
        )
        .with_hint("Weave cannot run in a repository whose .git state is broken.")
    }
}

fn check_branch(root: &Path) -> Check {
    match gitx::current_branch(root) {
        Ok(Some(branch)) => pass("Branch", branch),
        Ok(None) => fail("Branch", "HEAD is detached; Weave needs one branch.")
            .with_hint("Run `git switch <branch>`."),
        Err(e) => fail("Branch", e.message).with_hint("Check the repository with `git status`."),
    }
}

fn check_no_operation(root: &Path) -> Check {
    match gitx::operation_in_progress(root) {
        Ok(None) => pass("No Git operation in progress", ""),
        Ok(Some(op)) => fail(
            "No Git operation in progress",
            format!("A Git {op} is in progress."),
        )
        .with_hint("Finish or abort it before starting a Weave session."),
        Err(e) => warn("No Git operation in progress", e.message),
    }
}

/// Working tree cleanliness: a requirement for a *new* session, and merely
/// informational once one is live.
fn check_working_tree(paths: &Paths) -> Check {
    let session_active = crate::session::load_session_record(paths)
        .ok()
        .flatten()
        .is_some();
    match gitx::dirty_entries(&paths.repo_root) {
        Ok(entries) if entries.is_empty() => pass("Working tree clean", ""),
        Ok(entries) if session_active => pass(
            "Working tree",
            format!(
                "{} live change(s) ahead of the last publication",
                entries.len()
            ),
        ),
        Ok(entries) => warn(
            "Working tree clean",
            format!(
                "{} uncommitted change(s). A new Weave session requires a clean tree.",
                entries.len()
            ),
        )
        .with_hint("Commit, stash or discard them before starting a session."),
        Err(e) => warn("Working tree clean", e.message),
    }
}

fn check_supported(root: &Path) -> Vec<Check> {
    match gitx::detect_unsupported(root) {
        Ok(items) if items.is_empty() => vec![
            pass("Supported Git attributes", ""),
            pass("Supported repository features", ""),
        ],
        Ok(items) => {
            let detail = items
                .iter()
                .map(|i| format!("{}: {}", i.feature, i.detail))
                .collect::<Vec<_>>()
                .join("; ");
            vec![fail("Supported repository features", detail)
                .with_hint("Weave V1 refuses these features rather than corrupting them.")]
        }
        Err(e) => vec![warn("Supported repository features", e.message)],
    }
}

/// Path portability across Windows, macOS and Linux participants, including
/// case and Unicode collisions the scanner would otherwise reject one by one.
fn check_portable_paths(root: &Path) -> Check {
    let list = match gitx::list_repository_paths(root) {
        Ok(list) => list,
        Err(e) => return warn("Portable paths", e.message),
    };
    let mut problems = Vec::new();
    let mut collisions: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for raw in &list {
        match RepoPath::new(raw) {
            Ok(path) => {
                let key = path.collision_key();
                if let Some(other) = collisions.insert(key, raw.clone()) {
                    if &other != raw {
                        problems.push(format!("{raw} collides with {other}"));
                    }
                }
            }
            Err(e) => problems.push(format!("{raw}: {}", e.message)),
        }
    }
    if problems.is_empty() {
        pass("Portable paths", format!("{} file(s)", list.len()))
    } else {
        let shown: Vec<String> = problems.iter().take(10).cloned().collect();
        warn("Portable paths", shown.join("; ")).with_hint(
            "These files stay out of the session; everything else synchronizes normally.",
        )
    }
}

fn check_storage(paths: &Paths) -> Check {
    match storage_detail(paths) {
        Ok(detail) => pass("Weave storage", detail),
        Err(e) => {
            fail("Weave storage", e.message).with_hint("Weave needs to write inside .git/weave.")
        }
    }
}

fn storage_detail(paths: &Paths) -> Result<String> {
    paths.ensure()?;
    let probe = paths.weave_dir.join(".weave-write-probe");
    crate::util::write_atomic(&probe, b"weave")?;
    std::fs::remove_file(&probe)?;
    let blobs = crate::blobs::BlobStore::open(paths.blobs())?;
    let (count, bytes) = blobs.stats()?;
    Ok(format!(
        "{} writable, {count} blob(s), {:.1} MiB",
        paths.weave_dir.display(),
        bytes as f64 / (1024.0 * 1024.0)
    ))
}

/// Weave materializes canonical content into the working tree, so a read-only
/// checkout is a hard blocker rather than something to discover mid-session.
fn check_working_tree_writable(paths: &Paths) -> Check {
    let probe = paths
        .repo_root
        .join(format!(".weave-write-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"weave") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            pass(
                "Working tree writable",
                paths.repo_root.display().to_string(),
            )
        }
        Err(e) => {
            let _ = std::fs::remove_file(&probe);
            fail(
                "Working tree writable",
                format!("{} is not writable: {e}", paths.repo_root.display()),
            )
            .with_hint("Weave writes shared content into the working tree.")
        }
    }
}

/// Another daemon already owning this working tree.
fn check_no_other_daemon(paths: &Paths) -> Check {
    match crate::session::DaemonLock::acquire(paths) {
        // Acquired and immediately released: nothing else holds it.
        Ok(lock) => {
            drop(lock);
            pass("No other Weave daemon", "")
        }
        Err(e) => fail("No other Weave daemon", e.message)
            .with_hint("Run `weave status` to inspect it, or `weave stop` to shut it down."),
    }
}

// ---------------------------------------------------------------------------
// The full report
// ---------------------------------------------------------------------------

/// Installation and this repository: what `weave doctor` prints.
pub fn run(start_dir: &Path) -> Report {
    let mut checks = install_report().checks;
    checks.extend(check_repository(start_dir));
    Report::new(Scope::Full, checks)
}

/// This repository only.
pub fn repository_report(start_dir: &Path) -> Report {
    Report::new(Scope::Full, check_repository(start_dir))
}

pub fn print_report(report: &Report) {
    for check in &report.checks {
        let mark = match check.status {
            CheckStatus::Pass => "\u{2713}",
            CheckStatus::Warn => "!",
            CheckStatus::Fail => "\u{2717}",
        };
        if check.detail.is_empty() {
            println!("{mark} {}", check.name);
        } else {
            println!("{mark} {} — {}", check.name, check.detail);
        }
        if check.status != CheckStatus::Pass {
            if let Some(hint) = &check.hint {
                println!("    {hint}");
            }
        }
    }
    println!();
    if report.ready {
        match report.scope {
            Scope::Install => println!("The Weave installation is healthy."),
            Scope::Full => println!("Weave is ready."),
        }
    } else {
        println!("Weave is not ready. Fix the items marked \u{2717} above.");
    }
}

// ---------------------------------------------------------------------------
// Preflight — the subset `weave host` and `weave join` run automatically
// ---------------------------------------------------------------------------

/// What a session is about to do, which decides whether `cloudflared` matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// `weave host`, exposing a Cloudflare Quick Tunnel.
    HostRemote,
    /// `weave host --lan` or `weave host --local`.
    HostLocal,
    /// `weave join`, which never launches `cloudflared`.
    Join,
}

/// The result of a preflight: what blocks the session, and what is merely worth
/// saying. Informational passes are dropped — a successful start stays quiet.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub blockers: Vec<Check>,
    pub warnings: Vec<Check>,
}

impl Preflight {
    pub fn ok(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// Run the checks that must hold before a session starts.
///
/// This is deliberately the *subset* of `weave doctor`, not all of it: it is on
/// the critical path of every `weave host` and `weave join`.
pub fn preflight(start_dir: &Path, intent: Intent) -> Preflight {
    let mut checks = Vec::new();

    // Machine-level, in the order that produces the most useful first failure.
    checks.push(check_git());
    if checks[0].status == CheckStatus::Fail {
        return split(checks);
    }

    // The repository comes before the runtime dependency: someone standing in
    // the wrong directory should be told that, not sent to install cloudflared.
    let paths = match Paths::discover(start_dir) {
        Ok(paths) => paths,
        Err(e) => {
            checks.push(
                fail("Repository", e.message)
                    .with_hint("Run Weave from inside a Git working tree."),
            );
            return split(checks);
        }
    };

    checks.push(check_git_dir(&paths));
    checks.push(check_branch(&paths.repo_root));
    checks.push(check_no_operation(&paths.repo_root));
    checks.extend(check_supported(&paths.repo_root));
    checks.push(check_storage(&paths));
    checks.push(check_working_tree_writable(&paths));
    checks.push(check_no_other_daemon(&paths));
    checks.push(check_portable_paths(&paths.repo_root));
    if intent == Intent::HostRemote {
        checks.push(check_cloudflared_for_host());
    }
    split(checks)
}

/// `weave host` without `--lan`/`--local` launches `cloudflared` immediately,
/// so a missing one is a blocker rather than a surprise ninety seconds in.
fn check_cloudflared_for_host() -> Check {
    match install::cloudflared() {
        Some(found) => pass(
            "cloudflared",
            format!("{} ({})", found.path.display(), found.source.describe()),
        ),
        None => fail("cloudflared", "cloudflared was not found.").with_hint(
            "Install Weave from a package, which bundles it, or start a local-network \
             session with `weave host --lan`.",
        ),
    }
}

fn split(checks: Vec<Check>) -> Preflight {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    for check in checks {
        match check.status {
            CheckStatus::Fail => blockers.push(check),
            CheckStatus::Warn => warnings.push(check),
            CheckStatus::Pass => {}
        }
    }
    Preflight { blockers, warnings }
}

/// Preflight, then either continue quietly or refuse with one actionable line.
///
/// Warnings are printed on stderr and do not stop anything; the first blocker
/// becomes the error. Nothing is printed when everything passes.
pub fn ensure_ready(start_dir: &Path, intent: Intent) -> Result<()> {
    let report = preflight(start_dir, intent);
    for warning in &report.warnings {
        eprintln!("! {} — {}", warning.name, warning.detail);
        if let Some(hint) = &warning.hint {
            eprintln!("  {hint}");
        }
    }
    let Some(blocker) = report.blockers.first() else {
        return Ok(());
    };
    let mut detail = String::new();
    if let Some(hint) = &blocker.hint {
        detail.push_str(hint);
        detail.push_str("\n\n");
    }
    detail.push_str("Run `weave doctor` for full diagnostics.");
    Err(crate::error::repository(format!(
        "Weave cannot start: {}",
        as_clause(&blocker.detail)
    ))
    .with_detail(detail))
}

/// Fold a check's standalone sentence into the middle of another one.
///
/// Checks phrase their detail as a sentence of its own ("This directory is not
/// inside a Git repository."), which reads badly after a colon. Only a leading
/// determiner is lowered — "Git was not found on PATH." and "SQLite is
/// unusable." must keep their capital.
fn as_clause(detail: &str) -> String {
    const DETERMINERS: [&str; 5] = ["This", "That", "The", "A", "An"];
    match detail.split_once(' ') {
        Some((first, rest)) if DETERMINERS.contains(&first) => {
            format!("{} {rest}", first.to_lowercase())
        }
        _ => detail.to_string(),
    }
}

/// The error `weave doctor` itself returns when the machine or the repository
/// is not ready, so its exit code stays meaningful.
pub fn not_ready_error(report: &Report) -> WeaveError {
    let summary = match report.scope {
        Scope::Install => "The Weave installation is not healthy.",
        Scope::Full => "Weave is not ready in this repository.",
    };
    let detail = match report.first_failure() {
        Some(check) => format!("{} — {}", check.name, check.detail),
        None => "See the checklist above.".to_string(),
    };
    crate::error::repository(summary).with_detail(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_versions_parse() {
        assert_eq!(parse_git_version("git version 2.51.0"), Some((2, 51)));
        assert_eq!(
            parse_git_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39))
        );
        assert_eq!(
            parse_git_version("git version 2.47.1.windows.2"),
            Some((2, 47))
        );
        assert_eq!(parse_git_version("git version broken"), None);
    }

    #[test]
    fn only_a_leading_determiner_is_lowered() {
        assert_eq!(
            as_clause("This directory is not inside a Git repository."),
            "this directory is not inside a Git repository."
        );
        assert_eq!(
            as_clause("The .git directory is unusable."),
            "the .git directory is unusable."
        );
        assert_eq!(
            as_clause("Git was not found on PATH."),
            "Git was not found on PATH."
        );
        assert_eq!(as_clause("SQLite is unusable."), "SQLite is unusable.");
        assert_eq!(as_clause(""), "");
    }

    #[test]
    fn a_report_is_ready_only_without_failures() {
        let ready = Report::new(Scope::Install, vec![pass("a", ""), warn("b", "")]);
        assert!(ready.ready);
        assert!(ready.first_failure().is_none());

        let broken = Report::new(Scope::Install, vec![pass("a", ""), fail("b", "boom")]);
        assert!(!broken.ready);
        assert_eq!(broken.first_failure().map(|c| c.name.as_str()), Some("b"));
    }

    #[test]
    fn preflight_reports_warnings_separately_from_blockers() {
        let split = split(vec![
            pass("fine", ""),
            warn("noisy", "worth saying"),
            fail("broken", "stop"),
        ]);
        assert_eq!(split.blockers.len(), 1);
        assert_eq!(split.warnings.len(), 1);
        assert!(!split.ok());
    }

    #[test]
    fn the_installation_report_needs_no_repository() {
        // Runs from the test harness's own directory, which is not a Git
        // repository the way a user's checkout is.
        let report = install_report();
        assert_eq!(report.scope, Scope::Install);
        assert!(report.checks.iter().any(|c| c.name == "Platform"));
        assert!(report.checks.iter().any(|c| c.name == "Git"));
    }
}
