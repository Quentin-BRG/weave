// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The participant replica engine.
//!
//! One synchronous state machine per machine, owning the working tree, the
//! persistent outbox and the local Git publication journal. The host runs one
//! of these too, so its own edits follow the identical path
//! (specification section 5).
//!
//! The invariant that shapes this file is section 7.3: Weave must never
//! overwrite local bytes it has not already captured durably. Every write to
//! the working tree is preceded by a synchronous capture check (section 36).

use crate::blobs::BlobStore;
use crate::error::{ErrorClass, Result, WeaveError};
use crate::gitx;
use crate::ipc::{IpcCommand, IpcResponse};
use crate::model::*;
use crate::path::RepoPath;
use crate::proto::*;
use crate::reconcile::{reconcile, MergeContext, Reconciled};
use crate::scan::{self, RejectedPath};
use crate::session::Paths;
use crate::store_client::{ClientStore, ConflictDraft, InFlight, PendingLocal};
use crate::transport::Outbound;
use crate::watch::WatchEvent;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[path = "client_ipc.rs"]
mod ipc_commands;

const TICK_MS: u64 = 400;
const SAFETY_RESCAN_MS: i64 = 15_000;
const PRESENCE_INTERVAL_MS: i64 = 8_000;
const HEARTBEAT_INTERVAL_MS: i64 = 15_000;
const GIT_GUARD_INTERVAL_MS: i64 = 3_000;
const RETRY_INTERVAL_MS: i64 = 2_000;
/// An in-flight operation with no durable result after this long is resent.
const RESEND_AFTER_MS: i64 = 20_000;

pub struct IpcCall {
    pub command: IpcCommand,
    pub reply: tokio::sync::oneshot::Sender<IpcResponse>,
}

pub enum ClientInput {
    Watch(WatchEvent),
    Host(HostMessage),
    Connected(Outbound),
    Disconnected(String),
    Ipc(IpcCall),
    Tick,
    Shutdown,
}

#[derive(Clone)]
pub struct ClientHandle {
    tx: std::sync::mpsc::Sender<ClientInput>,
}

impl ClientHandle {
    pub fn send(&self, input: ClientInput) {
        let _ = self.tx.send(input);
    }
}

struct BarrierLocal {
    barrier_id: Uuid,
    watermark: u64,
    ready_sent: bool,
    conflicted: bool,
}

/// A request forwarded to the host whose answer completes a CLI command.
struct PendingRequest {
    reply: tokio::sync::oneshot::Sender<IpcResponse>,
}

pub struct ClientEngine {
    paths: Paths,
    store: ClientStore,
    blobs: BlobStore,
    actor_id: Uuid,
    display_name: String,
    git_name: String,
    git_email: String,
    role: Role,
    session: SessionInfo,
    branch: String,

    out: Option<Outbound>,
    connected: bool,
    connection_note: String,

    control: Option<ControlSnapshot>,
    peers: Vec<PeerInfo>,
    host_state: SyncState,
    local_state: SyncState,

    op_index: HashMap<Uuid, RepoPath>,
    pending_revisions: BTreeMap<u64, (Revision, Option<Vec<u8>>)>,
    barrier: Option<BarrierLocal>,
    requests: HashMap<Uuid, PendingRequest>,

    rejected_paths: Vec<RejectedPath>,
    notices: Vec<String>,
    expected_head: String,

    last_rescan_ms: i64,
    last_presence_ms: i64,
    last_heartbeat_ms: i64,
    last_git_check_ms: i64,
    last_retry_ms: i64,
    heartbeat_nonce: u64,

    /// A Git-guard problem seen once and awaiting confirmation.
    git_problem_pending: bool,
    /// Set when materialization is waiting for blobs from the host.
    materialization_blocked: bool,
    shutdown: bool,
}

