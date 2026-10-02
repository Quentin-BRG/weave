// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. https://mozilla.org/MPL/2.0/.
//! Ordered Git state and crash-resumable external adoption, without invented publications.
use crate::{
    blobs::BlobStore,
    db,
    error::Result,
    gitx,
    model::*,
    path::RepoPath,
    reconcile::{reconcile, MergeContext, Reconciled},
    session::Paths,
    store_host::HostStore,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitState {
    pub sequence: u64,
    pub branch: String,
    pub commit: String,
    pub tree: String,
    pub origin: String,
    pub revision: u64,
}

pub fn current(store: &HostStore, paths: &Paths) -> Result<GitState> {
    if let Some(state) = db::get_json::<GitState>(store.conn(), "git_state")? {
        if store
            .latest_publication()?
            .is_none_or(|p| p.sequence <= state.sequence)
        {
            return Ok(state);
        }
    }
    let session = store
        .session()?
        .ok_or_else(|| crate::error::integrity("Session metadata is missing."))?;
    let publication = store.latest_publication()?;
    let commit = publication
        .as_ref()
        .map(|p| p.descriptor.commit_oid.clone())
        .unwrap_or(session.base_commit);
    let state = GitState {
        tree: gitx::rev_parse(&paths.repo_root, &format!("{commit}^{{tree}}"))?
            .ok_or_else(|| crate::error::integrity("The session Git tree is missing."))?,
        commit,
        branch: session.branch,
        sequence: publication.as_ref().map(|p| p.sequence).unwrap_or(0),
        origin: if publication.is_some() {
            "weave"
        } else {
            "initial"
        }
        .into(),
        revision: publication
            .as_ref()
            .map(|p| p.descriptor.target_revision)
            .unwrap_or(0),
    };
    save(store, &state)?;
    Ok(state)
}

pub fn save(store: &HostStore, state: &GitState) -> Result<()> {
    store.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS git_states (sequence INTEGER PRIMARY KEY, data TEXT NOT NULL)",
    )?;
    let tx = store.conn().unchecked_transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO git_states(sequence,data) VALUES (?1,?2)",
        rusqlite::params![state.sequence as i64, serde_json::to_string(state)?],
    )?;
    db::set_json(&tx, "git_state", state)?;
    db::set_u64(
        &tx,
        "publication_sequence",
        state
            .sequence
            .max(db::get_u64(&tx, "publication_sequence", 0)?),
    )?;
    tx.commit()?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Change {
    id: Uuid,
    path: RepoPath,
    before: Option<FileEntry>,
    after: Option<FileEntry>,
}
#[derive(Serialize, Deserialize)]
struct Adoption {
    state: GitState,
    changes: Vec<Change>,
    conflicts: Vec<Conflict>,
    disk: BTreeMap<RepoPath, FileEntry>,
    actor: Uuid,
}

