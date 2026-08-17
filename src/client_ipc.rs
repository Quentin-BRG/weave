// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI command handling inside the replica engine.
//!
//! Every command an agent may drive is answered here with stable machine
//! readable data (specification section 152). Commands that need the host are
//! forwarded and their reply is delivered when the host answers.

use super::*;
use crate::ipc::ResolveSource;

type Reply = tokio::sync::oneshot::Sender<IpcResponse>;

impl ClientEngine {
    pub(crate) fn on_ipc(&mut self, command: IpcCommand, reply: Reply) {
        let result = self.dispatch(command, reply);
        if let Err((e, reply)) = result {
            let _ = reply.send(IpcResponse::error(&e));
        }
    }

    #[allow(clippy::result_large_err)]
    fn dispatch(
        &mut self,
        command: IpcCommand,
        reply: Reply,
    ) -> std::result::Result<(), (WeaveError, Reply)> {
        macro_rules! attempt {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => return Err((e, reply)),
                }
            };
        }

        match command {
            IpcCommand::Status => {
                let value = attempt!(self.status_json());
                let _ = reply.send(IpcResponse::ok(value));
            }
            IpcCommand::Peers => {
                let _ = reply.send(IpcResponse::ok(serde_json::json!({
                    "peers": self.peers,
                })));
            }
            IpcCommand::Rescan => {
                attempt!(self.full_rescan());
                let _ = reply.send(IpcResponse::ok(serde_json::json!({
                    "rescanned": true,
                    "rejected_paths": self.rejected_paths,
                })));
            }
            IpcCommand::TaskList => {
                let _ = reply.send(IpcResponse::ok(serde_json::json!({
                    "tasks": self.tasks(),
                    "active_task_id": self.my_active_task().map(|t| t.id),
                })));
            }
            IpcCommand::TaskShow { id } => {
                let task = attempt!(self.find_task(&id));
                let overlaps = self.overlapping_tasks(&task);
                let _ = reply.send(IpcResponse::ok(serde_json::json!({
                    "task": task,
                    "overlaps": overlaps,
                })));
            }
            IpcCommand::TaskStart {
                description,
                scopes,
            } => {
                attempt!(self.require_connection());
                if description.trim().is_empty() {
                    return Err((
                        crate::error::usage("A Task description is required.")
                            .with_detail("Pass --description \"what you intend to change\"."),
                        reply,
                    ));
                }
                let scopes = attempt!(parse_scopes(&scopes));
                let task_id = Uuid::new_v4();
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                self.send(ClientMessage::TaskStart {
                    request_id,
                    task_id,
                    description,
                    scopes,
                });
            }
            IpcCommand::TaskUpdate {
                id,
                description,
                scopes,
            } => {
                attempt!(self.require_connection());
                let task = attempt!(self.find_task(&id));
                let scopes = match scopes {
                    Some(raw) => Some(attempt!(parse_scopes(&raw))),
                    None => None,
                };
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                self.send(ClientMessage::TaskUpdate {
                    request_id,
                    task_id: task.id,
                    description,
                    scopes,
                });
            }
            IpcCommand::TaskComplete { id } => {
                attempt!(self.require_connection());
                let task = attempt!(self.find_task(&id));
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                self.send(ClientMessage::TaskComplete {
                    request_id,
                    task_id: task.id,
                });
            }
            IpcCommand::TaskCancel { id } => {
                attempt!(self.require_connection());
                let task = attempt!(self.find_task(&id));
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                self.send(ClientMessage::TaskCancel {
                    request_id,
                    task_id: task.id,
                });
            }
            IpcCommand::ConflictList => {
                let conflicts = self.conflicts();
                let open: Vec<&Conflict> = conflicts
                    .iter()
                    .filter(|c| c.status == ConflictStatus::Open)
                    .collect();
                let _ = reply.send(IpcResponse::ok(serde_json::json!({
                    "conflicts": conflicts,
                    "open_count": open.len(),
                })));
            }
            IpcCommand::ConflictShow { id } => {
                let conflict = attempt!(self.find_conflict(&id));
                let value = attempt!(self.conflict_detail(&conflict));
                let _ = reply.send(IpcResponse::ok(value));
            }
            IpcCommand::ConflictDismiss { id } => {
                attempt!(self.require_connection());
                let conflict = attempt!(self.find_conflict(&id));
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                self.send(ClientMessage::DismissConflict {
                    request_id,
                    conflict_id: conflict.id,
                });
            }
            IpcCommand::ConflictResolve {
                id,
                source,
                content_file,
            } => {
                attempt!(self.require_connection());
                let conflict = attempt!(self.find_conflict(&id));
                if conflict.status != ConflictStatus::Open {
                    return Err((
                        crate::error::usage(format!(
                            "Conflict {} is already {}.",
                            conflict.short_id(),
                            conflict.status.as_str()
                        )),
                        reply,
                    ));
                }
                let resolved = attempt!(self.resolution_entry(&conflict, source, content_file));
                let state = attempt!(self.store.path_state(&conflict.path));
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                // The resolved content reaches the host before the resolution
                // that names it, exactly as an ordinary operation does.
                let needs: Vec<String> = resolved
                    .iter()
                    .map(|entry| entry.blob_hash.clone())
                    .collect();
                let emitted = self.traffic.send_when_uploaded(
                    request_id,
                    needs,
                    ClientMessage::ResolveConflict {
                        request_id,
                        conflict_id: conflict.id,
                        operation_id: Uuid::new_v4(),
                        expected_canonical: state.confirmed.clone(),
                        resolved_entry: resolved,
                    },
                );
                if let Err(e) = self.emit(emitted) {
                    tracing::error!("conflict resolution: {}", e.message);
                }
            }
            IpcCommand::CommitPrepare { allow_active_tasks } => {
                attempt!(self.require_connection());
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                self.send(ClientMessage::CommitPrepare {
                    request_id,
                    allow_active_tasks,
                });
            }
            IpcCommand::CommitCreate {
                prepare_id,
                message,
            } => {
                attempt!(self.require_connection());
                let prepare_id =
                    match Uuid::parse_str(prepare_id.trim()) {
                        Ok(id) => id,
                        Err(_) => return Err((
                            crate::error::usage(format!(
                                "`{prepare_id}` is not a Weave preparation identifier."
                            ))
                            .with_detail(
                                "Use the `prepare_id` reported by `weave commit prepare --json`.",
                            ),
                            reply,
                        )),
                    };
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                self.send(ClientMessage::CommitCreate {
                    request_id,
                    prepare_id,
                    message,
                });
            }
            IpcCommand::Push => {
                attempt!(self.require_connection());
                let request_id = Uuid::new_v4();
                self.requests.insert(request_id, PendingRequest { reply });
                self.send(ClientMessage::PushRequest { request_id });
            }
            IpcCommand::Invite
            | IpcCommand::TunnelRestart
            | IpcCommand::Stop
            | IpcCommand::Leave => {
                // Handled by the daemon before reaching the engine.
                let _ = reply.send(IpcResponse::error(&crate::error::usage(
                    "That command is not available in this session role.",
                )));
            }
        }
        Ok(())
    }

    // ----------------------------------------------------------------- status

    pub(crate) fn status_json(&mut self) -> Result<serde_json::Value> {
        let live_revision = self.store.last_applied_revision()?;
        let manifest = self.store.replica_manifest()?;
        let state_hash_value = state_hash(manifest.iter());

        let mut outbox_pending = 0usize;
        let mut conflict_drafts = Vec::new();
        for (path, state) in self.store.all_states()? {
            if state.has_local_work() {
                outbox_pending += 1;
            }
            if state.conflict_draft.is_some() {
                conflict_drafts.push(path.to_string());
            }
        }

        let publication = self.control.as_ref().and_then(|c| c.publication.clone());
        let published_revision = publication
            .as_ref()
            .map(|p| p.descriptor.target_revision)
            .unwrap_or(0);

        let open_conflicts: Vec<&Conflict> = self
            .control
            .as_ref()
            .map(|c| {
                c.conflicts
                    .iter()
                    .filter(|x| x.status == ConflictStatus::Open)
                    .collect()
            })
            .unwrap_or_default();

        let sync_state = if !self.local_state.is_live() {
            self.local_state.clone()
        } else {
            self.host_state.clone()
        };

        Ok(serde_json::json!({
            "active": true,
            "repository": self.paths.repo_name(),
            "role": self.role.as_str(),
            "actor_id": self.actor_id,
            "display_name": self.display_name,
            "session_id": self.session.session_id,
            "host": self.session.host_display_name,
            "branch": self.branch,
            "base_commit": self.session.base_commit,
            "git_publication": publication.as_ref().map(|p| serde_json::json!({
                "commit": p.descriptor.commit_oid,
                "short_commit": crate::util::short_oid(&p.descriptor.commit_oid),
                "revision": p.descriptor.target_revision,
                "push_status": p.push_status.as_str(),
                "push_error": p.push_error,
                "sequence": p.sequence,
            })),
            "published_revision": published_revision,
            "live_revision": live_revision,
            "revisions_ahead": live_revision.saturating_sub(published_revision),
            "state": state_hash_value,
            "connection": if self.connected { "online" } else { "offline" },
            "connection_note": self.connection_note,
            "sync_state": sync_state,
            "participants": self.peers,
            "active_task": self.my_active_task(),
            "tasks_active": self.tasks().iter().filter(|t| t.status == TaskStatus::Active).count(),
            "outbox_pending": outbox_pending,
            "conflicts_open": open_conflicts.len(),
            "conflict_drafts": conflict_drafts,
            "rejected_paths": self.rejected_paths,
            "notices": self.notices,
            "file_count": manifest.len(),
        }))
    }

    fn overlapping_tasks(&self, task: &Task) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for other in self.tasks() {
            if other.id == task.id || other.status != TaskStatus::Active {
                continue;
            }
            let overlapping: Vec<String> = other
                .scopes
                .iter()
                .filter(|s| task.scopes.iter().any(|mine| mine.overlaps(s)))
                .map(|s| s.display())
                .collect();
            if overlapping.is_empty() {
                continue;
            }
            out.push(serde_json::json!({
                "task_id": other.id,
                "short_id": other.short_id(),
                "description": other.description,
                "actor_id": other.actor_id,
                "scopes": overlapping,
            }));
        }
        out
    }

    // -------------------------------------------------------------- conflicts

    fn conflict_detail(&mut self, conflict: &Conflict) -> Result<serde_json::Value> {
        let dir = self.paths.conflicts().join(conflict.short_id());
        std::fs::create_dir_all(&dir)?;
        let mut files = serde_json::Map::new();
        let mut previews = serde_json::Map::new();

        let candidates: [(&str, &Option<FileEntry>); 4] = [
            ("base", &conflict.base_entry),
            ("canonical", &conflict.canonical_entry),
            ("incoming", &conflict.incoming_entry),
            ("local", &conflict.latest_local_candidate),
        ];
        for (label, entry) in candidates {
            let Some(entry) = entry else {
                previews.insert(label.into(), serde_json::Value::Null);
                continue;
            };
            let Ok(bytes) = self.blobs.get(&entry.blob_hash) else {
                previews.insert(label.into(), serde_json::Value::Null);
                continue;
            };
            let name = format!(
                "{label}-{}",
                conflict.path.as_str().rsplit('/').next().unwrap_or("file")
            );
            let out_path = dir.join(&name);
            crate::util::write_atomic(&out_path, &bytes)?;
            files.insert(
                label.into(),
                serde_json::Value::String(out_path.display().to_string()),
            );
            if entry.file_kind == FileKind::Text && entry.size <= 256 * 1024 {
                previews.insert(
                    label.into(),
                    serde_json::Value::String(String::from_utf8_lossy(&bytes).to_string()),
                );
            } else {
                previews.insert(label.into(), serde_json::Value::Null);
            }
        }

        let task = conflict
            .incoming_task_id
            .and_then(|id| self.tasks().into_iter().find(|t| t.id == id));
        let actor = self
            .peers
            .iter()
            .find(|p| p.actor_id == conflict.incoming_actor_id)
            .map(|p| p.display_name.clone());
        let state = self.store.path_state(&conflict.path)?;

        Ok(serde_json::json!({
            "conflict": conflict,
            "short_id": conflict.short_id(),
            "kind_description": conflict.kind.describe(),
            "incoming_actor": actor,
            "incoming_task": task,
            "candidate_files": files,
            "candidates": previews,
            "current_canonical": state.confirmed,
            "in_conflict_draft": state.conflict_draft.is_some(),
            "working_tree_path": conflict.path.to_fs_path(&self.paths.repo_root).display().to_string(),
        }))
    }

    fn resolution_entry(
        &mut self,
        conflict: &Conflict,
        source: ResolveSource,
        content_file: Option<String>,
    ) -> Result<Option<FileEntry>> {
        match source {
            ResolveSource::Delete => Ok(None),
            ResolveSource::Canonical => Ok(self.store.path_state(&conflict.path)?.confirmed),
            ResolveSource::Incoming => {
                self.require_blob(conflict.incoming_entry.as_ref())?;
                Ok(conflict.incoming_entry.clone())
            }
            ResolveSource::LocalCandidate => {
                let entry = conflict
                    .latest_local_candidate
                    .clone()
                    .or_else(|| conflict.incoming_entry.clone());
                self.require_blob(entry.as_ref())?;
                Ok(entry)
            }
            ResolveSource::Supplied => {
                let source = content_file
                    .ok_or_else(|| crate::error::usage("No resolved content was supplied."))?;
                // Streamed into the blob store: a resolution is as large as the
                // file it resolves, and that is no longer bounded by a message.
                let ingested = self
                    .blobs
                    .ingest_file(std::path::Path::new(&source), CLASSIFY_PREFIX)?
                    .ok_or_else(|| crate::error::usage(format!("No such file: {source}")))?;
                if ingested.size > MAX_SYNCED_FILE {
                    return Err(crate::error::unsupported(
                        "The supplied resolution is above the Weave file size limit.",
                    ));
                }
                let mode = conflict
                    .canonical_entry
                    .as_ref()
                    .map(|e| e.git_mode)
                    .unwrap_or(GitMode::Regular);
                Ok(Some(FileEntry::from_ingested(&ingested, mode)))
            }
            ResolveSource::WorkingTree => {
                let state = self.store.path_state(&conflict.path)?;
                let previous = state
                    .materialized
                    .clone()
                    .or_else(|| state.confirmed.clone());
                scan::read_path(
                    &self.paths.repo_root,
                    &conflict.path,
                    previous.as_ref(),
                    &self.blobs,
                    &mut self.scan_cache,
                )
            }
        }
    }

    fn require_blob(&self, entry: Option<&FileEntry>) -> Result<()> {
        let Some(entry) = entry else { return Ok(()) };
        if self.blobs.has(&entry.blob_hash) {
            return Ok(());
        }
        Err(
            crate::error::usage("That candidate's content is not available on this machine.")
                .with_detail(
                    "Run `weave conflict show <id>` to write the available candidates to \
             .git/weave/conflicts, edit the working file, then resolve from the working tree.",
                ),
        )
    }
}