impl ClientEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: Paths,
        store: ClientStore,
        blobs: BlobStore,
        actor_id: Uuid,
        display_name: String,
        git_name: String,
        git_email: String,
        role: Role,
        session: SessionInfo,
        branch: String,
        expected_head: String,
    ) -> ClientEngine {
        ClientEngine {
            paths,
            store,
            blobs,
            actor_id,
            display_name,
            git_name,
            git_email,
            role,
            session,
            branch,
            out: None,
            connected: false,
            connection_note: "connecting".into(),
            control: None,
            peers: Vec::new(),
            host_state: SyncState::Live,
            local_state: SyncState::Live,
            op_index: HashMap::new(),
            pending_revisions: BTreeMap::new(),
            barrier: None,
            requests: HashMap::new(),
            rejected_paths: Vec::new(),
            notices: Vec::new(),
            expected_head,
            last_rescan_ms: 0,
            last_presence_ms: 0,
            last_heartbeat_ms: 0,
            last_git_check_ms: 0,
            last_retry_ms: 0,
            heartbeat_nonce: 0,
            git_problem_pending: false,
            materialization_blocked: false,
            shutdown: false,
        }
    }

    pub fn spawn(self) -> (ClientHandle, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("weave-client".into())
            .spawn(move || {
                let mut engine = self;
                engine.run(rx);
            })
            .expect("spawn client engine thread");
        (ClientHandle { tx }, handle)
    }

    fn run(&mut self, rx: Receiver<ClientInput>) {
        if let Err(e) = self.rebuild_op_index() {
            tracing::error!("client: {}", e.message);
        }
        // Ticks are scheduled against the clock, not against an idle input
        // channel. Deriving them from `recv_timeout` expiring means a caller
        // polling `weave status` faster than TICK_MS keeps the channel
        // permanently non-empty and starves every timed duty — the Git guard,
        // the safety rescan, the outbox retry, presence and heartbeats — for as
        // long as it keeps polling.
        let tick = Duration::from_millis(TICK_MS);
        let mut next_tick = Instant::now() + tick;
        loop {
            let now = Instant::now();
            let input = if now >= next_tick {
                next_tick = now + tick;
                ClientInput::Tick
            } else {
                match rx.recv_timeout(next_tick - now) {
                    Ok(v) => v,
                    Err(RecvTimeoutError::Timeout) => {
                        next_tick = Instant::now() + tick;
                        ClientInput::Tick
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            if let ClientInput::Shutdown = input {
                return;
            }
            if let Err(e) = self.handle(input) {
                tracing::error!(class = %e.class, "client: {}", e.message);
                if matches!(
                    e.class,
                    ErrorClass::PersistenceError | ErrorClass::IntegrityError
                ) {
                    self.local_state = SyncState::Degraded {
                        reason: e.message.clone(),
                    };
                }
            }
            if self.shutdown {
                return;
            }
        }
    }

    fn handle(&mut self, input: ClientInput) -> Result<()> {
        match input {
            ClientInput::Watch(WatchEvent::Changed(paths)) => self.on_watch_batch(paths),
            ClientInput::Watch(WatchEvent::RescanNeeded(reason)) => {
                tracing::warn!("full rescan requested: {reason}");
                self.full_rescan()
            }
            ClientInput::Host(message) => self.on_host_message(message),
            ClientInput::Connected(out) => self.on_connected(out),
            ClientInput::Disconnected(reason) => {
                self.connected = false;
                self.connection_note = reason;
                self.out = None;
                // Unsent work stays in the outbox; nothing is discarded
                // (specification sections 35, 148).
                self.mark_all_unsent()
            }
            ClientInput::Ipc(call) => {
                self.on_ipc(call.command, call.reply);
                Ok(())
            }
            ClientInput::Tick => self.on_tick(),
            ClientInput::Shutdown => Ok(()),
        }
    }

    // ------------------------------------------------------------------ setup

    fn rebuild_op_index(&mut self) -> Result<()> {
        self.op_index.clear();
        for (path, state) in self.store.all_states()? {
            if let Some(f) = &state.in_flight {
                self.op_index.insert(f.operation_id, path.clone());
            }
        }
        Ok(())
    }

    /// Record the current working tree as "materialized" without treating it as
    /// local work. Used once, before the first canonical manifest arrives, when
    /// the working tree is known clean at the session base commit
    /// (specification section 11).
    pub fn seed_materialized_from_disk(&mut self) -> Result<()> {
        let previous = BTreeMap::new();
        let result = scan::scan_repository(&self.paths.repo_root, &previous, &self.blobs)?;
        self.rejected_paths = result.rejected;
        for (path, entry) in result.entries {
            let mut state = self.store.path_state(&path)?;
            if state.materialized.is_some() || state.has_local_work() {
                continue;
            }
            state.materialized = Some(entry);
            self.store.put_path_state(&path, &state)?;
        }
        Ok(())
    }

    fn on_connected(&mut self, out: Outbound) -> Result<()> {
        self.out = Some(out);
        self.connected = true;
        self.connection_note = "online".into();
        // Hello must be the first frame on the socket, so the handshake is sent
        // before the reconnect rescan can produce any operation.
        let resume = self.resume_state()?;
        self.send(ClientMessage::Hello {
            session_id: self.session.session_id,
            actor_id: self.actor_id,
            display_name: self.display_name.clone(),
            git_name: self.git_name.clone(),
            git_email: self.git_email.clone(),
            base_commit: gitx::head_oid(&self.paths.repo_root)?.unwrap_or_default(),
            branch: self.branch.clone(),
            resume,
        });
        // A full rescan on (re)connect is mandatory (specification section 32):
        // anything edited while Weave was not watching is captured here.
        self.full_rescan()?;
        Ok(())
    }

    fn resume_state(&self) -> Result<ClientResumeState> {
        let manifest = self.store.replica_manifest()?;
        let pending: Vec<Uuid> = self.op_index.keys().copied().collect();
        Ok(ClientResumeState {
            last_applied_revision: self.store.last_applied_revision()?,
            control_version: self.store.control_version()?,
            last_publication_sequence: self.store.last_publication_sequence()?,
            pending_operation_ids: pending,
            replica_hash: state_hash(manifest.iter()),
            has_manifest: self.store.has_manifest()?,
        })
    }

    fn mark_all_unsent(&mut self) -> Result<()> {
        for (path, mut state) in self.store.all_states()? {
            if let Some(f) = &mut state.in_flight {
                if f.sent {
                    f.sent = false;
                    self.store.put_path_state(&path, &state)?;
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------- tick

    fn on_tick(&mut self) -> Result<()> {
        let now = crate::util::now_ms();

        if now - self.last_git_check_ms >= GIT_GUARD_INTERVAL_MS {
            self.last_git_check_ms = now;
            self.check_git_state()?;
        }
        if now - self.last_rescan_ms >= SAFETY_RESCAN_MS {
            self.full_rescan()?;
        }
        if now - self.last_retry_ms >= RETRY_INTERVAL_MS {
            self.last_retry_ms = now;
            self.flush_outbox()?;
            if self.materialization_blocked {
                self.sync_working_tree()?;
            }
        }
        if self.connected && now - self.last_presence_ms >= PRESENCE_INTERVAL_MS {
            self.last_presence_ms = now;
            let last_applied = self.store.last_applied_revision()?;
            self.send(ClientMessage::Presence {
                last_applied_revision: last_applied,
                active_task_id: self.my_active_task().map(|t| t.id),
            });
            // Only meaningful once a canonical manifest has been installed;
            // before that an empty replica is not a diverged one.
            if self.store.has_manifest()? {
                let manifest = self.store.replica_manifest()?;
                self.send(ClientMessage::ReplicaHash {
                    revision: last_applied,
                    hash: state_hash(manifest.iter()),
                });
            }
        }
        if self.connected && now - self.last_heartbeat_ms >= HEARTBEAT_INTERVAL_MS {
            self.last_heartbeat_ms = now;
            self.heartbeat_nonce += 1;
            let nonce = self.heartbeat_nonce;
            self.send(ClientMessage::Ping { nonce });
        }
        Ok(())
    }

    /// Detect Git state mutated outside Weave (specification section 14).
    fn check_git_state(&mut self) -> Result<()> {
        let root = self.paths.repo_root.clone();
        let branch = gitx::current_branch(&root)?;
        let head = gitx::head_oid(&root)?.unwrap_or_default();
        let staged = gitx::has_staged_changes(&root).unwrap_or(false);

        let problem = if branch.as_deref() != Some(self.branch.as_str()) {
            Some((
                "Weave paused: Git state changed outside Weave.".to_string(),
                format!(
                    "Expected branch:\n{}\n\nCurrent branch:\n{}\n\nRestore the expected state or leave the session.",
                    self.branch,
                    branch.clone().unwrap_or_else(|| "(detached HEAD)".into())
                ),
            ))
        } else if head != self.expected_head {
            Some((
                "Weave paused: Git state changed outside Weave.".to_string(),
                format!(
                    "Expected Git commit:\n{}\n\nCurrent Git commit:\n{}\n\nRestore the expected state or leave the session.",
                    crate::util::short_oid(&self.expected_head),
                    crate::util::short_oid(&head)
                ),
            ))
        } else if staged {
            Some((
                "Weave paused: the Git index changed outside Weave.".to_string(),
                "Weave owns all Git-writing operations during a session. Run `git reset` to \
                 unstage, or leave the session."
                    .to_string(),
            ))
        } else {
            None
        };

        // A publication moves HEAD and then the index, in two separate Git
        // commands, and the host engine does the same on its own thread against
        // the same repository. Between those two commands the repository really
        // does look like someone staged something — so a single observation is
        // not evidence of an external change, it may just be Weave mid-write.
        // Only a problem still present on the following check is treated as
        // real: a `git commit` a user ran stays, a publication window does not.
        match (problem, &self.local_state) {
            (Some((reason, detail)), SyncState::Live) => {
                if self.git_problem_pending {
                    self.git_problem_pending = false;
                    self.local_state = SyncState::Paused { reason, detail };
                } else {
                    self.git_problem_pending = true;
                }
            }
            (None, SyncState::Paused { .. }) => {
                self.git_problem_pending = false;
                self.local_state = SyncState::Live;
                self.full_rescan()?;
            }
            (None, _) => self.git_problem_pending = false,
            _ => {}
        }
        Ok(())
    }

    fn paused(&self) -> bool {
        !self.local_state.is_live()
    }

    // ---------------------------------------------------------------- capture

    fn on_watch_batch(&mut self, raw_paths: Vec<String>) -> Result<()> {
        if self.paused() {
            return Ok(());
        }
        let root = self.paths.repo_root.clone();
        let ignored = gitx::filter_ignored(&root, &raw_paths)?;
        for raw in raw_paths {
            if ignored.contains(&raw) {
                continue;
            }
            let path = match RepoPath::new(&raw) {
                Ok(p) => p,
                Err(e) => {
                    self.note_rejected(&raw, &e.message);
                    continue;
                }
            };
            if let Err(e) = self.capture_path(&path) {
                self.note_rejected(path.as_str(), &e.message);
            }
        }
        Ok(())
    }

    /// Authoritative full rescan (specification sections 32, 185).
    pub fn full_rescan(&mut self) -> Result<()> {
        self.last_rescan_ms = crate::util::now_ms();
        if self.paused() {
            return Ok(());
        }
        let root = self.paths.repo_root.clone();
        let previous = self.store.replica_manifest()?;
        let result = scan::scan_repository(&root, &previous, &self.blobs)?;
        self.rejected_paths = result.rejected;

        let seen: HashSet<RepoPath> = result.entries.keys().cloned().collect();
        for path in result.entries.keys() {
            if let Err(e) = self.capture_path(path) {
                self.note_rejected(path.as_str(), &e.message);
            }
        }
        // Paths Weave knows about that the scan did not return.
        let known: Vec<RepoPath> = self.store.all_paths()?;
        let mut vanished: Vec<String> = Vec::new();
        for path in known {
            if seen.contains(&path) {
                continue;
            }
            let state = self.store.path_state(&path)?;
            if state.materialized.is_none() && state.confirmed.is_none() {
                continue;
            }
            if path.to_fs_path(&root).exists() {
                // Present on disk but not reported by Git: it became ignored.
                vanished.push(path.to_string());
                continue;
            }
            if let Err(e) = self.capture_path(&path) {
                self.note_rejected(path.as_str(), &e.message);
            }
        }
        if !vanished.is_empty() {
            self.note(format!(
                "{} path(s) are now Git-ignored and no longer synchronized: {}",
                vanished.len(),
                vanished.join(", ")
            ));
        }
        self.sync_working_tree()?;
        Ok(())
    }

    /// Durably capture the current on-disk state of `path` if it differs from
    /// what Weave last materialized.
    ///
    /// This is both the watcher path and the capture-before-overwrite check
    /// (specification sections 36, 44). Comparing against `materialized` is
    /// what makes a Weave-written file not echo back as a local edit, and it
    /// does so by content rather than by timer.
    fn capture_path(&mut self, path: &RepoPath) -> Result<bool> {
        let mut state = self.store.path_state(path)?;
        let previous = state
            .materialized
            .clone()
            .or_else(|| state.confirmed.clone());
        // Reading the path also stores its content, so by the time the entry
        // exists the blob it names is durable.
        let entry = scan::read_path(&self.paths.repo_root, path, previous.as_ref(), &self.blobs)?;

        if FileEntry::same_as(entry.as_ref(), state.materialized.as_ref()) {
            return Ok(false);
        }
        if entry.is_none() && state.materialized.is_none() {
            return Ok(false);
        }

        let seq = self.store.next_local_seq()?;
        let task_id = self.my_active_task().map(|t| t.id);

        if let Some(draft) = &mut state.conflict_draft {
            // Conflict draft mode: captured durably, never auto-submitted
            // (specification section 85).
            draft.entry = entry.clone();
            draft.local_seq = seq;
            state.materialized = entry;
            self.store.put_path_state(path, &state)?;
            return Ok(true);
        }

        if state.in_flight.is_some() {
            // At most one in-flight operation per path; newer work coalesces
            // into pending_local (specification section 38).
            state.pending_local = Some(PendingLocal {
                desired: entry.clone(),
                local_seq: seq,
                task_id,
            });
            state.materialized = entry;
            self.store.put_path_state(path, &state)?;
            self.check_barrier_ready()?;
            return Ok(true);
        }

        let operation_id = Uuid::new_v4();
        state.in_flight = Some(InFlight {
            operation_id,
            base_revision: state.confirmed_revision,
            base_entry: state.confirmed.clone(),
            desired: entry.clone(),
            local_seq: seq,
            task_id,
            sent: false,
            sent_at_ms: 0,
        });
        state.materialized = entry;
        self.store.put_path_state(path, &state)?;
        self.op_index.insert(operation_id, path.clone());
        self.submit_path(path)?;
        Ok(true)
    }

    fn submit_path(&mut self, path: &RepoPath) -> Result<()> {
        let mut state = self.store.path_state(path)?;
        let Some(flight) = state.in_flight.clone() else {
            return Ok(());
        };
        if flight.sent || !self.connected {
            return Ok(());
        }
        // Post-barrier work is withheld until the target revision is fixed
        // (specification section 114).
        if let Some(barrier) = &self.barrier {
            if flight.local_seq > barrier.watermark {
                return Ok(());
            }
        }
        let content_b64 = match &flight.desired {
            Some(entry) => Some(crate::util::b64_encode(&self.blobs.get(&entry.blob_hash)?)),
            None => None,
        };
        let op = FileOperation {
            operation_id: flight.operation_id,
            actor_id: self.actor_id,
            task_id: flight.task_id,
            local_seq: flight.local_seq,
            base_revision: flight.base_revision,
            base_entry: flight.base_entry.clone(),
            path: path.clone(),
            desired_entry: flight.desired.clone(),
            content_b64,
        };
        self.send(ClientMessage::SubmitOperation {
            operation: Box::new(op),
        });
        if let Some(f) = &mut state.in_flight {
            f.sent = true;
            f.sent_at_ms = crate::util::now_ms();
        }
        self.store.put_path_state(path, &state)?;
        Ok(())
    }

    fn flush_outbox(&mut self) -> Result<()> {
        if !self.connected {
            return Ok(());
        }
        let now = crate::util::now_ms();
        let paths: Vec<RepoPath> = self.store.all_paths()?;
        for path in paths {
            let mut state = self.store.path_state(&path)?;
            match &state.in_flight {
                Some(f) if !f.sent => self.submit_path(&path)?,
                Some(f) if now - f.sent_at_ms > RESEND_AFTER_MS => {
                    // No durable result arrived. Retransmission is safe because
                    // the host resolves a repeated operation_id with an
                    // identical payload to its original result (section 24).
                    if let Some(f) = &mut state.in_flight {
                        f.sent = false;
                    }
                    self.store.put_path_state(&path, &state)?;
                    self.submit_path(&path)?;
                }
                Some(_) => {}
                None => {
                    if state.pending_local.is_some() {
                        self.promote_pending(&path)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Turn a queued `pending_local` into the next in-flight operation.
    fn promote_pending(&mut self, path: &RepoPath) -> Result<()> {
        let mut state = self.store.path_state(path)?;
        if state.in_flight.is_some() || state.conflict_draft.is_some() {
            return Ok(());
        }
        let Some(pending) = state.pending_local.take() else {
            return Ok(());
        };
        let operation_id = Uuid::new_v4();
        state.in_flight = Some(InFlight {
            operation_id,
            base_revision: state.confirmed_revision,
            base_entry: state.confirmed.clone(),
            desired: pending.desired,
            local_seq: pending.local_seq,
            task_id: pending.task_id,
            sent: false,
            sent_at_ms: 0,
        });
        self.store.put_path_state(path, &state)?;
        self.op_index.insert(operation_id, path.clone());
        self.submit_path(path)
    }

    fn note_rejected(&mut self, path: &str, reason: &str) {
        let entry = RejectedPath {
            path: path.to_string(),
            reason: reason.to_string(),
        };
        if !self.rejected_paths.iter().any(|r| r.path == entry.path) {
            tracing::warn!("{}: {}", entry.path, entry.reason);
            self.rejected_paths.push(entry);
        }
    }

    fn note(&mut self, message: String) {
        if !self.notices.contains(&message) {
            tracing::info!("{message}");
            self.notices.push(message);
            if self.notices.len() > 20 {
                self.notices.remove(0);
            }
        }
    }

    // --------------------------------------------------------- host messages

    fn on_host_message(&mut self, message: HostMessage) -> Result<()> {
        match message {
            HostMessage::Welcome {
                session,
                snapshot_revision,
                manifest,
                control,
                pending_publications,
                host_state_hash,
            } => {
                self.session = session.clone();
                self.store.set_session(&session)?;
                if let Some(manifest) = manifest {
                    self.apply_manifest(snapshot_revision, manifest, &host_state_hash)?;
                }
                self.apply_control(*control)?;
                if !pending_publications.is_empty() {
                    tracing::info!(
                        "{} Git publication(s) to install",
                        pending_publications.len()
                    );
                }
                self.drain_pending_revisions()?;
                self.sync_working_tree()?;
                self.flush_outbox()?;
                Ok(())
            }
            HostMessage::ManifestSnapshot {
                snapshot_revision,
                manifest,
                host_state_hash,
            } => {
                self.apply_manifest(snapshot_revision, manifest, &host_state_hash)?;
                self.drain_pending_revisions()?;
                self.sync_working_tree()?;
                self.flush_outbox()
            }
            HostMessage::RevisionBroadcast {
                revision,
                content_b64,
            } => {
                let content = match content_b64 {
                    Some(text) => Some(crate::util::b64_decode(&text)?),
                    None => None,
                };
                if let Some(bytes) = &content {
                    self.blobs.put(bytes)?;
                }
                self.enqueue_revision(*revision, content)
            }
            HostMessage::OperationResult {
                operation_id,
                outcome,
                content_b64,
            } => {
                if let Some(text) = &content_b64 {
                    let bytes = crate::util::b64_decode(text)?;
                    self.blobs.put(&bytes)?;
                }
                self.on_operation_result(operation_id, *outcome)
            }
            HostMessage::Blobs { blobs } => {
                for blob in blobs {
                    let bytes = crate::util::b64_decode(&blob.content_b64)?;
                    if crate::util::sha256_hex(&bytes) != blob.hash {
                        return Err(crate::error::integrity(
                            "The host sent blob content that does not match its hash.",
                        ));
                    }
                    self.blobs.put(&bytes)?;
                }
                self.sync_working_tree()
            }
            HostMessage::Control { control } => self.apply_control(*control),
            HostMessage::Presence { peers } => {
                self.peers = peers;
                Ok(())
            }
            HostMessage::BarrierStart { barrier_id } => self.on_barrier_start(barrier_id),
            HostMessage::BarrierEnd { barrier_id } => {
                if self.barrier.as_ref().map(|b| b.barrier_id) == Some(barrier_id) {
                    self.barrier = None;
                    self.flush_outbox()?;
                }
                Ok(())
            }
            HostMessage::Publication {
                publication,
                pack_b64,
            } => {
                let pack = match pack_b64 {
                    Some(text) => Some(crate::util::b64_decode(&text)?),
                    None => None,
                };
                self.apply_publication(*publication, pack)
            }
            HostMessage::PrepareResult {
                request_id,
                outcome,
            } => {
                let response = match *outcome {
                    PrepareOutcome::Prepared(prep) => {
                        IpcResponse::ok(serde_json::to_value(&*prep)?)
                    }
                    PrepareOutcome::Rejected {
                        class,
                        message,
                        detail,
                    } => {
                        let mut e = WeaveError::new(class, message);
                        e.detail = detail;
                        IpcResponse::error(&e)
                    }
                };
                self.complete_request(request_id, response);
                Ok(())
            }
            HostMessage::CommitResult {
                request_id,
                outcome,
            } => {
                let response = match *outcome {
                    CommitOutcome::Published { publication } => {
                        IpcResponse::ok(serde_json::to_value(&*publication)?)
                    }
                    CommitOutcome::Rejected {
                        class,
                        message,
                        detail,
                    } => {
                        let mut e = WeaveError::new(class, message);
                        e.detail = detail;
                        IpcResponse::error(&e)
                    }
                };
                self.complete_request(request_id, response);
                Ok(())
            }
            HostMessage::PushResult {
                request_id,
                status,
                message,
            } => {
                let response = if status == PushStatus::Pushed {
                    IpcResponse::ok(serde_json::json!({
                        "push_status": status.as_str(),
                        "message": message,
                    }))
                } else {
                    IpcResponse::error(
                        &crate::error::git(message)
                            .with_detail(format!("Push status: {}", status.as_str())),
                    )
                };
                self.complete_request(request_id, response);
                Ok(())
            }
            HostMessage::Ack { request_id, note } => {
                self.complete_request(
                    request_id,
                    IpcResponse::ok(serde_json::json!({ "note": note })),
                );
                Ok(())
            }
            HostMessage::Error {
                request_id,
                class,
                message,
                detail,
            } => {
                let mut e = WeaveError::new(class, message);
                e.detail = detail;
                match request_id {
                    Some(id) => self.complete_request(id, IpcResponse::error(&e)),
                    None => {
                        tracing::error!(class = %e.class, "host: {}", e.message);
                        self.note(e.message.clone());
                    }
                }
                Ok(())
            }
            HostMessage::HostState { state } => {
                self.host_state = state;
                Ok(())
            }
            HostMessage::Ping { nonce } => {
                self.send(ClientMessage::Pong { nonce });
                Ok(())
            }
            HostMessage::Pong { .. } => Ok(()),
            HostMessage::Goodbye { reason } => {
                self.connected = false;
                self.connection_note = reason;
                Ok(())
            }
        }
    }

    fn apply_manifest(
        &mut self,
        snapshot_revision: u64,
        manifest: Vec<ManifestEntry>,
        host_state_hash: &str,
    ) -> Result<()> {
        let entries: BTreeMap<RepoPath, FileEntry> =
            manifest.into_iter().map(|m| (m.path, m.entry)).collect();
        let computed = state_hash(entries.iter());
        if computed != host_state_hash {
            return Err(crate::error::integrity(
                "The canonical manifest sent by the host failed its own state hash check.",
            ));
        }
        self.store
            .replace_confirmed_manifest(snapshot_revision, &entries)?;
        self.store.set_last_applied_revision(snapshot_revision)?;
        self.pending_revisions
            .retain(|rev, _| *rev > snapshot_revision);
        self.request_missing_blobs(entries.values())?;
        Ok(())
    }

    fn request_missing_blobs<'a, I: Iterator<Item = &'a FileEntry>>(
        &mut self,
        entries: I,
    ) -> Result<()> {
        let mut missing: Vec<String> = Vec::new();
        for entry in entries {
            if !self.blobs.has(&entry.blob_hash) && !missing.contains(&entry.blob_hash) {
                missing.push(entry.blob_hash.clone());
            }
        }
        if !missing.is_empty() {
            self.materialization_blocked = true;
            for chunk in missing.chunks(256) {
                self.send(ClientMessage::RequestBlobs {
                    hashes: chunk.to_vec(),
                });
            }
        }
        Ok(())
    }

    fn apply_control(&mut self, control: ControlSnapshot) -> Result<()> {
        self.store.set_control_cache(&control)?;
        let previous_open: HashSet<Uuid> = self
            .control
            .as_ref()
            .map(|c| {
                c.conflicts
                    .iter()
                    .filter(|x| x.status == ConflictStatus::Open)
                    .map(|x| x.id)
                    .collect()
            })
            .unwrap_or_default();
        self.control = Some(control);

        // Leaving conflict draft mode when a conflict is no longer open.
        let now_open: HashSet<Uuid> = self
            .control
            .as_ref()
            .map(|c| {
                c.conflicts
                    .iter()
                    .filter(|x| x.status == ConflictStatus::Open)
                    .map(|x| x.id)
                    .collect()
            })
            .unwrap_or_default();
        let closed: Vec<Uuid> = previous_open.difference(&now_open).copied().collect();
        if !closed.is_empty() {
            for (path, mut state) in self.store.all_states()? {
                let Some(draft) = &state.conflict_draft else {
                    continue;
                };
                if closed.contains(&draft.conflict_id) {
                    state.conflict_draft = None;
                    self.store.put_path_state(&path, &state)?;
                }
            }
            self.sync_working_tree()?;
        }
        Ok(())
    }

    // ------------------------------------------------------------- revisions

    fn enqueue_revision(&mut self, revision: Revision, content: Option<Vec<u8>>) -> Result<()> {
        let last = self.store.last_applied_revision()?;
        if revision.revision <= last {
            return Ok(());
        }
        self.pending_revisions
            .insert(revision.revision, (revision, content));
        self.drain_pending_revisions()
    }

    /// Apply buffered revisions strictly in order. The watermark means "every
    /// revision up to and including this one has been applied", never "the
    /// highest revision seen" (specification section 105).
    fn drain_pending_revisions(&mut self) -> Result<()> {
        loop {
            let last = self.store.last_applied_revision()?;
            let Some((revision, _)) = self.pending_revisions.remove(&(last + 1)) else {
                break;
            };
            self.apply_revision(revision)?;
            self.store.set_last_applied_revision(last + 1)?;
        }
        // A gap that persists means we lost a broadcast; ask for a snapshot.
        if let Some(next) = self.pending_revisions.keys().next().copied() {
            let last = self.store.last_applied_revision()?;
            if next > last + 1 {
                self.send(ClientMessage::RequestManifest {
                    reason: format!("gap between r{last} and r{next}"),
                });
            }
        }
        Ok(())
    }

    fn apply_revision(&mut self, revision: Revision) -> Result<()> {
        let path = revision.path.clone();
        let mut state = self.store.path_state(&path)?;
        if revision.revision > state.confirmed_revision {
            state.confirmed = revision.after.clone();
            state.confirmed_revision = revision.revision;
            self.store.put_path_state(&path, &state)?;
        }
        self.materialize_if_safe(&path)?;
        Ok(())
    }

    /// Bring the working tree to canonical state for one path, but only when
    /// doing so cannot lose local bytes (specification sections 36, 39).
    fn materialize_if_safe(&mut self, path: &RepoPath) -> Result<()> {
        let state = self.store.path_state(path)?;
        if state.conflict_draft.is_some() {
            // The user is resolving a conflict here; canonical updates are
            // recorded but the draft is never overwritten.
            return Ok(());
        }
        if state.has_local_work() {
            // Local unconfirmed work exists: keep the local bytes and let the
            // operation result drive the rebase.
            return Ok(());
        }
        if FileEntry::same_as(state.confirmed.as_ref(), state.materialized.as_ref()) {
            return Ok(());
        }
        // Capture-before-overwrite: inspect the real filesystem entry first.
        if self.capture_path(path)? {
            return Ok(());
        }
        self.write_canonical(path)
    }

    fn write_canonical(&mut self, path: &RepoPath) -> Result<()> {
        let mut state = self.store.path_state(path)?;
        match &state.confirmed {
            Some(entry) => {
                if !self.blobs.has(&entry.blob_hash) {
                    self.materialization_blocked = true;
                    self.send(ClientMessage::RequestBlobs {
                        hashes: vec![entry.blob_hash.clone()],
                    });
                    return Ok(());
                }
                scan::materialize_file(
                    &self.paths.repo_root,
                    path,
                    &self.blobs,
                    &entry.blob_hash,
                    entry.git_mode,
                )?;
                state.materialized = Some(entry.clone());
            }
            None => {
                scan::materialize_delete(&self.paths.repo_root, path)?;
                state.materialized = None;
            }
        }
        self.store.put_path_state(path, &state)?;
        Ok(())
    }

    fn sync_working_tree(&mut self) -> Result<()> {
        self.materialization_blocked = false;
        let paths: Vec<RepoPath> = self.store.all_paths()?;
        for path in paths {
            self.materialize_if_safe(&path)?;
        }
        Ok(())
    }

    // ------------------------------------------------------ operation results

    fn on_operation_result(&mut self, operation_id: Uuid, outcome: OperationOutcome) -> Result<()> {
        let Some(path) = self.op_index.get(&operation_id).cloned() else {
            return Ok(());
        };
        let mut state = self.store.path_state(&path)?;
        let Some(flight) = state.in_flight.clone() else {
            self.op_index.remove(&operation_id);
            return Ok(());
        };
        if flight.operation_id != operation_id {
            return Ok(());
        }

        match &outcome {
            OperationOutcome::Rejected { class, message } => {
                return self.on_operation_rejected(&path, operation_id, *class, message.clone());
            }
            _ => {
                self.op_index.remove(&operation_id);
            }
        }

        let (revision, canonical) = outcome
            .canonical()
            .unwrap_or((state.confirmed_revision, state.confirmed.clone()));
        if revision >= state.confirmed_revision {
            state.confirmed = canonical.clone();
            state.confirmed_revision = revision;
        }
        state.in_flight = None;
        self.store.put_path_state(&path, &state)?;

        if let OperationOutcome::Conflicted { conflict_id, .. } = &outcome {
            return self.on_conflicted(&path, *conflict_id, canonical);
        }

        // Specification sections 40-42: reconcile any newer local work against
        // the canonical result before the working tree is touched.
        let state = self.store.path_state(&path)?;
        if let Some(pending) = state.pending_local.clone() {
            self.continuation_rebase(&path, flight.desired, canonical, pending)?;
        } else {
            self.materialize_if_safe(&path)?;
        }
        self.check_barrier_ready()?;
        Ok(())
    }

    fn on_operation_rejected(
        &mut self,
        path: &RepoPath,
        operation_id: Uuid,
        class: ErrorClass,
        message: String,
    ) -> Result<()> {
        let mut state = self.store.path_state(path)?;
        match class {
            // Transient: keep the operation in the outbox and retry.
            ErrorClass::SessionError | ErrorClass::NetworkError | ErrorClass::PersistenceError => {
                if let Some(f) = &mut state.in_flight {
                    f.sent = false;
                }
                self.store.put_path_state(path, &state)?;
                self.note(format!("{path}: {message} (queued for retry)"));
            }
            // The declared base no longer matches the host's history: keep the
            // bytes, resynchronize, then resubmit from the fresh base.
            ErrorClass::ProtocolError => {
                self.op_index.remove(&operation_id);
                let flight = state.in_flight.take();
                if let Some(flight) = flight {
                    let seq = flight.local_seq;
                    if state.pending_local.is_none() {
                        state.pending_local = Some(PendingLocal {
                            desired: flight.desired,
                            local_seq: seq,
                            task_id: flight.task_id,
                        });
                    }
                }
                self.store.put_path_state(path, &state)?;
                self.note(format!("{path}: {message}; resynchronizing"));
                self.send(ClientMessage::RequestManifest { reason: message });
            }
            // Terminal: the file cannot participate. Never resubmit in a loop,
            // and never discard the user's bytes on disk.
            _ => {
                self.op_index.remove(&operation_id);
                state.in_flight = None;
                state.pending_local = None;
                self.store.put_path_state(path, &state)?;
                self.note_rejected(path.as_str(), &message);
            }
        }
        self.check_barrier_ready()?;
        Ok(())
    }

    /// Local continuation rebase: `base = in-flight candidate`,
    /// `current = canonical result`, `incoming = newer local candidate`
    /// (specification sections 40-42).
    fn continuation_rebase(
        &mut self,
        path: &RepoPath,
        base: Option<FileEntry>,
        canonical: Option<FileEntry>,
        pending: PendingLocal,
    ) -> Result<()> {
        let ctx = MergeContext::new(&self.paths.repo_root, self.paths.scratch(), &self.blobs);
        let outcome = reconcile(
            &ctx,
            base.as_ref(),
            canonical.as_ref(),
            pending.desired.as_ref(),
        );

        let outcome = match outcome {
            Ok(v) => v,
            Err(e) if e.class == ErrorClass::IntegrityError => {
                // A blob needed for the rebase is missing locally; ask for it
                // and retry on arrival. Nothing is discarded.
                self.materialization_blocked = true;
                if let Some(entry) = &canonical {
                    self.send(ClientMessage::RequestBlobs {
                        hashes: vec![entry.blob_hash.clone()],
                    });
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        let mut state = self.store.path_state(path)?;
        match outcome {
            Reconciled::Converged => {
                state.pending_local = None;
                self.store.put_path_state(path, &state)?;
                self.materialize_if_safe(path)?;
            }
            Reconciled::Accept { entry, .. } => {
                // Persist and materialize the rebased local content, then base
                // the next operation on the canonical result.
                state.pending_local = None;
                let operation_id = Uuid::new_v4();
                state.in_flight = Some(InFlight {
                    operation_id,
                    base_revision: state.confirmed_revision,
                    base_entry: state.confirmed.clone(),
                    desired: entry.clone(),
                    local_seq: pending.local_seq,
                    task_id: pending.task_id,
                    sent: false,
                    sent_at_ms: 0,
                });
                self.store.put_path_state(path, &state)?;
                self.op_index.insert(operation_id, path.clone());
                self.write_local_candidate(path, entry.as_ref())?;
                self.submit_path(path)?;
            }
            Reconciled::Conflict(kind) => {
                // Preserve both sides durably before the working tree is
                // restored to canonical (specification section 42).
                let conflict_id = Uuid::new_v4();
                let mut payloads = Vec::new();
                for entry in [base.as_ref(), pending.desired.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    if let Ok(bytes) = self.blobs.get(&entry.blob_hash) {
                        payloads.push(BlobPayload {
                            hash: entry.blob_hash.clone(),
                            content_b64: crate::util::b64_encode(&bytes),
                        });
                    }
                }
                self.send(ClientMessage::ReportConflict {
                    report: Box::new(ConflictReport {
                        id: conflict_id,
                        path: path.clone(),
                        kind,
                        base_entry: base.clone(),
                        canonical_entry: canonical.clone(),
                        incoming_entry: pending.desired.clone(),
                        latest_local_candidate: pending.desired.clone(),
                        incoming_task_id: pending.task_id,
                        blobs: payloads,
                    }),
                });
                state.pending_local = None;
                state.conflict_draft = Some(ConflictDraft {
                    conflict_id,
                    entry: pending.desired.clone(),
                    local_seq: pending.local_seq,
                });
                self.store.put_path_state(path, &state)?;
                // Only now may the working path be restored to canonical.
                self.restore_canonical_for_conflict(path)?;
                self.note(format!(
                    "Conflict {} — {path}\nYour changes could not be merged automatically.\n\
                     No work was discarded.\nRun: weave conflict show {}",
                    crate::util::short_id('C', &conflict_id),
                    crate::util::short_id('C', &conflict_id)
                ));
            }
        }
        Ok(())
    }

    /// Write locally rebased bytes without treating the write as a new edit.
    fn write_local_candidate(&mut self, path: &RepoPath, entry: Option<&FileEntry>) -> Result<()> {
        let mut state = self.store.path_state(path)?;
        match entry {
            Some(entry) => {
                scan::materialize_file(
                    &self.paths.repo_root,
                    path,
                    &self.blobs,
                    &entry.blob_hash,
                    entry.git_mode,
                )?;
                state.materialized = Some(entry.clone());
            }
            None => {
                scan::materialize_delete(&self.paths.repo_root, path)?;
                state.materialized = None;
            }
        }
        self.store.put_path_state(path, &state)?;
        Ok(())
    }

    fn on_conflicted(
        &mut self,
        path: &RepoPath,
        conflict_id: Uuid,
        canonical: Option<FileEntry>,
    ) -> Result<()> {
        let mut state = self.store.path_state(path)?;
        // The newest local candidate must be attached to the conflict before
        // anything is restored (specification section 43).
        let latest = state.pending_local.clone();
        if let Some(pending) = &latest {
            let content = match &pending.desired {
                Some(entry) => match self.blobs.get(&entry.blob_hash) {
                    Ok(bytes) => Some(crate::util::b64_encode(&bytes)),
                    Err(_) => None,
                },
                None => None,
            };
            self.send(ClientMessage::AttachLocalCandidate {
                conflict_id,
                entry: pending.desired.clone(),
                content_b64: content,
            });
        }
        let draft_entry = latest
            .as_ref()
            .map(|p| p.desired.clone())
            .unwrap_or_else(|| state.materialized.clone());
        state.pending_local = None;
        state.conflict_draft = Some(ConflictDraft {
            conflict_id,
            entry: draft_entry,
            local_seq: self.store.local_seq()?,
        });
        let _ = canonical;
        self.store.put_path_state(path, &state)?;
        self.restore_canonical_for_conflict(path)?;
        if let Some(barrier) = &mut self.barrier {
            barrier.conflicted = true;
        }
        self.note(format!(
            "Conflict {} — {path}\nYour changes could not be merged automatically.\n\
             No work was discarded.\nRun: weave conflict show {}",
            crate::util::short_id('C', &conflict_id),
            crate::util::short_id('C', &conflict_id)
        ));
        self.check_barrier_ready()?;
        Ok(())
    }

    /// Restore the working path to canonical content once the local candidate
    /// is durably stored. The candidate remains recoverable through the
    /// conflict record and the blob store.
    fn restore_canonical_for_conflict(&mut self, path: &RepoPath) -> Result<()> {
        let mut state = self.store.path_state(path)?;
        match state.confirmed.clone() {
            Some(entry) => {
                if !self.blobs.has(&entry.blob_hash) {
                    self.materialization_blocked = true;
                    self.send(ClientMessage::RequestBlobs {
                        hashes: vec![entry.blob_hash.clone()],
                    });
                    return Ok(());
                }
                scan::materialize_file(
                    &self.paths.repo_root,
                    path,
                    &self.blobs,
                    &entry.blob_hash,
                    entry.git_mode,
                )?;
                state.materialized = Some(entry);
            }
            None => {
                scan::materialize_delete(&self.paths.repo_root, path)?;
                state.materialized = None;
            }
        }
        if let Some(draft) = &mut state.conflict_draft {
            // The draft now starts from canonical content on disk.
            draft.entry = state.materialized.clone();
        }
        self.store.put_path_state(path, &state)?;
        Ok(())
    }

    // -------------------------------------------------------------- barriers

    fn on_barrier_start(&mut self, barrier_id: Uuid) -> Result<()> {
        // Rescan and capture everything currently visible, then freeze the
        // watermark (specification section 113).
        self.full_rescan()?;
        let watermark = self.store.local_seq()?;
        self.barrier = Some(BarrierLocal {
            barrier_id,
            watermark,
            ready_sent: false,
            conflicted: false,
        });
        self.send(ClientMessage::BarrierAck {
            barrier_id,
            watermark,
        });
        self.flush_outbox()?;
        self.check_barrier_ready()
    }

    /// A participant is barrier-ready only when every operation at or before
    /// its watermark is accepted, converged, or turned into an explicit
    /// conflict (specification section 115).
    fn check_barrier_ready(&mut self) -> Result<()> {
        let Some(barrier) = &self.barrier else {
            return Ok(());
        };
        if barrier.ready_sent {
            return Ok(());
        }
        let barrier_id = barrier.barrier_id;
        let watermark = barrier.watermark;

        let mut outstanding = 0usize;
        for (_, state) in self.store.all_states()? {
            if state.max_local_seq() > 0
                && state.max_local_seq() <= watermark
                && state.has_local_work()
            {
                outstanding += 1;
            }
        }
        if outstanding > 0 {
            return Ok(());
        }

        let open_conflicts: Vec<String> = self
            .control
            .as_ref()
            .map(|c| {
                c.conflicts
                    .iter()
                    .filter(|x| {
                        x.status == ConflictStatus::Open && x.incoming_actor_id == self.actor_id
                    })
                    .map(|x| format!("{} — {}", x.short_id(), x.path))
                    .collect()
            })
            .unwrap_or_default();
        let has_conflict = self.barrier.as_ref().map(|b| b.conflicted).unwrap_or(false)
            || !open_conflicts.is_empty();

        let detail = if has_conflict {
            if open_conflicts.is_empty() {
                "unresolved conflict from pre-barrier work".to_string()
            } else {
                open_conflicts.join("; ")
            }
        } else {
            String::new()
        };
        self.send(ClientMessage::BarrierReady {
            barrier_id,
            ok: !has_conflict,
            detail,
        });
        if let Some(barrier) = &mut self.barrier {
            barrier.ready_sent = true;
        }
        Ok(())
    }

    // ----------------------------------------------------------- publication

    /// Install exact host-produced Git objects and advance local Git state,
    /// journalling each step (specification sections 131-135).
    fn apply_publication(
        &mut self,
        publication: GitPublication,
        pack: Option<Vec<u8>>,
    ) -> Result<()> {
        let root = self.paths.repo_root.clone();
        let descriptor = publication.descriptor.clone();

        if self.store.publication_journal_stage(publication.sequence)?
            == Some(PublicationStage::Complete)
        {
            self.store
                .set_last_publication_sequence(publication.sequence)?;
            self.expected_head = descriptor.commit_oid.clone();
            return Ok(());
        }

        self.store
            .put_publication_journal(&publication, PublicationStage::Pending)?;

        if let Some(pack) = pack {
            if !gitx::object_exists(&root, &descriptor.commit_oid)? {
                gitx::unpack_objects(&root, &pack)?;
            }
        }
        // Specification section 132: verify before touching branch metadata.
        if !gitx::object_exists(&root, &descriptor.commit_oid)? {
            return Err(crate::error::integrity(format!(
                "The published commit {} is missing after installing the host's Git objects.",
                crate::util::short_oid(&descriptor.commit_oid)
            )));
        }
        let tree = gitx::rev_parse(&root, &format!("{}^{{tree}}", descriptor.commit_oid))?;
        if tree.as_deref() != Some(descriptor.tree_oid.as_str()) {
            return Err(crate::error::integrity(
                "The published commit does not carry the tree named in its descriptor.",
            ));
        }
        self.store
            .put_publication_journal(&publication, PublicationStage::ObjectsInstalled)?;

        let refname = format!("refs/heads/{}", descriptor.branch);
        let current = gitx::rev_parse(&root, &refname)?;
        if current.as_deref() != Some(descriptor.commit_oid.as_str()) {
            gitx::update_ref_cas(
                &root,
                &refname,
                &descriptor.commit_oid,
                Some(&descriptor.parent_commit_oid),
            )?;
        }
        self.expected_head = descriptor.commit_oid.clone();
        self.store
            .put_publication_journal(&publication, PublicationStage::RefUpdated)?;

        // The index moves to the published tree; live working-tree files after
        // the target revision stay exactly where they are (section 134).
        gitx::read_tree_into_index(&root, &descriptor.tree_oid)?;
        self.store
            .put_publication_journal(&publication, PublicationStage::IndexUpdated)?;
        self.store
            .put_publication_journal(&publication, PublicationStage::Complete)?;
        self.store
            .set_last_publication_sequence(publication.sequence)?;

        self.note(format!(
            "Git publication installed: {} ({})",
            crate::util::short_oid(&descriptor.commit_oid),
            crate::util::fmt_revision(descriptor.target_revision)
        ));
        Ok(())
    }

    /// Finish or repair a publication interrupted by a crash
    /// (specification sections 135, 195).
    pub fn repair_publications(&mut self) -> Result<Vec<String>> {
        let mut repaired = Vec::new();
        for (publication, stage) in self.store.incomplete_publications()? {
            let oid = publication.descriptor.commit_oid.clone();
            self.apply_publication(publication, None)?;
            repaired.push(format!(
                "resumed publication {} from stage {}",
                crate::util::short_oid(&oid),
                stage.as_str()
            ));
        }
        Ok(repaired)
    }

    // ----------------------------------------------------------------- helpers

    fn send(&mut self, message: ClientMessage) {
        if let Some(out) = &self.out {
            if !out.send_client(message) {
                self.connected = false;
                self.connection_note = "outbound queue overflow".into();
                self.out = None;
            }
        }
    }

    fn complete_request(&mut self, request_id: Uuid, response: IpcResponse) {
        if let Some(pending) = self.requests.remove(&request_id) {
            let _ = pending.reply.send(response);
        }
    }

    fn my_active_task(&self) -> Option<&Task> {
        self.control.as_ref().and_then(|c| {
            c.tasks
                .iter()
                .find(|t| t.actor_id == self.actor_id && t.status == TaskStatus::Active)
        })
    }

    fn tasks(&self) -> Vec<Task> {
        self.control
            .as_ref()
            .map(|c| c.tasks.clone())
            .unwrap_or_default()
    }

    fn conflicts(&self) -> Vec<Conflict> {
        self.control
            .as_ref()
            .map(|c| c.conflicts.clone())
            .unwrap_or_default()
    }

    fn find_task(&self, needle: &str) -> Result<Task> {
        find_by_id(self.tasks(), needle, |t| t.id, |t| t.short_id(), "Task")
    }

    fn find_conflict(&self, needle: &str) -> Result<Conflict> {
        find_by_id(
            self.conflicts(),
            needle,
            |c| c.id,
            |c| c.short_id(),
            "conflict",
        )
    }

    fn require_connection(&self) -> Result<()> {
        if !self.connected {
            return Err(
                crate::error::network("Not connected to the Weave host.").with_detail(
                    "Local editing continues and local changes are queued. Retry when the session \
                 reconnects.",
                ),
            );
        }
        Ok(())
    }
}

fn find_by_id<T: Clone>(
    items: Vec<T>,
    needle: &str,
    id_of: impl Fn(&T) -> Uuid,
    short_of: impl Fn(&T) -> String,
    label: &str,
) -> Result<T> {
    let needle_trim = needle.trim();
    if let Ok(uuid) = Uuid::parse_str(needle_trim) {
        if let Some(found) = items.iter().find(|item| id_of(item) == uuid) {
            return Ok(found.clone());
        }
    }
    let upper = needle_trim.to_uppercase();
    let matches: Vec<&T> = items
        .iter()
        .filter(|item| {
            let short = short_of(item);
            short.eq_ignore_ascii_case(needle_trim)
                || short
                    .trim_start_matches(['C', 'T', 'P', '-'])
                    .eq_ignore_ascii_case(&upper)
                || id_of(item)
                    .simple()
                    .to_string()
                    .to_uppercase()
                    .starts_with(&upper)
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(
            crate::error::usage(format!("No Weave {label} matches `{needle_trim}`.")).with_detail(
                format!("Run `weave {label} list` to see the current identifiers.").to_lowercase(),
            ),
        ),
        _ => Err(crate::error::usage(format!(
            "`{needle_trim}` matches {} {label}s; use the full identifier.",
            matches.len()
        ))),
    }
}