/// Called under the daemon lock before either engine or any listener starts.
pub fn adopt(
    paths: &Paths,
    store: &mut HostStore,
    blobs: &BlobStore,
    head: &str,
    include_local: bool,
) -> Result<()> {
    let mut plan: Option<Adoption> = db::get_json(store.conn(), "pending_adoption")?;
    if plan.is_none() {
        let old = current(store, paths)?;
        if old.commit == head && !include_local {
            return Ok(());
        }
        let session = store.session()?.unwrap();
        if gitx::current_branch(&paths.repo_root)?.as_deref() != Some(session.branch.as_str()) {
            return Err(crate::error::repository(
                "The branch changed. Leave this session before starting one on another branch.",
            ));
        }
        let backup = crate::backup::archive_session(paths, "external-git-adoption")?;
        crate::backup::capture_worktree(paths, &backup)?;
        gitx::protect_commit(&paths.repo_root, &old.commit, &backup)?;
        let base = gitx::committed_manifest(&paths.repo_root, &old.commit, blobs)?;
        let next = if old.commit == head {
            base.clone()
        } else {
            gitx::committed_manifest(&paths.repo_root, head, blobs)?
        };
        let canonical = store.manifest_all()?;
        let max_file_size = store.max_file_size()?;
        let scan = crate::scan::scan_repository(
            &paths.repo_root,
            &canonical,
            blobs,
            &mut crate::scan::ScanCache::new(),
            max_file_size,
        )?;
        if !scan.rejected.is_empty() {
            return Err(crate::error::repository(
                "Some local files cannot be captured before Git adoption.",
            )
            .with_detail(format!("{:?}", scan.rejected)));
        }
        let too_large: Vec<_> = scan
            .oversize
            .iter()
            .map(|(path, size)| format!("{path}: {size} bytes"))
            .chain(
                next.iter()
                    .filter(|(_, entry)| entry.size > max_file_size)
                    .map(|(path, entry)| format!("{path}: {} bytes in Git", entry.size)),
            )
            .collect();
        if !too_large.is_empty() {
            return Err(crate::error::repository(
                "Git adoption needs a larger session file size limit; no files were replaced.",
            )
            .with_detail(too_large.join("\n")));
        }
        let disk = scan.entries;
        let paths_all: BTreeSet<_> = base
            .keys()
            .chain(next.keys())
            .chain(canonical.keys())
            .chain(disk.keys())
            .cloned()
            .collect();
        let ctx = MergeContext::new(&paths.repo_root, paths.scratch(), blobs);
        let mut changes = Vec::new();
        let mut conflicts = Vec::new();
        for path in paths_all {
            let b = base.get(&path);
            let n = next.get(&path);
            let c = canonical.get(&path);
            let d = disk.get(&path);
            let mut desired = n.cloned();
            let mut conflict = None;
            match reconcile(&ctx, b, n, c)? {
                Reconciled::Converged => {}
                Reconciled::Accept { entry, .. } => desired = entry,
                Reconciled::Conflict(kind) => conflict = Some((kind, b.cloned(), c.cloned())),
            }
            // Git may have carried the canonical edits in the working tree.
            // Identical carried edits are not an additional local change.
            if !FileEntry::same_as(d, n) && !FileEntry::same_as(d, c) {
                match reconcile(&ctx, n, desired.as_ref(), d)? {
                    Reconciled::Converged => {}
                    Reconciled::Accept { entry, .. } => desired = entry,
                    Reconciled::Conflict(kind) => {
                        conflict = Some((kind, n.cloned(), desired.clone()));
                        desired = n.cloned();
                    }
                }
            }
            if let Some((kind, base_entry, incoming_entry)) = conflict {
                desired = n.cloned();
                conflicts.push(Conflict {
                    id: Uuid::new_v4(),
                    path: path.clone(),
                    kind,
                    base_entry,
                    canonical_entry: desired.clone(),
                    incoming_entry,
                    latest_local_candidate: d.cloned(),
                    incoming_actor_id: session.host_actor_id,
                    incoming_task_id: None,
                    canonical_revision: store.current_revision()?,
                    created_at_ms: crate::util::now_ms(),
                    status: ConflictStatus::Open,
                    resolved_revision: None,
                });
            }
            if !FileEntry::same_as(c, desired.as_ref()) {
                changes.push(Change {
                    id: Uuid::new_v4(),
                    path,
                    before: c.cloned(),
                    after: desired,
                });
            }
        }
        // Each input may be portable on its own while the merged result has a
        // case collision or both a file and one of its descendants.
        let mut merged = canonical.clone();
        for change in &changes {
            match &change.after {
                Some(entry) => {
                    merged.insert(change.path.clone(), entry.clone());
                }
                None => {
                    merged.remove(&change.path);
                }
            }
        }
        let mut portable = BTreeMap::new();
        for path in merged.keys() {
            if let Some(other) = portable.insert(path.collision_key(), path) {
                return Err(crate::error::repository(format!(
                    "Git adoption has a portable path collision: {other} and {path}. Saved work is available through `weave recover`."
                )));
            }
        }
        for path in merged.keys() {
            for parent in path.parent_dirs() {
                if let Some(other) = portable.get(&crate::path::collision_key_of(&parent)) {
                    return Err(crate::error::repository(format!(
                        "Git adoption has a file/directory collision: {other} and {path}. Saved work is available through `weave recover`."
                    )));
                }
            }
        }
        let state = GitState {
            sequence: if old.commit == head {
                old.sequence
            } else {
                store.next_publication_sequence()?
            },
            branch: old.branch,
            commit: head.into(),
            tree: gitx::rev_parse(&paths.repo_root, &format!("{head}^{{tree}}"))?.unwrap(),
            origin: if old.commit == head {
                old.origin
            } else {
                "external".into()
            },
            revision: store.current_revision()?,
        };
        let value = Adoption {
            state,
            changes,
            conflicts,
            disk,
            actor: session.host_actor_id,
        };
        db::set_json(store.conn(), "pending_adoption", &value)?;
        plan = Some(value);
    }
    let plan = plan.unwrap();
    if plan.state.commit != head {
        return Err(crate::error::repository("An interrupted Git adoption must finish before another Git change. Recover the saved adoption first."));
    }
    for change in &plan.changes {
        if store.lookup_operation(&change.id)?.is_some() {
            continue;
        }
        store.commit_revision(
            &change.id,
            &plan.actor,
            None,
            "git-adoption",
            &change.path,
            change.before.as_ref(),
            change.after.as_ref(),
            |revision| OperationOutcome::Accepted {
                revision,
                canonical_entry: change.after.clone(),
            },
        )?;
    }
    for conflict in &plan.conflicts {
        store.put_conflict(conflict)?;
    }
    // Seed only what was actually on disk. Pending operations keep their IDs
    // and historical bases, and will be reconciled by the ordinary host path.
    let replica = crate::store_client::ClientStore::open(&paths.client_db())?;
    let known: BTreeSet<_> = replica
        .all_paths()?
        .into_iter()
        .chain(plan.disk.keys().cloned())
        .collect();
    for path in known {
        let mut state = replica.path_state(&path)?;
        state.materialized = plan.disk.get(&path).cloned();
        if let Some(conflict) = plan.conflicts.iter().find(|c| c.path == path) {
            state.conflict_draft = Some(crate::store_client::ConflictDraft {
                conflict_id: conflict.id,
                entry: plan.disk.get(&path).cloned(),
                local_seq: replica.next_local_seq()?,
            });
        }
        replica.put_path_state(&path, &state)?;
    }
    gitx::read_tree_into_index(&paths.repo_root, &plan.state.tree)?;
    save(store, &plan.state)?;
    store.conn().execute("DELETE FROM preparations", [])?;
    store.bump_control_version()?;
    store
        .conn()
        .execute("DELETE FROM meta_v4 WHERE key = 'pending_adoption'", [])?;
    Ok(())
}