/// Parse `path` or `path:START-END` scope arguments (specification section 93).
pub(crate) fn parse_scopes(raw: &[String]) -> Result<Vec<TaskScope>> {
    let mut out = Vec::new();
    for item in raw {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (path_part, range) = match item.rsplit_once(':') {
            Some((p, r))
                if r.contains('-') && r.chars().next().is_some_and(|c| c.is_ascii_digit()) =>
            {
                (p, Some(r))
            }
            _ => (item, None),
        };
        let path = RepoPath::new(&path_part.replace('\\', "/"))?;
        let (line_start, line_end) = match range {
            None => (None, None),
            Some(range) => {
                let (a, b) = range.split_once('-').ok_or_else(|| {
                    crate::error::usage(format!("`{item}` is not a valid Task scope.")).with_detail(
                        "Use `path` or `path:START-END`, for example \
                                      `slides/07-pricing.tsx:50-110`.",
                    )
                })?;
                let a: u32 = a.trim().parse().map_err(|_| {
                    crate::error::usage(format!("`{item}` has a non-numeric start line."))
                })?;
                let b: u32 = b.trim().parse().map_err(|_| {
                    crate::error::usage(format!("`{item}` has a non-numeric end line."))
                })?;
                if a == 0 || b < a {
                    return Err(crate::error::usage(format!(
                        "`{item}` has an empty or inverted line range."
                    )));
                }
                (Some(a), Some(b))
            }
        };
        out.push(TaskScope {
            path,
            line_start,
            line_end,
            declared_against: None,
            stale: false,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::parse_scopes;

    #[test]
    fn parses_file_and_range_scopes() {
        let scopes = parse_scopes(&[
            "slides/07-pricing.tsx".to_string(),
            "slides/07-pricing.tsx:50-110".to_string(),
        ])
        .unwrap();
        assert_eq!(scopes.len(), 2);
        assert!(scopes[0].line_start.is_none());
        assert_eq!(scopes[1].line_start, Some(50));
        assert_eq!(scopes[1].line_end, Some(110));
    }

    #[test]
    fn rejects_bad_ranges() {
        assert!(parse_scopes(&["a.txt:10-2".to_string()]).is_err());
        assert!(parse_scopes(&["../escape.txt".to_string()]).is_err());
    }
}
