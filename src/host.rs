// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The host coordinator: the single authority for canonical state.
//!
//! Runs as one synchronous state machine on its own thread. Every accepted
//! mutation is durable before it is acknowledged, and revision numbers are
//! assigned only here (specification sections 5, 6, 7, 68, 69).

use crate::blobs::BlobStore;
use crate::error::{ErrorClass, Result, WeaveError};
use crate::gitx;
use crate::model::*;
use crate::path::RepoPath;
use crate::proto::*;
use crate::reconcile::{reconcile, MergeContext, Reconciled};
use crate::session::Paths;
use crate::store_host::{validate_base, ActorRecord, HostStore};
use crate::transport::Outbound;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;
use uuid::Uuid;

/// How long the host waits for every connected participant to complete a
/// commit-preparation barrier before proceeding with a warning.
const BARRIER_TIMEOUT_MS: i64 = 20_000;
/// Replaying more than this many revisions is slower than sending a snapshot.
const MAX_REPLAY: u64 = 5_000;
/// Approximate byte budget for one `Blobs` response.
const BLOB_BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Interval between external-Git-state checks.
const GIT_GUARD_INTERVAL_MS: i64 = 3_000;

/// One input to the coordinator state machine.
///
/// `Message` dominates the size of this enum because a `ClientMessage` can carry
/// a whole file. Boxing it would trade one allocation per frame for a size the
/// channel already handles fine, so the variance is deliberate.
#[allow(clippy::large_enum_variant)]
pub enum HostInput {
    Connected {
        conn_id: u64,
        out: Outbound,
        is_local: bool,
    },
    Message {
        conn_id: u64,
        message: ClientMessage,
    },
    Disconnected {
        conn_id: u64,
    },
    Tick,
    Shutdown,
}

#[derive(Clone)]
pub struct HostHandle {
    tx: std::sync::mpsc::Sender<HostInput>,
}

impl HostHandle {
    pub fn send(&self, input: HostInput) {
        let _ = self.tx.send(input);
    }
}

struct Conn {
    out: Outbound,
    is_local: bool,
    actor_id: Option<Uuid>,
    display_name: String,
    last_applied_revision: u64,
    active_task_id: Option<Uuid>,
    last_seen_ms: i64,
}

struct BarrierPeer {
    watermark: Option<u64>,
    ready: Option<(bool, String)>,
}

struct BarrierState {
    barrier_id: Uuid,
    request_id: Uuid,
    requester_conn: u64,
    requester_actor: Uuid,
    allow_active_tasks: bool,
    started_ms: i64,
    peers: HashMap<u64, BarrierPeer>,
}

pub struct HostEngine {
    paths: Paths,
    store: HostStore,
    blobs: BlobStore,
    session: SessionInfo,
    secret_actor_names: HashMap<Uuid, String>,
    conns: HashMap<u64, Conn>,
    barrier: Option<BarrierState>,
    deferred_ops: Vec<(u64, FileOperation)>,
    state: SyncState,
    last_git_check_ms: i64,
    expected_head: String,
    branch: String,
    host_git_name: String,
    host_git_email: String,
    remote_name: Option<String>,
}

