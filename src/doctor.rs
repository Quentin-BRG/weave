// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `weave doctor` (specification section 155).

use crate::error::Result;
use crate::gitx;
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

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    pub ready: bool,
}

fn pass(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Pass,
        detail: detail.into(),
    }
}
fn warn(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Warn,
        detail: detail.into(),
    }
}
fn fail(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Fail,
        detail: detail.into(),
    }
}

pub fn run(start_dir: &Path) -> Report {
    let mut checks = Vec::new();

    // Git executable.
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => checks.push(pass(
            "Git",
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        )),
        _ => {
            checks.push(fail("Git", "The `git` executable was not found on PATH."));
            let ready = false;
            return Report { checks, ready };
        }
    }

    let paths = match Paths::discover(start_dir) {
        Ok(p) => {
            checks.push(pass("Repository", p.repo_root.display().to_string()));
            p
        }
        Err(e) => {
            checks.push(fail("Repository", e.message));
            return Report {
                checks,
                ready: false,
            };
        }
    };
    let root = paths.repo_root.clone();

    // Branch and Git operations in progress.
    match gitx::current_branch(&root) {
        Ok(Some(branch)) => checks.push(pass("Branch", branch)),
        Ok(None) => checks.push(fail("Branch", "HEAD is detached; Weave needs one branch.")),
        Err(e) => checks.push(fail("Branch", e.message)),
    }
    match gitx::operation_in_progress(&root) {
        Ok(None) => checks.push(pass("No Git operation in progress", "")),
        Ok(Some(op)) => checks.push(fail(
            "No Git operation in progress",
            format!("A Git {op} is in progress."),
        )),
        Err(e) => checks.push(warn("No Git operation in progress", e.message)),
    }

    // Working tree cleanliness (informational once a session is live).
    let session_active = crate::session::load_session_record(&paths)
        .ok()
        .flatten()
        .is_some();
    match gitx::dirty_entries(&root) {
        Ok(entries) if entries.is_empty() => checks.push(pass("Working tree clean", "")),
        Ok(entries) if session_active => checks.push(pass(
            "Working tree",
            format!(
                "{} live change(s) ahead of the last publication",
                entries.len()
            ),
        )),
        Ok(entries) => checks.push(warn(
            "Working tree clean",
            format!(
                "{} uncommitted change(s). A new Weave session requires a clean tree.",
                entries.len()
            ),
        )),
        Err(e) => checks.push(warn("Working tree clean", e.message)),
    }

    // Unsupported repository features and Git filters.
    match gitx::detect_unsupported(&root) {
        Ok(items) if items.is_empty() => {
            checks.push(pass("Supported Git attributes", ""));
            checks.push(pass("Supported repository features", ""));
        }
        Ok(items) => {
            let detail = items
                .iter()
                .map(|i| format!("{}: {}", i.feature, i.detail))
                .collect::<Vec<_>>()
                .join("; ");
            checks.push(fail("Supported repository features", detail));
        }
        Err(e) => checks.push(warn("Supported repository features", e.message)),
    }

    // Portable path policy across the whole repository.
    match gitx::list_repository_paths(&root) {
        Ok(list) => {
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
                checks.push(pass("Portable paths", format!("{} file(s)", list.len())));
            } else {
                let shown: Vec<String> = problems.iter().take(10).cloned().collect();
                checks.push(fail("Portable paths", shown.join("; ")));
            }
        }
        Err(e) => checks.push(warn("Portable paths", e.message)),
    }

    // Weave storage.
    match check_storage(&paths) {
        Ok(detail) => checks.push(pass("Weave storage", detail)),
        Err(e) => checks.push(fail("Weave storage", e.message)),
    }

    // SQLite.
    match rusqlite::Connection::open_in_memory() {
        Ok(conn) => {
            let version: String = conn
                .query_row("SELECT sqlite_version()", [], |r| r.get(0))
                .unwrap_or_else(|_| "unknown".into());
            checks.push(pass("SQLite", format!("bundled {version}")));
        }
        Err(e) => checks.push(fail("SQLite", e.to_string())),
    }

    // cloudflared.
    if crate::tunnel::cloudflared_available() {
        checks.push(pass("cloudflared", "found on PATH"));
    } else {
        checks.push(warn(
            "cloudflared",
            "not found; remote sessions need it. Use `weave host --lan` without it.",
        ));
    }

    let ready = !checks.iter().any(|c| c.status == CheckStatus::Fail);
    Report { checks, ready }
}

fn check_storage(paths: &Paths) -> Result<String> {
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
    }
    println!();
    if report.ready {
        println!("Weave is ready.");
    } else {
        println!("Weave is not ready. Fix the items marked \u{2717} above.");
    }
}
