// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `weave recover` (specification section 146).
//!
//! Diagnostics first, repair second, destruction never: recovery always
//! prefers preserving data over automatic destructive repair.

use crate::blobs::BlobStore;
use crate::error::Result;
use crate::session::{load_session_record, Paths};
use crate::store_client::ClientStore;
use crate::store_host::HostStore;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct RecoverReport {
    pub role: Option<String>,
    pub findings: Vec<String>,
    pub repairs: Vec<String>,
    pub export_directory: Option<String>,
    pub healthy: bool,
}

pub struct RecoverOptions {
    /// Rebuild the derived canonical manifest from durable revision history.
    pub rebuild: bool,
    /// Export the latest recoverable canonical files to a directory.
    pub export: Option<PathBuf>,
}

pub fn run(start_dir: &Path, opts: RecoverOptions) -> Result<RecoverReport> {
    let paths = Paths::discover(start_dir)?;
    paths.ensure()?;
    let record = load_session_record(&paths)?;
    let mut findings = Vec::new();
    let mut repairs = Vec::new();
    let blobs = BlobStore::open(paths.blobs())?;

    // ---- participant replica ----
    if paths.client_db().exists() {
        let store = ClientStore::open(&paths.client_db())?;
        let problems = store.integrity_check()?;
        if problems.is_empty() {
            findings.push("Replica database integrity: ok".into());
        } else {
            findings.push(format!(
                "Replica database problems: {}",
                problems.join("; ")
            ));
        }
        let incomplete = store.incomplete_publications()?;
        if incomplete.is_empty() {
            findings.push("No interrupted Git publication on this machine".into());
        } else {
            for (publication, stage) in &incomplete {
                findings.push(format!(
                    "Interrupted Git publication {} at stage {}. Run `weave resume` to finish it.",
                    crate::util::short_oid(&publication.descriptor.commit_oid),
                    stage.as_str()
                ));
            }
        }
        let mut missing_local = 0;
        for (_, state) in store.all_states()? {
            for entry in [&state.confirmed, &state.materialized]
                .into_iter()
                .flatten()
            {
                if !blobs.has(&entry.blob_hash) {
                    missing_local += 1;
                }
            }
            if let Some(draft) = &state.conflict_draft {
                if let Some(entry) = &draft.entry {
                    if !blobs.has(&entry.blob_hash) {
                        missing_local += 1;
                    }
                }
            }
        }
        if missing_local == 0 {
            findings.push("Replica blob references: ok".into());
        } else {
            findings.push(format!(
                "{missing_local} replica blob reference(s) are missing; the host copy remains \
                 authoritative and will be re-fetched on reconnect."
            ));
        }
        let pending: usize = store
            .all_states()?
            .values()
            .filter(|s| s.has_local_work())
            .count();
        findings.push(format!(
            "Outbox: {pending} path(s) with unconfirmed local work"
        ));
    }

    // ---- host canonical state ----
    let mut export_directory = None;
    if paths.host_db().exists() {
        let mut store = HostStore::open(&paths.host_db())?;
        let problems = store.integrity_check()?;
        if problems.is_empty() {
            findings.push("Canonical database integrity: ok".into());
        } else {
            findings.push(format!(
                "Canonical database problems: {}",
                problems.join("; ")
            ));
        }
        let missing = store.verify_blob_references(&blobs)?;
        if missing.is_empty() {
            findings.push("Canonical blob references: ok".into());
        } else {
            findings.push(format!(
                "IntegrityError: {} canonical blob reference(s) are missing",
                missing.len()
            ));
            for line in missing.iter().take(10) {
                findings.push(format!("  {line}"));
            }
        }
        findings.push(format!(
            "Canonical revision: {}",
            crate::util::fmt_revision(store.current_revision()?)
        ));
        findings.push(format!("Canonical files: {}", store.manifest_len()?));
        findings.push(format!("Publications: {}", store.publication_count()?));
        findings.push(format!("Open conflicts: {}", store.open_conflicts()?.len()));

        if opts.rebuild {
            let count = store.rebuild_manifest()?;
            repairs.push(format!(
                "Rebuilt the canonical manifest from durable revision history ({count} file(s))."
            ));
        }

        if let Some(dir) = &opts.export {
            std::fs::create_dir_all(dir)?;
            let manifest = store.manifest_all()?;
            let mut exported = 0usize;
            let mut skipped = 0usize;
            for (path, entry) in &manifest {
                match blobs.get(&entry.blob_hash) {
                    Ok(bytes) => {
                        let out = path.to_fs_path(dir);
                        if let Some(parent) = out.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        crate::util::write_atomic(&out, &bytes)?;
                        exported += 1;
                    }
                    Err(_) => skipped += 1,
                }
            }
            repairs.push(format!(
                "Exported {exported} canonical file(s) to {} ({skipped} unavailable).",
                dir.display()
            ));
            export_directory = Some(dir.display().to_string());
        }
    }

    let (count, bytes) = blobs.stats()?;
    findings.push(format!(
        "Blob store: {count} object(s), {:.1} MiB",
        bytes as f64 / (1024.0 * 1024.0)
    ));

    let healthy = !findings.iter().any(|f| {
        f.contains("IntegrityError") || f.contains("problems:") || f.contains("Interrupted")
    });

    Ok(RecoverReport {
        role: record.map(|r| r.role.as_str().to_string()),
        findings,
        repairs,
        export_directory,
        healthy,
    })
}

pub fn print_report(report: &RecoverReport) {
    if let Some(role) = &report.role {
        println!("Session role: {role}");
        println!();
    }
    for line in &report.findings {
        println!("{line}");
    }
    if !report.repairs.is_empty() {
        println!();
        for line in &report.repairs {
            println!("{line}");
        }
    }
    println!();
    if report.healthy {
        println!("No integrity problems found.");
    } else {
        println!("Weave found problems above. No data was discarded.");
        println!("Use `weave recover --export <dir>` to copy the latest recoverable canonical");
        println!("files to a safe directory before taking any further action.");
    }
}