impl HostEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: Paths,
        store: HostStore,
        blobs: BlobStore,
        session: SessionInfo,
        expected_head: String,
        branch: String,
        host_git_name: String,
        host_git_email: String,
        remote_name: Option<String>,
    ) -> HostEngine {
        HostEngine {
            paths,
            store,
            blobs,
            session,
            secret_actor_names: HashMap::new(),
            conns: HashMap::new(),
            barrier: None,
            deferred_ops: Vec::new(),
            state: SyncState::Live,
            last_git_check_ms: 0,
            expected_head,
            branch,
            host_git_name,
            host_git_email,
            remote_name,
        }
    }

    /// Start the coordinator on its own thread.
    pub fn spawn(mut self) -> (HostHandle, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("weave-host".into())
            .spawn(move || self.run(rx))
            .expect("spawn host coordinator thread");
        (HostHandle { tx }, handle)
    }

    fn run(&mut self, rx: Receiver<HostInput>) {
        loop {
            let input = match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(v) => v,
                Err(RecvTimeoutError::Timeout) => HostInput::Tick,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            match input {
                HostInput::Shutdown => {
                    self.broadcast(HostMessage::Goodbye {
                        reason: "host stopped".into(),
                    });
                    return;
                }
                other => {
                    if let Err(e) = self.handle(other) {
                        tracing::error!(class = %e.class, "host: {}", e.message);
                        if matches!(
                            e.class,
                            ErrorClass::PersistenceError | ErrorClass::IntegrityError
                        ) {
                            self.enter_degraded(&e);
                        }
                    }
                }
            }
        }
    }

    fn handle(&mut self, input: HostInput) -> Result<()> {
        match input {
            HostInput::Connected {
                conn_id,
                out,
                is_local,
            } => {
                self.conns.insert(
                    conn_id,
                    Conn {
                        out,
                        is_local,
                        actor_id: None,
                        display_name: String::new(),
                        last_applied_revision: 0,
                        active_task_id: None,
                        last_seen_ms: crate::util::now_ms(),
                    },
                );
                Ok(())
            }
            HostInput::Disconnected { conn_id } => {
                self.conns.remove(&conn_id);
                if let Some(barrier) = &mut self.barrier {
                    barrier.peers.remove(&conn_id);
                }
                self.try_finish_barrier()?;
                self.broadcast_presence();
                Ok(())
            }
            HostInput::Message { conn_id, message } => self.on_message(conn_id, message),
            HostInput::Tick => self.on_tick(),
            HostInput::Shutdown => Ok(()),
        }
    }

    // ------------------------------------------------------------------ tick

    fn on_tick(&mut self) -> Result<()> {
        let now = crate::util::now_ms();
        if now - self.last_git_check_ms >= GIT_GUARD_INTERVAL_MS {
            self.last_git_check_ms = now;
            self.check_git_state()?;
        }
        if self.barrier.is_some() {
            let expired = self
                .barrier
                .as_ref()
                .map(|b| now - b.started_ms > BARRIER_TIMEOUT_MS)
                .unwrap_or(false);
            if expired {
                self.finish_barrier(true)?;
            } else {
                self.try_finish_barrier()?;
            }
        }
        Ok(())
    }

    /// Detect Git state mutated outside Weave (specification section 14).
    /// Weave never repairs it automatically; it pauses until the expected state
    /// returns.
    fn check_git_state(&mut self) -> Result<()> {
        let root = self.paths.repo_root.clone();
        let branch = gitx::current_branch(&root)?;
        let head = gitx::head_oid(&root)?.unwrap_or_default();
        let staged = gitx::has_staged_changes(&root).unwrap_or(false);

        let problem = if branch.as_deref() != Some(self.branch.as_str()) {
            Some((
                "Git state changed outside Weave.".to_string(),
                format!(
                    "Expected branch:\n{}\n\nCurrent branch:\n{}\n\nRestore the expected state or leave the session.",
                    self.branch,
                    branch.clone().unwrap_or_else(|| "(detached HEAD)".into())
                ),
            ))
        } else if head != self.expected_head {
            Some((
                "Git state changed outside Weave.".to_string(),
                format!(
                    "Expected Git commit:\n{}\n\nCurrent Git commit:\n{}\n\nRestore the expected state or leave the session.",
                    crate::util::short_oid(&self.expected_head),
                    crate::util::short_oid(&head)
                ),
            ))
        } else if staged {
            Some((
                "Git index changed outside Weave.".to_string(),
                "Staged changes were found. Weave owns all Git-writing operations during a \
                 session. Run `git reset` to unstage, or leave the session."
                    .to_string(),
            ))
        } else {
            None
        };

        match (problem, &self.state) {
            (Some((reason, detail)), SyncState::Live) => {
                self.state = SyncState::Paused { reason, detail };
                let state = self.state.clone();
                self.broadcast(HostMessage::HostState { state });
            }
            (Some(_), _) => {}
            (None, SyncState::Paused { .. }) => {
                self.state = SyncState::Live;
                let state = self.state.clone();
                self.broadcast(HostMessage::HostState { state });
                tracing::info!("host resumed: expected Git state restored");
            }
            (None, _) => {}
        }
        Ok(())
    }

    fn enter_degraded(&mut self, error: &WeaveError) {
        self.state = SyncState::Degraded {
            reason: error.message.clone(),
        };
        let state = self.state.clone();
        self.broadcast(HostMessage::HostState { state });
    }

    // --------------------------------------------------------------- messages

    fn on_message(&mut self, conn_id: u64, message: ClientMessage) -> Result<()> {
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.last_seen_ms = crate::util::now_ms();
        }
        match message {
            ClientMessage::Hello {
                session_id,
                actor_id,
                display_name,
                git_name,
                git_email,
                base_commit,
                branch,
                resume,
            } => self.on_hello(
                conn_id,
                session_id,
                actor_id,
                display_name,
                git_name,
                git_email,
                base_commit,
                branch,
                resume,
            ),
            ClientMessage::Ping { nonce } => {
                self.send(conn_id, HostMessage::Pong { nonce });
                Ok(())
            }
            ClientMessage::Pong { .. } => Ok(()),
            other => {
                if self.actor_of(conn_id).is_none() {
                    self.send(
                        conn_id,
                        HostMessage::Error {
                            request_id: None,
                            class: ErrorClass::ProtocolError,
                            message: "Send `hello` before any other message.".into(),
                            detail: None,
                        },
                    );
                    return Ok(());
                }
                self.on_authenticated(conn_id, other)
            }
        }
    }

    fn actor_of(&self, conn_id: u64) -> Option<Uuid> {
        self.conns.get(&conn_id).and_then(|c| c.actor_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_hello(
        &mut self,
        conn_id: u64,
        session_id: Uuid,
        actor_id: Uuid,
        display_name: String,
        git_name: String,
        git_email: String,
        base_commit: String,
        branch: String,
        resume: ClientResumeState,
    ) -> Result<()> {
        if session_id != self.session.session_id {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: None,
                    class: ErrorClass::SessionError,
                    message: "That invite is for a different Weave session.".into(),
                    detail: Some(
                        "Ask the host for the current invite (`weave invite` on the host)."
                            .to_string(),
                    ),
                },
            );
            return Ok(());
        }
        if branch != self.branch {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: None,
                    class: ErrorClass::RepositoryError,
                    message: "Your branch is not compatible with this Weave session.".into(),
                    detail: Some(format!(
                        "Session branch:\n{}\n\nYour branch:\n{branch}\n\nCheck out the session \
                         branch and retry.",
                        self.branch
                    )),
                },
            );
            return Ok(());
        }
        // A first-time joiner must sit at the session base commit or at a
        // Weave-published commit (specification section 11).
        if !resume.has_manifest {
            let acceptable = self.acceptable_join_commits()?;
            if !acceptable.iter().any(|c| c == &base_commit) {
                self.send(
                    conn_id,
                    HostMessage::Error {
                        request_id: None,
                        class: ErrorClass::RepositoryError,
                        message: "Cannot join Weave session.".into(),
                        detail: Some(format!(
                            "Session base:\n{}\n\nYour current Git commit:\n{}\n\nCheckout the \
                             expected base commit and retry.",
                            crate::util::short_oid(&self.session.base_commit),
                            crate::util::short_oid(&base_commit)
                        )),
                    },
                );
                return Ok(());
            }
        }

        self.store.upsert_actor(&ActorRecord {
            actor_id,
            display_name: display_name.clone(),
            git_name,
            git_email,
            last_seen_ms: crate::util::now_ms(),
        })?;
        self.secret_actor_names
            .insert(actor_id, display_name.clone());

        // One connection per actor: a reconnect supersedes the stale socket.
        let stale: Vec<u64> = self
            .conns
            .iter()
            .filter(|(id, c)| **id != conn_id && c.actor_id == Some(actor_id))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(conn) = self.conns.remove(&id) {
                conn.out.send_host(HostMessage::Goodbye {
                    reason: "superseded by a new connection".into(),
                });
                conn.out.close();
            }
        }

        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.actor_id = Some(actor_id);
            conn.display_name = display_name;
            conn.last_applied_revision = resume.last_applied_revision;
        }

        let current = self.store.current_revision()?;
        let manifest_all = self.store.manifest_all()?;
        let host_hash = state_hash(manifest_all.iter());

        let needs_snapshot = !resume.has_manifest
            || resume.last_applied_revision > current
            || current.saturating_sub(resume.last_applied_revision) > MAX_REPLAY
            || (resume.last_applied_revision == current
                && !resume.replica_hash.is_empty()
                && resume.replica_hash != host_hash);

        if resume.last_applied_revision == current
            && !resume.replica_hash.is_empty()
            && resume.replica_hash != host_hash
        {
            tracing::warn!(
                actor = %actor_id,
                "ReplicaDivergence at r{current}; sending a fresh canonical snapshot"
            );
        }

        let manifest = if needs_snapshot {
            Some(
                manifest_all
                    .iter()
                    .map(|(path, entry)| ManifestEntry {
                        path: path.clone(),
                        entry: entry.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };

        let control = self.control_snapshot()?;
        let pending_pubs = self
            .store
            .publications_after(resume.last_publication_sequence)?;

        self.send(
            conn_id,
            HostMessage::Welcome {
                session: self.session.clone(),
                snapshot_revision: current,
                manifest,
                control: Box::new(control),
                pending_publications: pending_pubs.clone(),
                host_state_hash: host_hash,
            },
        );

        // Replay the revisions the client is missing (specification section 106).
        if !needs_snapshot && resume.last_applied_revision < current {
            let revisions = self
                .store
                .revisions_in_range(resume.last_applied_revision + 1, current)?;
            for revision in revisions {
                let content = self.content_for_entry(revision.after.as_ref())?;
                self.send(
                    conn_id,
                    HostMessage::RevisionBroadcast {
                        revision: Box::new(revision),
                        content_b64: content,
                    },
                );
            }
        }

        // A disconnected client must never permanently miss a Git publication
        // (specification sections 104, 137).
        for publication in pending_pubs {
            self.send_publication(conn_id, &publication)?;
        }

        if !self.state.is_live() {
            let state = self.state.clone();
            self.send(conn_id, HostMessage::HostState { state });
        }
        self.broadcast_presence();
        Ok(())
    }

    fn acceptable_join_commits(&self) -> Result<Vec<String>> {
        let mut out = vec![self.session.base_commit.clone()];
        if let Some(publication) = self.store.latest_publication()? {
            out.push(publication.descriptor.commit_oid);
        }
        Ok(out)
    }

    fn on_authenticated(&mut self, conn_id: u64, message: ClientMessage) -> Result<()> {
        let actor_id = match self.actor_of(conn_id) {
            Some(a) => a,
            None => return Ok(()),
        };
        match message {
            ClientMessage::SubmitOperation { operation } => {
                self.on_submit(conn_id, actor_id, *operation)
            }
            ClientMessage::RequestBlobs { hashes } => self.on_request_blobs(conn_id, hashes),
            ClientMessage::RequestManifest { reason } => {
                tracing::info!(actor = %actor_id, "manifest resync requested: {reason}");
                let current = self.store.current_revision()?;
                let manifest_all = self.store.manifest_all()?;
                let host_hash = state_hash(manifest_all.iter());
                self.send(
                    conn_id,
                    HostMessage::ManifestSnapshot {
                        snapshot_revision: current,
                        manifest: manifest_all
                            .into_iter()
                            .map(|(path, entry)| ManifestEntry { path, entry })
                            .collect(),
                        host_state_hash: host_hash,
                    },
                );
                Ok(())
            }
            ClientMessage::RequestControlSnapshot => {
                let control = self.control_snapshot()?;
                self.send(
                    conn_id,
                    HostMessage::Control {
                        control: Box::new(control),
                    },
                );
                Ok(())
            }
            ClientMessage::ReportConflict { report } => {
                self.on_report_conflict(conn_id, actor_id, *report)
            }
            ClientMessage::AttachLocalCandidate {
                conflict_id,
                entry,
                content_b64,
            } => self.on_attach_candidate(actor_id, conflict_id, entry, content_b64),
            ClientMessage::ResolveConflict {
                request_id,
                conflict_id,
                operation_id,
                expected_canonical,
                resolved_entry,
                content_b64,
            } => self.on_resolve_conflict(
                conn_id,
                actor_id,
                request_id,
                conflict_id,
                operation_id,
                expected_canonical,
                resolved_entry,
                content_b64,
            ),
            ClientMessage::DismissConflict {
                request_id,
                conflict_id,
            } => self.on_dismiss_conflict(conn_id, request_id, conflict_id),
            ClientMessage::TaskStart {
                request_id,
                task_id,
                description,
                scopes,
            } => self.on_task_start(conn_id, actor_id, request_id, task_id, description, scopes),
            ClientMessage::TaskUpdate {
                request_id,
                task_id,
                description,
                scopes,
            } => self.on_task_update(conn_id, actor_id, request_id, task_id, description, scopes),
            ClientMessage::TaskComplete {
                request_id,
                task_id,
            } => self.on_task_finish(
                conn_id,
                actor_id,
                request_id,
                task_id,
                TaskStatus::Completed,
            ),
            ClientMessage::TaskCancel {
                request_id,
                task_id,
            } => self.on_task_finish(
                conn_id,
                actor_id,
                request_id,
                task_id,
                TaskStatus::Cancelled,
            ),
            ClientMessage::CommitPrepare {
                request_id,
                allow_active_tasks,
            } => self.on_commit_prepare(conn_id, actor_id, request_id, allow_active_tasks),
            ClientMessage::CommitCreate {
                request_id,
                prepare_id,
                message,
            } => self.on_commit_create(conn_id, actor_id, request_id, prepare_id, message),
            ClientMessage::PushRequest { request_id } => self.on_push(conn_id, request_id),
            ClientMessage::BarrierAck {
                barrier_id,
                watermark,
            } => {
                if let Some(barrier) = &mut self.barrier {
                    if barrier.barrier_id == barrier_id {
                        barrier
                            .peers
                            .entry(conn_id)
                            .or_insert(BarrierPeer {
                                watermark: None,
                                ready: None,
                            })
                            .watermark = Some(watermark);
                    }
                }
                self.try_finish_barrier()
            }
            ClientMessage::BarrierReady {
                barrier_id,
                ok,
                detail,
            } => {
                if let Some(barrier) = &mut self.barrier {
                    if barrier.barrier_id == barrier_id {
                        barrier
                            .peers
                            .entry(conn_id)
                            .or_insert(BarrierPeer {
                                watermark: None,
                                ready: None,
                            })
                            .ready = Some((ok, detail));
                    }
                }
                self.try_finish_barrier()
            }
            ClientMessage::ReplicaHash { revision, hash } => {
                let current = self.store.current_revision()?;
                if revision == current {
                    let manifest = self.store.manifest_all()?;
                    let host_hash = state_hash(manifest.iter());
                    if host_hash != hash {
                        tracing::warn!(
                            actor = %actor_id,
                            "ReplicaDivergence at r{current}; sending a fresh canonical snapshot"
                        );
                        self.send(
                            conn_id,
                            HostMessage::ManifestSnapshot {
                                snapshot_revision: current,
                                manifest: manifest
                                    .into_iter()
                                    .map(|(path, entry)| ManifestEntry { path, entry })
                                    .collect(),
                                host_state_hash: host_hash,
                            },
                        );
                    }
                }
                Ok(())
            }
            ClientMessage::Presence {
                last_applied_revision,
                active_task_id,
            } => {
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    conn.last_applied_revision = last_applied_revision;
                    conn.active_task_id = active_task_id;
                }
                self.broadcast_presence();
                Ok(())
            }
            ClientMessage::Hello { .. }
            | ClientMessage::Ping { .. }
            | ClientMessage::Pong { .. } => Ok(()),
        }
    }

    // ------------------------------------------------------------- operations

    fn on_submit(&mut self, conn_id: u64, actor_id: Uuid, mut op: FileOperation) -> Result<()> {
        // Actor identity is bound to the connection, never taken from the
        // payload (specification section 26).
        op.actor_id = actor_id;

        // Post-barrier work is queued until the target revision is fixed
        // (specification section 114).
        if let Some(barrier) = &self.barrier {
            if let Some(peer) = barrier.peers.get(&conn_id) {
                if let Some(watermark) = peer.watermark {
                    if op.local_seq > watermark {
                        self.deferred_ops.push((conn_id, op));
                        return Ok(());
                    }
                }
            }
        }

        if !self.state.is_live() {
            let reason = match &self.state {
                SyncState::Paused { reason, .. } => reason.clone(),
                SyncState::Degraded { reason } => reason.clone(),
                SyncState::Live => String::new(),
            };
            self.reply_operation(
                conn_id,
                op.operation_id,
                OperationOutcome::Rejected {
                    class: ErrorClass::SessionError,
                    message: format!("Host is not accepting changes: {reason}"),
                },
                None,
            );
            return Ok(());
        }

        // Idempotency (specification sections 24, 182, 183).
        let payload_hash = op.payload_hash();
        if let Some((stored_hash, outcome)) = self.store.lookup_operation(&op.operation_id)? {
            if stored_hash == payload_hash {
                let content = match outcome.canonical() {
                    Some((_, entry)) => self.content_for_entry(entry.as_ref())?,
                    None => None,
                };
                self.reply_operation(conn_id, op.operation_id, outcome, content);
            } else {
                self.reply_operation(
                    conn_id,
                    op.operation_id,
                    OperationOutcome::Rejected {
                        class: ErrorClass::ProtocolError,
                        message: "Operation identifier reused with a different payload.".into(),
                    },
                    None,
                );
            }
            return Ok(());
        }

        match self.try_apply(conn_id, actor_id, &op, &payload_hash) {
            Ok(()) => Ok(()),
            Err(e) => {
                match e.class {
                    // Durability failures must never be acknowledged
                    // (specification section 144).
                    ErrorClass::PersistenceError | ErrorClass::IntegrityError => {
                        self.enter_degraded(&e);
                        self.reply_operation(
                            conn_id,
                            op.operation_id,
                            OperationOutcome::Rejected {
                                class: e.class,
                                message: e.message.clone(),
                            },
                            None,
                        );
                    }
                    _ => {
                        let outcome = OperationOutcome::Rejected {
                            class: e.class,
                            message: match &e.detail {
                                Some(d) => format!("{}\n{d}", e.message),
                                None => e.message.clone(),
                            },
                        };
                        self.store.record_operation_result(
                            &op.operation_id,
                            &actor_id,
                            &payload_hash,
                            &outcome,
                        )?;
                        self.reply_operation(conn_id, op.operation_id, outcome, None);
                    }
                }
                Ok(())
            }
        }
    }

    fn try_apply(
        &mut self,
        conn_id: u64,
        actor_id: Uuid,
        op: &FileOperation,
        payload_hash: &str,
    ) -> Result<()> {
        crate::path::validate(op.path.as_str())?;

        if let Some(entry) = &op.desired_entry {
            if entry.size > MAX_SYNCED_FILE {
                return Err(crate::error::unsupported(format!(
                    "{} is {} bytes, above the {} MiB Weave file limit.",
                    op.path,
                    entry.size,
                    MAX_SYNCED_FILE / (1024 * 1024)
                )));
            }
            self.install_content(entry, op.content_b64.as_deref())?;
        }

        // The host validates the declared base against its own history; it
        // never trusts base_entry alone (specification section 25).
        validate_base(&self.store, op)?;

        let base = op.base_entry.clone();
        let current = self.store.manifest_entry(&op.path)?;

        // Portable path collision (specification section 48).
        if op.desired_entry.is_some() && current.is_none() {
            if let Some(other) = self.store.colliding_path(&op.path)? {
                return Err(crate::error::unsupported(format!(
                    "{} collides with {other}.",
                    op.path
                ))
                .with_detail(
                    "Two repository paths that differ only by case or Unicode normalization \
                     cannot coexist in a portable Weave session.",
                ));
            }
        }

        let ctx = MergeContext::new(&self.paths.repo_root, self.paths.scratch(), &self.blobs);
        let outcome = reconcile(
            &ctx,
            base.as_ref(),
            current.as_ref(),
            op.desired_entry.as_ref(),
        )?;

        let task_id = self.effective_task(actor_id, op.task_id)?;

        match outcome {
            Reconciled::Converged => {
                let revision = self.store.current_revision()?;
                let result = OperationOutcome::Converged {
                    revision,
                    canonical_entry: current.clone(),
                };
                self.store.record_operation_result(
                    &op.operation_id,
                    &actor_id,
                    payload_hash,
                    &result,
                )?;
                let content = self.content_for_entry(current.as_ref())?;
                self.reply_operation(conn_id, op.operation_id, result, content);
                Ok(())
            }
            Reconciled::Accept { entry, merged } => {
                let (revision, record) = self.store.commit_revision(
                    &op.operation_id,
                    &actor_id,
                    task_id.as_ref(),
                    payload_hash,
                    &op.path,
                    current.as_ref(),
                    entry.as_ref(),
                    |revision| {
                        if merged {
                            OperationOutcome::Merged {
                                revision,
                                canonical_entry: entry.clone(),
                            }
                        } else {
                            OperationOutcome::Accepted {
                                revision,
                                canonical_entry: entry.clone(),
                            }
                        }
                    },
                )?;
                if let Some(task_id) = task_id {
                    self.attribute_revision_to_task(&task_id, &op.path, revision)?;
                }
                let result = if merged {
                    OperationOutcome::Merged {
                        revision,
                        canonical_entry: entry.clone(),
                    }
                } else {
                    OperationOutcome::Accepted {
                        revision,
                        canonical_entry: entry.clone(),
                    }
                };
                let content = self.content_for_entry(entry.as_ref())?;
                self.reply_operation(conn_id, op.operation_id, result, content);
                self.broadcast_revision(record)?;
                Ok(())
            }
            Reconciled::Conflict(kind) => {
                let revision = self.store.current_revision()?;
                let conflict = Conflict {
                    id: Uuid::new_v4(),
                    path: op.path.clone(),
                    kind,
                    base_entry: base,
                    canonical_entry: current.clone(),
                    incoming_entry: op.desired_entry.clone(),
                    latest_local_candidate: None,
                    incoming_actor_id: actor_id,
                    incoming_task_id: task_id,
                    canonical_revision: revision,
                    created_at_ms: crate::util::now_ms(),
                    status: ConflictStatus::Open,
                    resolved_revision: None,
                };
                self.store.put_conflict(&conflict)?;
                let result = OperationOutcome::Conflicted {
                    conflict_id: conflict.id,
                    kind,
                    revision,
                    canonical_entry: current.clone(),
                };
                self.store.record_operation_result(
                    &op.operation_id,
                    &actor_id,
                    payload_hash,
                    &result,
                )?;
                self.store.bump_control_version()?;
                let content = self.content_for_entry(current.as_ref())?;
                self.reply_operation(conn_id, op.operation_id, result, content);
                self.broadcast_control()?;
                Ok(())
            }
        }
    }

    /// Only an active Task owned by this actor may be attributed
    /// (specification sections 91, 97).
    fn effective_task(&self, actor_id: Uuid, requested: Option<Uuid>) -> Result<Option<Uuid>> {
        let Some(task_id) = requested else {
            return Ok(None);
        };
        match self.store.task(&task_id)? {
            Some(task) if task.actor_id == actor_id && task.status == TaskStatus::Active => {
                Ok(Some(task_id))
            }
            _ => Ok(None),
        }
    }

    fn attribute_revision_to_task(
        &mut self,
        task_id: &Uuid,
        path: &RepoPath,
        revision: u64,
    ) -> Result<()> {
        let Some(mut task) = self.store.task(task_id)? else {
            return Ok(());
        };
        if !task.touched_paths.contains(path) {
            task.touched_paths.push(path.clone());
        }
        if task.first_accepted_revision.is_none() {
            task.first_accepted_revision = Some(revision);
        }
        task.last_accepted_revision = Some(revision);
        task.updated_at_ms = crate::util::now_ms();
        self.mark_stale_scopes(&mut task)?;
        self.store.put_task(&task)?;
        self.store.bump_control_version()?;
        self.broadcast_control()
    }

    /// A line-range scope degrades to file level once the file it was declared
    /// against has moved on (specification section 96).
    fn mark_stale_scopes(&self, task: &mut Task) -> Result<()> {
        for scope in &mut task.scopes {
            if scope.stale || scope.line_start.is_none() {
                continue;
            }
            let current = self.store.manifest_entry(&scope.path)?;
            let declared = scope.declared_against.as_ref();
            if !FileEntry::same_as(current.as_ref(), declared) {
                scope.stale = true;
            }
        }
        Ok(())
    }

    fn install_content(&self, entry: &FileEntry, content_b64: Option<&str>) -> Result<()> {
        if self.blobs.has(&entry.blob_hash) {
            return Ok(());
        }
        let Some(encoded) = content_b64 else {
            return Err(crate::error::protocol(format!(
                "Operation references blob {} without supplying its content.",
                entry.blob_hash
            )));
        };
        let bytes = crate::util::b64_decode(encoded)?;
        if bytes.len() as u64 != entry.size {
            return Err(crate::error::protocol(
                "Operation content length does not match the declared entry size.",
            ));
        }
        let hash = crate::util::sha256_hex(&bytes);
        if hash != entry.blob_hash {
            return Err(crate::error::protocol(
                "Operation content does not hash to the declared blob.",
            ));
        }
        self.blobs.put(&bytes)?;
        Ok(())
    }

    fn content_for_entry(&self, entry: Option<&FileEntry>) -> Result<Option<String>> {
        match entry {
            None => Ok(None),
            Some(entry) => {
                let bytes = self.blobs.get(&entry.blob_hash)?;
                Ok(Some(crate::util::b64_encode(&bytes)))
            }
        }
    }

    // -------------------------------------------------------------- conflicts

    fn on_report_conflict(
        &mut self,
        conn_id: u64,
        actor_id: Uuid,
        report: ConflictReport,
    ) -> Result<()> {
        for blob in &report.blobs {
            if self.blobs.has(&blob.hash) {
                continue;
            }
            let bytes = crate::util::b64_decode(&blob.content_b64)?;
            if crate::util::sha256_hex(&bytes) != blob.hash {
                self.send(
                    conn_id,
                    HostMessage::Error {
                        request_id: None,
                        class: ErrorClass::ProtocolError,
                        message: "Reported conflict content does not match its hash.".into(),
                        detail: None,
                    },
                );
                return Ok(());
            }
            self.blobs.put(&bytes)?;
        }
        if self.store.conflict(&report.id)?.is_some() {
            return Ok(()); // idempotent retransmission
        }
        let conflict = Conflict {
            id: report.id,
            path: report.path,
            kind: report.kind,
            base_entry: report.base_entry,
            canonical_entry: report.canonical_entry,
            incoming_entry: report.incoming_entry,
            latest_local_candidate: report.latest_local_candidate,
            incoming_actor_id: actor_id,
            incoming_task_id: report.incoming_task_id,
            canonical_revision: self.store.current_revision()?,
            created_at_ms: crate::util::now_ms(),
            status: ConflictStatus::Open,
            resolved_revision: None,
        };
        self.store.put_conflict(&conflict)?;
        self.store.bump_control_version()?;
        self.broadcast_control()
    }

    fn on_attach_candidate(
        &mut self,
        _actor_id: Uuid,
        conflict_id: Uuid,
        entry: Option<FileEntry>,
        content_b64: Option<String>,
    ) -> Result<()> {
        let Some(mut conflict) = self.store.conflict(&conflict_id)? else {
            return Ok(());
        };
        if let Some(entry) = &entry {
            self.install_content(entry, content_b64.as_deref())?;
        }
        conflict.latest_local_candidate = entry;
        self.store.put_conflict(&conflict)?;
        self.store.bump_control_version()?;
        self.broadcast_control()
    }

    #[allow(clippy::too_many_arguments)]
    fn on_resolve_conflict(
        &mut self,
        conn_id: u64,
        actor_id: Uuid,
        request_id: Uuid,
        conflict_id: Uuid,
        operation_id: Uuid,
        expected_canonical: Option<FileEntry>,
        resolved_entry: Option<FileEntry>,
        content_b64: Option<String>,
    ) -> Result<()> {
        let Some(mut conflict) = self.store.conflict(&conflict_id)? else {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::UsageError,
                    message: format!("No Weave conflict {conflict_id}."),
                    detail: None,
                },
            );
            return Ok(());
        };
        if conflict.status != ConflictStatus::Open {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::UsageError,
                    message: format!(
                        "Conflict {} is already {}.",
                        conflict.short_id(),
                        conflict.status.as_str()
                    ),
                    detail: None,
                },
            );
            return Ok(());
        }

        let current = self.store.manifest_entry(&conflict.path)?;
        if !FileEntry::same_as(current.as_ref(), expected_canonical.as_ref()) {
            // Specification section 87: the resolver must reconsider against
            // the latest canonical state; the conflict stays open.
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::ConflictError,
                    message: "ResolutionOutdated".into(),
                    detail: Some(format!(
                        "The canonical content of {} changed while the resolution was being \
                         prepared.\n\nRe-read the file and run `weave conflict resolve {}` again.",
                        conflict.path,
                        conflict.short_id()
                    )),
                },
            );
            return Ok(());
        }

        if let Some(entry) = &resolved_entry {
            if let Err(e) = self.install_content(entry, content_b64.as_deref()) {
                self.send(
                    conn_id,
                    HostMessage::Error {
                        request_id: Some(request_id),
                        class: e.class,
                        message: e.message,
                        detail: e.detail,
                    },
                );
                return Ok(());
            }
        }

        let task_id = self.effective_task(actor_id, None)?;
        let payload_hash = crate::util::sha256_hex(
            serde_json::to_string(&(&conflict_id, &resolved_entry))?.as_bytes(),
        );

        if FileEntry::same_as(resolved_entry.as_ref(), current.as_ref()) {
            conflict.status = ConflictStatus::Resolved;
            conflict.resolved_revision = Some(self.store.current_revision()?);
            self.store.put_conflict(&conflict)?;
            self.store.bump_control_version()?;
            self.send(
                conn_id,
                HostMessage::Ack {
                    request_id,
                    note: Some("Resolution matched canonical state; no new revision.".into()),
                },
            );
            self.broadcast_control()?;
            return Ok(());
        }

        // Specification section 88: revision and conflict status change
        // together at the logical control level.
        let (revision, record) = self.store.commit_revision(
            &operation_id,
            &actor_id,
            task_id.as_ref(),
            &payload_hash,
            &conflict.path,
            current.as_ref(),
            resolved_entry.as_ref(),
            |revision| OperationOutcome::Accepted {
                revision,
                canonical_entry: resolved_entry.clone(),
            },
        )?;
        conflict.status = ConflictStatus::Resolved;
        conflict.resolved_revision = Some(revision);
        self.store.put_conflict(&conflict)?;
        if let Some(task_id) = task_id {
            self.attribute_revision_to_task(&task_id, &record.path, revision)?;
        }
        self.store.bump_control_version()?;
        self.send(
            conn_id,
            HostMessage::Ack {
                request_id,
                note: Some(format!(
                    "Resolved at {}.",
                    crate::util::fmt_revision(revision)
                )),
            },
        );
        self.broadcast_revision(record)?;
        self.broadcast_control()
    }

    fn on_dismiss_conflict(
        &mut self,
        conn_id: u64,
        request_id: Uuid,
        conflict_id: Uuid,
    ) -> Result<()> {
        let Some(mut conflict) = self.store.conflict(&conflict_id)? else {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::UsageError,
                    message: format!("No Weave conflict {conflict_id}."),
                    detail: None,
                },
            );
            return Ok(());
        };
        conflict.status = ConflictStatus::Dismissed;
        self.store.put_conflict(&conflict)?;
        self.store.bump_control_version()?;
        self.send(
            conn_id,
            HostMessage::Ack {
                request_id,
                note: None,
            },
        );
        self.broadcast_control()
    }

    // ------------------------------------------------------------------ tasks

    fn on_task_start(
        &mut self,
        conn_id: u64,
        actor_id: Uuid,
        request_id: Uuid,
        task_id: Uuid,
        description: String,
        mut scopes: Vec<TaskScope>,
    ) -> Result<()> {
        if let Some(existing) = self.store.active_task_for_actor(&actor_id)? {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::UsageError,
                    message: format!(
                        "You already have an active Task: {} — {}",
                        existing.short_id(),
                        existing.description
                    ),
                    detail: Some(
                        "V1 allows one active Task per participant. Complete or cancel it first."
                            .into(),
                    ),
                },
            );
            return Ok(());
        }
        for scope in &mut scopes {
            scope.declared_against = self.store.manifest_entry(&scope.path)?;
            scope.stale = false;
        }
        let now = crate::util::now_ms();
        let task = Task {
            id: task_id,
            actor_id,
            description,
            status: TaskStatus::Active,
            created_at_ms: now,
            updated_at_ms: now,
            created_revision: self.store.current_revision()?,
            completed_revision: None,
            scopes,
            touched_paths: Vec::new(),
            first_accepted_revision: None,
            last_accepted_revision: None,
        };
        self.store.put_task(&task)?;
        self.store.bump_control_version()?;
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.active_task_id = Some(task_id);
        }
        self.send(
            conn_id,
            HostMessage::Ack {
                request_id,
                note: None,
            },
        );
        self.broadcast_control()?;
        self.broadcast_presence();
        Ok(())
    }

    fn on_task_update(
        &mut self,
        conn_id: u64,
        actor_id: Uuid,
        request_id: Uuid,
        task_id: Uuid,
        description: Option<String>,
        scopes: Option<Vec<TaskScope>>,
    ) -> Result<()> {
        let Some(mut task) = self.store.task(&task_id)? else {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::UsageError,
                    message: format!("No Weave Task {task_id}."),
                    detail: None,
                },
            );
            return Ok(());
        };
        if task.actor_id != actor_id {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::UsageError,
                    message: "That Task belongs to another participant.".into(),
                    detail: None,
                },
            );
            return Ok(());
        }
        if let Some(description) = description {
            task.description = description;
        }
        if let Some(mut scopes) = scopes {
            for scope in &mut scopes {
                scope.declared_against = self.store.manifest_entry(&scope.path)?;
                scope.stale = false;
            }
            task.scopes = scopes;
        }
        task.updated_at_ms = crate::util::now_ms();
        self.store.put_task(&task)?;
        self.store.bump_control_version()?;
        self.send(
            conn_id,
            HostMessage::Ack {
                request_id,
                note: None,
            },
        );
        self.broadcast_control()
    }

    fn on_task_finish(
        &mut self,
        conn_id: u64,
        actor_id: Uuid,
        request_id: Uuid,
        task_id: Uuid,
        status: TaskStatus,
    ) -> Result<()> {
        let Some(mut task) = self.store.task(&task_id)? else {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::UsageError,
                    message: format!("No Weave Task {task_id}."),
                    detail: None,
                },
            );
            return Ok(());
        };
        if task.actor_id != actor_id {
            self.send(
                conn_id,
                HostMessage::Error {
                    request_id: Some(request_id),
                    class: ErrorClass::UsageError,
                    message: "That Task belongs to another participant.".into(),
                    detail: None,
                },
            );
            return Ok(());
        }
        task.status = status;
        task.completed_revision = Some(self.store.current_revision()?);
        task.updated_at_ms = crate::util::now_ms();
        self.store.put_task(&task)?;
        self.store.bump_control_version()?;
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            if conn.active_task_id == Some(task_id) {
                conn.active_task_id = None;
            }
        }
        self.send(
            conn_id,
            HostMessage::Ack {
                request_id,
                note: None,
            },
        );
        self.broadcast_control()?;
        self.broadcast_presence();
        Ok(())
    }

    // ------------------------------------------------------- commit preparation

    fn on_commit_prepare(
        &mut self,
        conn_id: u64,
        actor_id: Uuid,
        request_id: Uuid,
        allow_active_tasks: bool,
    ) -> Result<()> {
        if self.barrier.is_some() {
            self.send(
                conn_id,
                HostMessage::PrepareResult {
                    request_id,
                    outcome: Box::new(PrepareOutcome::Rejected {
                        class: ErrorClass::SessionError,
                        message: "Another commit preparation is already running.".into(),
                        detail: Some("Wait for it to finish and retry.".into()),
                    }),
                },
            );
            return Ok(());
        }
        if !self.state.is_live() {
            self.send(
                conn_id,
                HostMessage::PrepareResult {
                    request_id,
                    outcome: Box::new(PrepareOutcome::Rejected {
                        class: ErrorClass::SessionError,
                        message: "Weave is paused; Git publication is unavailable.".into(),
                        detail: Some("Run `weave status` for the reason.".into()),
                    }),
                },
            );
            return Ok(());
        }

        let barrier_id = Uuid::new_v4();
        let mut peers = HashMap::new();
        for (id, conn) in &self.conns {
            if conn.actor_id.is_some() {
                peers.insert(
                    *id,
                    BarrierPeer {
                        watermark: None,
                        ready: None,
                    },
                );
                conn.out.send_host(HostMessage::BarrierStart { barrier_id });
            }
        }
        self.barrier = Some(BarrierState {
            barrier_id,
            request_id,
            requester_conn: conn_id,
            requester_actor: actor_id,
            allow_active_tasks,
            started_ms: crate::util::now_ms(),
            peers,
        });
        self.try_finish_barrier()
    }

    fn try_finish_barrier(&mut self) -> Result<()> {
        let ready = match &self.barrier {
            None => return Ok(()),
            Some(barrier) => barrier
                .peers
                .values()
                .all(|p| p.watermark.is_some() && p.ready.is_some()),
        };
        if ready {
            self.finish_barrier(false)?;
        }
        Ok(())
    }

    fn finish_barrier(&mut self, timed_out: bool) -> Result<()> {
        let Some(barrier) = self.barrier.take() else {
            return Ok(());
        };
        let barrier_id = barrier.barrier_id;

        let mut unready: Vec<String> = Vec::new();
        let mut not_responding: Vec<String> = Vec::new();
        for (conn_id, peer) in &barrier.peers {
            let name = self
                .conns
                .get(conn_id)
                .map(|c| c.display_name.clone())
                .unwrap_or_else(|| "unknown participant".into());
            match &peer.ready {
                Some((false, detail)) => unready.push(format!("{name}: {detail}")),
                Some((true, _)) => {}
                None => not_responding.push(name),
            }
        }

        let result = self.build_preparation(&barrier, &unready, &not_responding, timed_out);

        // Release queued post-barrier operations (specification section 114).
        self.broadcast(HostMessage::BarrierEnd { barrier_id });
        let deferred = std::mem::take(&mut self.deferred_ops);
        for (conn_id, op) in deferred {
            if let Some(actor_id) = self.actor_of(conn_id) {
                self.on_submit(conn_id, actor_id, op)?;
            }
        }

        let outcome = match result {
            Ok(prep) => PrepareOutcome::Prepared(Box::new(prep)),
            Err(e) => PrepareOutcome::Rejected {
                class: e.class,
                message: e.message,
                detail: e.detail,
            },
        };
        self.send(
            barrier.requester_conn,
            HostMessage::PrepareResult {
                request_id: barrier.request_id,
                outcome: Box::new(outcome),
            },
        );
        self.broadcast_control()
    }

    fn build_preparation(
        &mut self,
        barrier: &BarrierState,
        unready: &[String],
        not_responding: &[String],
        timed_out: bool,
    ) -> Result<CommitPreparation> {
        if !unready.is_empty() {
            return Err(crate::error::conflict(
                "Cannot prepare Git publication: unresolved conflicts from pre-barrier work.",
            )
            .with_detail(format!(
                "{}\n\nResolve or dismiss every open conflict with `weave conflict resolve`.",
                unready.join("\n")
            )));
        }
        let open = self.store.open_conflicts()?;
        if !open.is_empty() {
            let list: Vec<String> = open
                .iter()
                .map(|c| format!("{} — {}", c.short_id(), c.path))
                .collect();
            return Err(crate::error::conflict(
                "Cannot prepare Git publication: open Weave conflicts remain.",
            )
            .with_detail(format!(
                "{}\n\nResolve or dismiss them, then run `weave commit prepare` again.",
                list.join("\n")
            )));
        }

        let target_revision = self.store.current_revision()?;
        let latest = self.store.latest_publication()?;
        let previous_published_revision = latest
            .as_ref()
            .map(|p| p.descriptor.target_revision)
            .unwrap_or(0);
        let parent_commit_oid = latest
            .as_ref()
            .map(|p| p.descriptor.commit_oid.clone())
            .unwrap_or_else(|| self.session.base_commit.clone());

        // Specification section 117: an active Task whose accepted revisions
        // are inside the target must not be silently published.
        if !barrier.allow_active_tasks {
            for task in self.store.active_tasks()? {
                let revisions = self.store.task_revisions_since(
                    &task.id,
                    previous_published_revision,
                    target_revision,
                )?;
                if !revisions.is_empty() {
                    let name = self
                        .secret_actor_names
                        .get(&task.actor_id)
                        .cloned()
                        .unwrap_or_else(|| "a participant".into());
                    let range = if revisions.len() == 1 {
                        crate::util::fmt_revision(revisions[0])
                    } else {
                        format!(
                            "{}–{}",
                            crate::util::fmt_revision(revisions[0]),
                            crate::util::fmt_revision(*revisions.last().unwrap())
                        )
                    };
                    return Err(crate::error::conflict("Cannot prepare Git publication.")
                        .with_detail(format!(
                            "{} — {name}\n\"{}\"\n\nis still active and contributed:\n{range}\n\n\
                             Complete or cancel the Task first.",
                            task.short_id(),
                            task.description
                        )));
                }
            }
        }

        let included_task_ids = self
            .store
            .tasks_contributing(previous_published_revision, target_revision)?;
        let mut included_tasks = Vec::new();
        let mut touched_paths: Vec<RepoPath> = Vec::new();
        for task_id in &included_task_ids {
            let Some(task) = self.store.task(task_id)? else {
                continue;
            };
            let revisions = self.store.task_revisions_since(
                task_id,
                previous_published_revision,
                target_revision,
            )?;
            for path in &task.touched_paths {
                if !touched_paths.contains(path) {
                    touched_paths.push(path.clone());
                }
            }
            included_tasks.push(PreparedTask {
                id: task.id,
                short_id: task.short_id(),
                description: task.description.clone(),
                status: task.status,
                actor_id: task.actor_id,
                actor_display_name: self
                    .store
                    .actor(&task.actor_id)?
                    .map(|a| a.display_name)
                    .unwrap_or_else(|| "unknown".into()),
                touched_paths: task.touched_paths.clone(),
                revisions,
            });
        }
        touched_paths.sort();
        let unassigned_revisions = self
            .store
            .unassigned_revisions(previous_published_revision, target_revision)?;

        let mut contributors = Vec::new();
        for (actor_id, count) in self
            .store
            .contributor_counts(previous_published_revision, target_revision)?
        {
            let record = self.store.actor(&actor_id)?;
            contributors.push(Contributor {
                actor_id,
                display_name: record
                    .as_ref()
                    .map(|r| r.display_name.clone())
                    .unwrap_or_else(|| "unknown".into()),
                email: record.as_ref().and_then(|r| {
                    if r.git_email.trim().is_empty() {
                        None
                    } else {
                        Some(r.git_email.clone())
                    }
                }),
                revisions: count,
            });
        }

        let before = self.store.manifest_at(previous_published_revision)?;
        let after = self.store.manifest_at(target_revision)?;
        let diff_summary = diff_manifests(&before, &after);

        let mut disconnected: Vec<String> = not_responding.to_vec();
        for actor in self.store.actors()? {
            let online = self
                .conns
                .values()
                .any(|c| c.actor_id == Some(actor.actor_id));
            if !online && !disconnected.contains(&actor.display_name) {
                disconnected.push(actor.display_name);
            }
        }
        if timed_out {
            tracing::warn!("commit preparation barrier timed out");
        }

        let prep = CommitPreparation {
            prepare_id: Uuid::new_v4(),
            target_revision,
            parent_commit_oid,
            requesting_actor: barrier.requester_actor,
            included_task_ids,
            included_tasks,
            touched_paths,
            unassigned_revisions,
            created_at_ms: crate::util::now_ms(),
            status: PrepareStatus::Prepared,
            previous_published_revision,
            diff_summary,
            contributors,
            disconnected_participants: disconnected,
        };
        self.store.put_preparation(&prep)?;
        self.store.bump_control_version()?;
        Ok(prep)
    }

    // -------------------------------------------------------- commit creation

    fn on_commit_create(
        &mut self,
        conn_id: u64,
        actor_id: Uuid,
        request_id: Uuid,
        prepare_id: Uuid,
        message: String,
    ) -> Result<()> {
        let outcome = match self.create_commit(actor_id, prepare_id, message) {
            Ok(publication) => CommitOutcome::Published {
                publication: Box::new(publication),
            },
            Err(e) => CommitOutcome::Rejected {
                class: e.class,
                message: e.message,
                detail: e.detail,
            },
        };
        if let CommitOutcome::Published { publication } = &outcome {
            let publication = (**publication).clone();
            let conn_ids: Vec<u64> = self.conns.keys().copied().collect();
            for id in conn_ids {
                self.send_publication(id, &publication)?;
            }
        }
        self.send(
            conn_id,
            HostMessage::CommitResult {
                request_id,
                outcome: Box::new(outcome),
            },
        );
        self.broadcast_control()
    }

    fn create_commit(
        &mut self,
        actor_id: Uuid,
        prepare_id: Uuid,
        message: String,
    ) -> Result<GitPublication> {
        let Some(mut prep) = self.store.preparation(&prepare_id)? else {
            return Err(
                crate::error::usage(format!("No Weave commit preparation {prepare_id}."))
                    .with_detail("Run `weave commit prepare` to create one."),
            );
        };
        if prep.status != PrepareStatus::Prepared {
            return Err(crate::error::usage(format!(
                "Preparation {} is already {:?}.",
                prep.short_id(),
                prep.status
            )));
        }
        if message.trim().is_empty() {
            return Err(crate::error::usage("A commit message is required.")
                .with_detail("Pass --message, or supply the message on stdin."));
        }

        let root = self.paths.repo_root.clone();
        let head = gitx::head_oid(&root)?.unwrap_or_default();
        if head != prep.parent_commit_oid {
            return Err(crate::error::git(
                "The Git branch moved since this publication was prepared.",
            )
            .with_detail(format!(
                "Expected parent:\n{}\n\nCurrent Git commit:\n{}\n\nRun `weave commit prepare` again.",
                crate::util::short_oid(&prep.parent_commit_oid),
                crate::util::short_oid(&head)
            )));
        }

        // Git objects are built from the historical target revision, never
        // from the live working tree (specification sections 126, 127).
        let manifest = self.store.manifest_at(prep.target_revision)?;
        let mut tree_entries: BTreeMap<RepoPath, (GitMode, String)> = BTreeMap::new();
        for (path, entry) in &manifest {
            let bytes = self.blobs.get(&entry.blob_hash)?;
            let oid = gitx::hash_object(&root, path, &bytes)?;
            tree_entries.insert(path.clone(), (entry.git_mode, oid));
        }
        let tree_oid = gitx::write_tree(&root, &self.paths.scratch(), &tree_entries)?;

        let author = self.store.actor(&actor_id)?;
        let (author_name, author_email) = match &author {
            Some(a) if !a.git_email.trim().is_empty() => (a.git_name.clone(), a.git_email.clone()),
            Some(a) => (a.git_name.clone(), self.host_git_email.clone()),
            None => (self.host_git_name.clone(), self.host_git_email.clone()),
        };
        // A primary commit author must have a usable email address
        // (specification section 27); Weave never invents one.
        if author_email.trim().is_empty() {
            return Err(crate::error::usage(
                "No Git email address is configured for the commit author.",
            )
            .with_detail(
                "Run `git config user.email you@example.com` on the requesting machine (and on \
                 the host), then prepare the publication again.",
            ));
        }
        if self.host_git_email.trim().is_empty() {
            return Err(
                crate::error::usage("The Weave host has no Git email address configured.")
                    .with_detail(
                        "Run `git config user.email you@example.com` on the host and retry.",
                    ),
            );
        }

        let mut full_message = message.trim_end().to_string();
        let trailers = self.coauthor_trailers(&prep, &author_email);
        if !trailers.is_empty() {
            full_message.push_str("\n\n");
            full_message.push_str(&trailers.join("\n"));
        }
        full_message.push('\n');

        let timestamp = crate::util::now_secs();
        let commit_oid = gitx::commit_tree(
            &root,
            &tree_oid,
            Some(&prep.parent_commit_oid),
            &full_message,
            &author_name,
            &author_email,
            &self.host_git_name,
            &self.host_git_email,
            timestamp,
            "+0000",
        )?;

        let descriptor = CommitDescriptor {
            prepare_id,
            target_revision: prep.target_revision,
            parent_commit_oid: prep.parent_commit_oid.clone(),
            tree_oid,
            commit_oid: commit_oid.clone(),
            author_name,
            author_email,
            committer_name: self.host_git_name.clone(),
            committer_email: self.host_git_email.clone(),
            timestamp,
            timezone: "+0000".into(),
            message: full_message,
            contributing_task_ids: prep.included_task_ids.clone(),
            branch: self.branch.clone(),
        };

        let sequence = self.store.next_publication_sequence()?;
        let mut publication = GitPublication {
            descriptor,
            stage: PublicationStage::ObjectsInstalled,
            push_status: PushStatus::NotAttempted,
            push_error: None,
            created_at_ms: crate::util::now_ms(),
            sequence,
        };
        // Record before mutating refs so a crash leaves a journal to repair
        // (specification sections 135, 136, 195).
        self.store.put_publication(&publication)?;

        let refname = format!("refs/heads/{}", self.branch);
        gitx::update_ref_cas(&root, &refname, &commit_oid, Some(&prep.parent_commit_oid))?;
        publication.stage = PublicationStage::RefUpdated;
        self.store.put_publication(&publication)?;
        self.expected_head = commit_oid.clone();

        gitx::read_tree_into_index(&root, &publication.descriptor.tree_oid)?;
        publication.stage = PublicationStage::Complete;
        self.store.put_publication(&publication)?;

        prep.status = PrepareStatus::Published;
        self.store.put_preparation(&prep)?;

        // Automatic push is the V1 default when an upstream exists
        // (specification section 138).
        if let Some(remote) = self.remote_name.clone() {
            let result = gitx::push(&root, &remote, &self.branch, None)?;
            if result.ok {
                publication.push_status = PushStatus::Pushed;
            } else if result.diverged {
                publication.push_status = PushStatus::Diverged;
                publication.push_error = Some(divergence_message(&result.message));
            } else {
                publication.push_status = PushStatus::Failed;
                publication.push_error = Some(result.message);
            }
        } else {
            publication.push_status = PushStatus::NoUpstream;
        }
        self.store.put_publication(&publication)?;
        self.store.bump_control_version()?;
        Ok(publication)
    }

    fn coauthor_trailers(&self, prep: &CommitPreparation, author_email: &str) -> Vec<String> {
        let mut trailers = Vec::new();
        for contributor in &prep.contributors {
            // Weave never invents a plausible personal address
            // (specification section 130).
            let Some(email) = &contributor.email else {
                continue;
            };
            if email == author_email {
                continue;
            }
            let trailer = format!("Co-authored-by: {} <{}>", contributor.display_name, email);
            if !trailers.contains(&trailer) {
                trailers.push(trailer);
            }
        }
        trailers.sort();
        trailers
    }

    fn on_push(&mut self, conn_id: u64, request_id: Uuid) -> Result<()> {
        let Some(mut publication) = self.store.latest_publication()? else {
            self.send(
                conn_id,
                HostMessage::PushResult {
                    request_id,
                    status: PushStatus::NotAttempted,
                    message: "Nothing has been published yet.".into(),
                },
            );
            return Ok(());
        };
        let Some(remote) = self.remote_name.clone() else {
            self.send(
                conn_id,
                HostMessage::PushResult {
                    request_id,
                    status: PushStatus::NoUpstream,
                    message: "This branch has no upstream remote.".into(),
                },
            );
            return Ok(());
        };
        let root = self.paths.repo_root.clone();
        let result = gitx::push(&root, &remote, &self.branch, None)?;
        let (status, message) = if result.ok {
            (
                PushStatus::Pushed,
                format!("Pushed to {remote}/{}.", self.branch),
            )
        } else if result.diverged {
            (PushStatus::Diverged, divergence_message(&result.message))
        } else {
            (PushStatus::Failed, result.message.clone())
        };
        publication.push_status = status;
        publication.push_error = if status == PushStatus::Pushed {
            None
        } else {
            Some(message.clone())
        };
        self.store.put_publication(&publication)?;
        self.store.bump_control_version()?;
        self.send(
            conn_id,
            HostMessage::PushResult {
                request_id,
                status,
                message,
            },
        );
        self.broadcast_control()
    }

    fn send_publication(&mut self, conn_id: u64, publication: &GitPublication) -> Result<()> {
        let is_local = self
            .conns
            .get(&conn_id)
            .map(|c| c.is_local)
            .unwrap_or(false);
        let pack = if is_local {
            // The host machine already holds the objects it just created.
            None
        } else {
            let root = self.paths.repo_root.clone();
            let pack = gitx::pack_objects(
                &root,
                &publication.descriptor.commit_oid,
                Some(&publication.descriptor.parent_commit_oid),
            )?;
            Some(crate::util::b64_encode(&pack))
        };
        self.send(
            conn_id,
            HostMessage::Publication {
                publication: Box::new(publication.clone()),
                pack_b64: pack,
            },
        );
        Ok(())
    }

    // ------------------------------------------------------------------ blobs

    fn on_request_blobs(&mut self, conn_id: u64, hashes: Vec<String>) -> Result<()> {
        let mut batch: Vec<BlobPayload> = Vec::new();
        let mut batch_bytes = 0usize;
        for hash in hashes {
            let bytes = match self.blobs.get(&hash) {
                Ok(b) => b,
                Err(e) => {
                    // Never silently substitute content (specification
                    // section 145).
                    self.enter_degraded(&e);
                    self.send(
                        conn_id,
                        HostMessage::Error {
                            request_id: None,
                            class: ErrorClass::IntegrityError,
                            message: e.message,
                            detail: e.detail,
                        },
                    );
                    return Ok(());
                }
            };
            let encoded = crate::util::b64_encode(&bytes);
            if batch_bytes + encoded.len() > BLOB_BATCH_BYTES && !batch.is_empty() {
                self.send(
                    conn_id,
                    HostMessage::Blobs {
                        blobs: std::mem::take(&mut batch),
                    },
                );
                batch_bytes = 0;
            }
            batch_bytes += encoded.len();
            batch.push(BlobPayload {
                hash,
                content_b64: encoded,
            });
        }
        if !batch.is_empty() {
            self.send(conn_id, HostMessage::Blobs { blobs: batch });
        }
        Ok(())
    }

    // -------------------------------------------------------------- broadcast

    fn control_snapshot(&self) -> Result<ControlSnapshot> {
        Ok(ControlSnapshot {
            control_version: self.store.control_version()?,
            tasks: self.store.tasks()?,
            conflicts: self.store.conflicts()?,
            publication: self.store.latest_publication()?,
            publication_sequence: self
                .store
                .latest_publication()?
                .map(|p| p.sequence)
                .unwrap_or(0),
            session: self.session.clone(),
        })
    }

    fn broadcast_control(&mut self) -> Result<()> {
        let control = self.control_snapshot()?;
        self.broadcast(HostMessage::Control {
            control: Box::new(control),
        });
        Ok(())
    }

    fn broadcast_presence(&mut self) {
        let mut peers = Vec::new();
        for conn in self.conns.values() {
            let Some(actor_id) = conn.actor_id else {
                continue;
            };
            peers.push(PeerInfo {
                actor_id,
                display_name: conn.display_name.clone(),
                role: if actor_id == self.session.host_actor_id {
                    Role::Host
                } else {
                    Role::Participant
                },
                online: true,
                last_known_revision: conn.last_applied_revision,
                active_task_id: conn.active_task_id,
                active_task_description: conn
                    .active_task_id
                    .and_then(|id| self.store.task(&id).ok().flatten())
                    .map(|t| t.description),
                last_seen_ms: conn.last_seen_ms,
            });
        }
        if let Ok(actors) = self.store.actors() {
            for actor in actors {
                if peers.iter().any(|p| p.actor_id == actor.actor_id) {
                    continue;
                }
                peers.push(PeerInfo {
                    actor_id: actor.actor_id,
                    display_name: actor.display_name,
                    role: if actor.actor_id == self.session.host_actor_id {
                        Role::Host
                    } else {
                        Role::Participant
                    },
                    online: false,
                    last_known_revision: 0,
                    active_task_id: None,
                    active_task_description: None,
                    last_seen_ms: actor.last_seen_ms,
                });
            }
        }
        peers.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        self.broadcast(HostMessage::Presence { peers });
    }

    fn broadcast_revision(&mut self, revision: Revision) -> Result<()> {
        let content = self.content_for_entry(revision.after.as_ref())?;
        self.broadcast(HostMessage::RevisionBroadcast {
            revision: Box::new(revision),
            content_b64: content,
        });
        Ok(())
    }

    fn broadcast(&mut self, message: HostMessage) {
        let mut dead = Vec::new();
        for (id, conn) in &self.conns {
            if conn.actor_id.is_none() {
                continue;
            }
            if !conn.out.send_host(message.clone()) {
                dead.push(*id);
            }
        }
        for id in dead {
            tracing::warn!(conn = id, "disconnecting slow participant (backpressure)");
            if let Some(conn) = self.conns.remove(&id) {
                conn.out.close();
            }
        }
    }

    fn send(&mut self, conn_id: u64, message: HostMessage) {
        let alive = match self.conns.get(&conn_id) {
            Some(conn) => conn.out.send_host(message),
            None => true,
        };
        if !alive {
            tracing::warn!(
                conn = conn_id,
                "disconnecting slow participant (backpressure)"
            );
            if let Some(conn) = self.conns.remove(&conn_id) {
                conn.out.close();
            }
        }
    }

    fn reply_operation(
        &mut self,
        conn_id: u64,
        operation_id: Uuid,
        outcome: OperationOutcome,
        content_b64: Option<String>,
    ) {
        self.send(
            conn_id,
            HostMessage::OperationResult {
                operation_id,
                outcome: Box::new(outcome),
                content_b64,
            },
        );
    }
}

fn divergence_message(raw: &str) -> String {
    format!(
        "Remote branch diverged from the Weave session.\n\n\
         Automatic reconciliation with external Git history is not supported in V1.\n\n{raw}"
    )
}

/// Added / modified / deleted paths between two manifests.
pub fn diff_manifests(
    before: &BTreeMap<RepoPath, FileEntry>,
    after: &BTreeMap<RepoPath, FileEntry>,
) -> DiffSummary {
    let mut summary = DiffSummary::default();
    for (path, entry) in after {
        match before.get(path) {
            None => summary.added.push(path.clone()),
            Some(old) if !FileEntry::same_as(Some(old), Some(entry)) => {
                summary.modified.push(path.clone())
            }
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            summary.deleted.push(path.clone());
        }
    }
    summary
}
