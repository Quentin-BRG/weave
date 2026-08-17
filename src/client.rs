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
use crate::blobwire::{self, BlobReceiver, Delivered, TransferIds};
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
use crate::transport::{BlobPump, DataFrame, Outbound, PumpJob};
use crate::watch::WatchEvent;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[path = "client_ipc.rs"]
mod ipc_commands;

#[path = "client_blobs.rs"]
mod blob_traffic;

use blob_traffic::{BlobTraffic, Emitted};

const TICK_MS: u64 = 400;
const SAFETY_RESCAN_MS: i64 = 15_000;
const PRESENCE_INTERVAL_MS: i64 = 8_000;
const HEARTBEAT_INTERVAL_MS: i64 = 15_000;
const GIT_GUARD_INTERVAL_MS: i64 = 3_000;
const RETRY_INTERVAL_MS: i64 = 2_000;
/// An in-flight operation with no durable result after this long is resent.
const RESEND_AFTER_MS: i64 = 20_000;
/// How often unreachable blob-store content is swept up. Rare on purpose: the
/// pass walks the store, and nothing depends on it running promptly.
const GC_INTERVAL_MS: i64 = 15 * 60 * 1000;
/// Files this size and above are captured only once they have stopped
/// changing. Below it, a file is written in one go often enough that waiting
/// would cost more than the occasional wasted hash.
const STABILITY_THRESHOLD: u64 = 8 * 1024 * 1024;
/// How long a large file has to hold still - same size, same modification time
/// - before Weave believes whoever was writing it has finished.
const STABILITY_WINDOW_MS: i64 = 1_000;

pub struct IpcCall {
    pub command: IpcCommand,
    pub reply: tokio::sync::oneshot::Sender<IpcResponse>,
}

pub enum ClientInput {
    Watch(WatchEvent),
    Host(HostMessage),
    /// One blob-plane frame from the host.
    Data(DataFrame),
    Connected {
        out: Outbound,
        pump: BlobPump,
    },
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

/// What a large file looked like when it was last checked for stability.
#[derive(Debug, Clone)]
struct Observation {
    size: u64,
    mtime_ms: i64,
    at_ms: i64,
    /// The canonical state this local change is based on, as it stood when the
    /// file was first seen changing. Waiting for a large file to settle must
    /// not turn a concurrent edit into a sequential one: whatever canonical
    /// state arrives while it is still being written is competing with it.
    base_revision: u64,
    base_entry: Option<FileEntry>,
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
    pump: Option<BlobPump>,
    traffic: BlobTraffic,
    connected: bool,
    connection_note: String,

    control: Option<ControlSnapshot>,
    peers: Vec<PeerInfo>,
    host_state: SyncState,
    local_state: SyncState,

    op_index: HashMap<Uuid, RepoPath>,
    pending_revisions: BTreeMap<u64, Revision>,
    barrier: Option<BarrierLocal>,
    requests: HashMap<Uuid, PendingRequest>,
    /// Publications whose Git pack is still travelling on the blob plane.
    awaiting_pack: Vec<(String, GitPublication)>,
    /// Conflicted paths whose canonical content is still travelling.
    ///
    /// These cannot ride on the ordinary rematerialization sweep: a path in
    /// conflict draft mode counts as local work, so `materialize_if_safe` will
    /// never touch it. Restoring canonical content over a conflict is a
    /// deliberate act, and it has to be retried deliberately too.
    awaiting_restore: Vec<RepoPath>,

    rejected_paths: Vec<RejectedPath>,
    notices: Vec<String>,
    expected_head: String,

    /// Metadata cache in front of the hash, so a rescan of an unchanged
    /// repository costs a stat per file rather than a read.
    scan_cache: scan::ScanCache,
    /// Large files seen mid-write, and what they looked like at the time.
    unstable: HashMap<RepoPath, Observation>,
    /// The session's file size limit, as of the last control snapshot.
    ///
    /// Cached out of `control` because it is consulted on every capture,
    /// including before the first snapshot of a session arrives: the durable
    /// value from the last connection is a far better answer there than the
    /// compiled-in default.
    max_file_size: u64,
    /// The oversize set as the host was last told it, so an unchanged
    /// situation does not produce a message every tick.
    reported_oversize: Option<Vec<OversizeReport>>,
    /// Set when a first join cannot proceed: the daemon exits with this rather
    /// than starting a session it cannot represent.
    fatal: Option<tokio::sync::mpsc::Sender<WeaveError>>,

    last_rescan_ms: i64,
    last_presence_ms: i64,
    last_heartbeat_ms: i64,
    last_git_check_ms: i64,
    last_retry_ms: i64,
    last_gc_ms: i64,
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
            traffic: BlobTraffic::new(blobs.clone()),
            blobs,
            actor_id,
            display_name,
            git_name,
            git_email,
            role,
            session,
            branch,
            out: None,
            pump: None,
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
            awaiting_pack: Vec::new(),
            awaiting_restore: Vec::new(),
            rejected_paths: Vec::new(),
            notices: Vec::new(),
            expected_head,
            scan_cache: scan::ScanCache::new(),
            unstable: HashMap::new(),
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            reported_oversize: None,
            fatal: None,
            last_rescan_ms: 0,
            last_presence_ms: 0,
            last_heartbeat_ms: 0,
            last_git_check_ms: 0,
            last_retry_ms: 0,
            // Not zero: the first sweep waits a full interval, by which time
            // this replica has its manifest and knows what it is keeping.
            last_gc_ms: crate::util::now_ms(),
            heartbeat_nonce: 0,
            git_problem_pending: false,
            materialization_blocked: false,
            shutdown: false,
        }
    }

    /// Where to report a condition that must end the session rather than
    /// degrade it. Set by the participant daemon before the engine starts.
    pub fn set_fatal_channel(&mut self, tx: tokio::sync::mpsc::Sender<WeaveError>) {
        self.fatal = Some(tx);
    }

    /// The session limit this replica last learned, so the very first scan of
    /// a reconnecting daemon judges files against the session's value and not
    /// against the default.
    pub fn load_cached_limit(&mut self) -> Result<()> {
        if let Some(control) = self.store.control_cache()? {
            self.max_file_size = control.max_file_size;
        }
        Ok(())
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
            ClientInput::Data(frame) => {
                let emitted = self.traffic.on_data(&frame);
                // The permit this frame carries is released here, once the
                // chunk is on disk, so the sender is paced by the slower of the
                // network and this replica's storage.
                drop(frame);
                self.emit(emitted)
            }
            ClientInput::Connected { out, pump } => self.on_connected(out, pump),
            ClientInput::Disconnected(reason) => {
                self.connected = false;
                self.connection_note = reason;
                self.out = None;
                self.pump = None;
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
        let result = scan::scan_repository(
            &self.paths.repo_root,
            &previous,
            &self.blobs,
            &mut self.scan_cache,
            self.max_file_size,
        )?;
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

    fn on_connected(&mut self, out: Outbound, pump: BlobPump) -> Result<()> {
        self.out = Some(out);
        self.pump = Some(pump);
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
        // Transfers do not survive the socket they were negotiated on.
        let emitted = self.traffic.reconnected();
        self.emit(emitted)?;
        // A full rescan on (re)connect is mandatory (specification section 32):
        // anything edited while Weave was not watching is captured here.
        self.full_rescan()?;
        // The host rebuilds its picture of this replica from this replica, so
        // an oversize file found while offline is announced on arrival rather
        // than waiting for something to change.
        self.reported_oversize = None;
        self.sync_oversize_report()?;
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
        // A file that was still being written when it was last looked at gets
        // looked at again, rather than waiting for the next full rescan.
        if !self.unstable.is_empty() && !self.paused() {
            let watched: Vec<RepoPath> = self.unstable.keys().cloned().collect();
            for path in &watched {
                if let Err(e) = self.capture_path(path) {
                    self.unstable.remove(path);
                    self.note_rejected(path.as_str(), &e.message);
                }
            }
            // Anything that settled may have been holding canonical content out
            // of the working tree while it did.
            if watched.iter().any(|p| !self.unstable.contains_key(p)) {
                self.sync_working_tree()?;
            }
        }
        if now - self.last_retry_ms >= RETRY_INTERVAL_MS {
            self.last_retry_ms = now;
            self.flush_outbox()?;
            // Whatever produced a change - a watcher batch, a rescan, a limit
            // that moved - the host hears about it from one place.
            self.sync_oversize_report()?;
            if self.materialization_blocked {
                self.sync_working_tree()?;
            }
            // A barrier held open by outstanding work of any kind - an
            // unanswered operation, or content still in transit - is answered
            // as soon as that work is done, without waiting for the event that
            // finished it to remember to ask.
            self.check_barrier_ready()?;
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
        if now - self.last_gc_ms >= GC_INTERVAL_MS {
            self.last_gc_ms = now;
            // Reclaiming space is never worth failing a session over.
            if let Err(e) = self.collect_garbage() {
                tracing::warn!("blob collection failed: {}", e.message);
            }
        }
        Ok(())
    }

    /// Reclaim blob-store content nothing can reach any more.
    ///
    /// Runs on the replica because there is exactly one per daemon, whatever
    /// its role. On a host machine the coordinator shares this blob store, and
    /// keeps content the replica has no name for - the far side of every
    /// revision, entries from conflicts, the manifest a late joiner will be
    /// sent - so its half of the live set is read from the host database
    /// rather than guessed at.
    ///
    /// Publication packs are deliberately not in anyone's live set. They are
    /// derived from Git objects the repository still holds, and the host
    /// rebuilds one whenever it announces a publication, including to a
    /// participant reconnecting after being away.
    fn collect_garbage(&mut self) -> Result<()> {
        let mut live = self.store.referenced_blobs()?;
        live.extend(crate::store_host::referenced_blobs_at(
            &self.paths.host_db(),
        )?);
        if let Some(control) = &self.control {
            for conflict in &control.conflicts {
                for entry in [
                    &conflict.base_entry,
                    &conflict.canonical_entry,
                    &conflict.incoming_entry,
                    &conflict.latest_local_candidate,
                ]
                .into_iter()
                .flatten()
                {
                    live.insert(entry.blob_hash.clone());
                }
            }
        }
        // A pack that has arrived but whose publication is still queued behind
        // an earlier one.
        for (hash, _) in &self.awaiting_pack {
            live.insert(hash.clone());
        }
        let report = self
            .blobs
            .collect_garbage(&live, crate::blobs::GC_GRACE_MS)?;
        if !report.is_empty() {
            tracing::info!(
                "reclaimed {} blob(s) ({} bytes), {} partial(s), {} temporary file(s)",
                report.blobs,
                report.bytes,
                report.partials,
                report.temps
            );
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
        let result = scan::scan_repository(
            &root,
            &previous,
            &self.blobs,
            &mut self.scan_cache,
            self.max_file_size,
        )?;
        self.rejected_paths = result.rejected;

        let mut seen: HashSet<RepoPath> = result.entries.keys().cloned().collect();
        // Above the limit: present, untouched, and recorded as the reason the
        // session cannot be published. Marked seen so the vanished-path pass
        // below does not read a file it deliberately did not read.
        for (path, size) in &result.oversize {
            seen.insert(path.clone());
            self.record_oversize(path, *size)?;
        }
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

    /// True when `path` is in a state worth capturing.
    ///
    /// A large file exists on disk long before it is finished: an editor
    /// flushing 40 MiB, a download, a copy across a network share. Hashing it
    /// mid-write would capture a prefix, publish it to everyone as canonical
    /// state, and then be superseded seconds later by the real thing - having
    /// moved the prefix across the network first. So a large file has to hold
    /// still, which it does either by not having been touched for a whole
    /// window, or - for timestamps too coarse or too skewed for that to mean
    /// anything - by being seen twice with the same size and time, a window
    /// apart.
    ///
    /// Small files skip this entirely. They are written in one go often enough
    /// that a spurious capture is rare, and cheap when it happens.
    ///
    /// A path that has disappeared settles immediately: a deletion is not
    /// something to wait out.
    fn has_settled(&mut self, path: &RepoPath) -> bool {
        let fs_path = path.to_fs_path(&self.paths.repo_root);
        let Ok(meta) = std::fs::symlink_metadata(&fs_path) else {
            self.unstable.remove(path);
            return true;
        };
        if !meta.is_file() || meta.len() < STABILITY_THRESHOLD {
            self.unstable.remove(path);
            return true;
        }
        let Some(mtime_ms) = scan::mtime_ms(&meta) else {
            // Without a usable timestamp there is nothing to compare, so this
            // degrades to the old behaviour rather than waiting forever.
            self.unstable.remove(path);
            return true;
        };
        let now = crate::util::now_ms();
        // Nothing has written to it for a whole window: it is finished, and no
        // second observation is needed to say so. Without this, every check of
        // a quiescent large file would look like a first sighting again and the
        // file would never be treated as settled twice running.
        if now - mtime_ms >= STABILITY_WINDOW_MS {
            self.unstable.remove(path);
            return true;
        }
        match self.unstable.get(path) {
            Some(seen) if seen.size == meta.len() && seen.mtime_ms == mtime_ms => {
                // Unchanged since it was first seen this way. The window is
                // measured from that first sighting, not from this one.
                if now - seen.at_ms >= STABILITY_WINDOW_MS {
                    self.unstable.remove(path);
                    return true;
                }
                false
            }
            other => {
                // The base is frozen at the first sighting of this episode and
                // carried across every later one: a canonical revision that
                // arrives while the file is still changing is concurrent with
                // what is being written, not a base for it.
                let base = other.map(|seen| (seen.base_revision, seen.base_entry.clone()));
                let (base_revision, base_entry) = base.unwrap_or_else(|| {
                    let state = self.store.path_state(path).unwrap_or_default();
                    (state.confirmed_revision, state.confirmed)
                });
                self.unstable.insert(
                    path.clone(),
                    Observation {
                        size: meta.len(),
                        mtime_ms,
                        at_ms: now,
                        base_revision,
                        base_entry,
                    },
                );
                false
            }
        }
    }

    /// Durably capture the current on-disk state of `path` if it differs from
    /// what Weave last materialized.
    ///
    /// This is both the watcher path and the capture-before-overwrite check
    /// (specification sections 36, 44). Comparing against `materialized` is
    /// what makes a Weave-written file not echo back as a local edit, and it
    /// does so by content rather than by timer.
    fn capture_path(&mut self, path: &RepoPath) -> Result<bool> {
        // Read before `has_settled`, which clears the entry once it settles.
        let frozen = self
            .unstable
            .get(path)
            .map(|seen| (seen.base_revision, seen.base_entry.clone()));
        if !self.has_settled(path) {
            return Ok(false);
        }
        // Judged from the filesystem's own size, before anything is read: a
        // file the session cannot carry must not be hashed into the blob store
        // on its way to being refused.
        if self.check_oversize(path)? {
            return Ok(false);
        }
        let mut state = self.store.path_state(path)?;
        let previous = state
            .materialized
            .clone()
            .or_else(|| state.confirmed.clone());
        // Reading the path also stores its content, so by the time the entry
        // exists the blob it names is durable.
        let entry = scan::read_path(
            &self.paths.repo_root,
            path,
            previous.as_ref(),
            &self.blobs,
            &mut self.scan_cache,
        )?;

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
        let (base_revision, base_entry) =
            frozen.unwrap_or_else(|| (state.confirmed_revision, state.confirmed.clone()));
        state.in_flight = Some(InFlight {
            operation_id,
            base_revision,
            base_entry,
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
        let op = FileOperation {
            operation_id: flight.operation_id,
            actor_id: self.actor_id,
            task_id: flight.task_id,
            local_seq: flight.local_seq,
            base_revision: flight.base_revision,
            base_entry: flight.base_entry.clone(),
            path: path.clone(),
            desired_entry: flight.desired.clone(),
        };
        // Upload before submit: the host accepts an operation only once the
        // content it names is durable there, so the operation waits behind its
        // own blob instead of the host parking it on arrival.
        let needs: Vec<String> = flight
            .desired
            .iter()
            .map(|entry| entry.blob_hash.clone())
            .collect();
        let emitted = self.traffic.send_when_uploaded(
            flight.operation_id,
            needs,
            ClientMessage::SubmitOperation {
                operation: Box::new(op),
            },
        );
        self.emit(emitted)?;
        // Marked sent even while the submission sits behind its upload: the
        // resend timer is what recovers a transfer that never completes, and it
        // replaces the deferred message rather than queueing a second one.
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

    // -------------------------------------------------------- the size limit

    /// Is `path` above the session limit, and record the answer either way.
    ///
    /// The whole state machine of `docs/BLOB-PLANE.md` section 4 turns on this
    /// one question being asked before anything is read, on every capture
    /// attempt. Nothing here touches the file: an oversize file is preserved
    /// exactly, never hashed, never copied, never rewritten.
    fn check_oversize(&mut self, path: &RepoPath) -> Result<bool> {
        let fs_path = path.to_fs_path(&self.paths.repo_root);
        let size = match std::fs::symlink_metadata(&fs_path) {
            Ok(meta) if meta.is_file() => meta.len(),
            // Gone, or not a file at all: whatever this path was, it is not an
            // oversize file now. Deleting is one of the two ways out.
            _ => {
                self.forget_oversize(path)?;
                return Ok(false);
            }
        };
        if size <= self.max_file_size {
            self.forget_oversize(path)?;
            return Ok(false);
        }
        self.record_oversize(path, size)?;
        Ok(true)
    }

    /// Durably record that `path` is being held back, and say so once.
    fn record_oversize(&mut self, path: &RepoPath, size: u64) -> Result<()> {
        let state = self.store.path_state(path)?;
        let canonical = state.confirmed.is_some();
        let known = self.store.is_oversize(path)?;
        self.store.put_oversize(path, size, canonical)?;
        if !known {
            let limit = crate::util::format_size(self.max_file_size);
            let actual = crate::util::format_size(size);
            self.note(if canonical {
                format!(
                    "{path} has grown to {actual}, above the session limit of {limit}. Your copy \
                     is preserved and untouched, the session still holds the previous content, \
                     and Git publication is blocked until this is resolved. Shrink the file, or \
                     raise the limit with `weave limit set <size>`."
                )
            } else {
                format!(
                    "{path} is {actual}, above the session limit of {limit}. It stays exactly as \
                     it is on this machine and is not synchronized, and Git publication is \
                     blocked until this is resolved. Shrink or delete the file, or raise the \
                     limit with `weave limit set <size>`."
                )
            });
        }
        Ok(())
    }

    /// Drop any oversize record for `path`, and say so once when there was one.
    fn forget_oversize(&mut self, path: &RepoPath) -> Result<()> {
        if self.store.clear_oversize(path)? {
            self.note(format!(
                "{path} is within the session file size limit again and is synchronized normally."
            ));
        }
        Ok(())
    }

    /// Tell the host what this replica is holding back, when that has changed.
    ///
    /// Sent as a whole set rather than as deltas: the host's picture of one
    /// participant is replaced by the participant's own, so there is no way for
    /// the two to drift apart.
    fn sync_oversize_report(&mut self) -> Result<()> {
        if !self.connected {
            return Ok(());
        }
        let current = self.store.oversize()?;
        if self.reported_oversize.as_ref() == Some(&current) {
            return Ok(());
        }
        self.reported_oversize = Some(current.clone());
        self.send(ClientMessage::ReportOversize { paths: current });
        // A file blocking publication is the sort of thing a barrier is
        // waiting to hear about.
        self.check_barrier_ready()
    }

    /// The authoritative check a join cannot make before it connects.
    ///
    /// `weave join` compares against the default beforehand, but only the
    /// session knows its own limit, and it arrives with `Welcome`. A machine
    /// that cannot represent the session's state from its first moment does not
    /// enter it half-way: the daemon exits and says why. Once a replica is
    /// established, the same situation is the ordinary oversize condition -
    /// reported, visible, blocking publication - because there is then a
    /// session to be part of and work not to throw away.
    fn verify_join_against_limit(&mut self) -> Result<()> {
        let mine = self.store.oversize()?;
        let Some(first) = mine.first() else {
            return Ok(());
        };
        let list: Vec<String> = mine
            .iter()
            .map(|item| format!("{} — {}", item.path, crate::util::format_size(item.size)))
            .collect();
        let error = crate::error::unsupported(format!(
            "Cannot join this Weave session: {} is above the session file size limit of {}.",
            first.path,
            crate::util::format_size(self.max_file_size)
        ))
        .with_detail(format!(
            "{}\n\nRemove these files from the repository, or ask the host to raise the limit \
             with `weave limit set <size>`, then join again.",
            list.join("\n")
        ));
        match &self.fatal {
            Some(tx) => {
                let _ = tx.try_send(error);
                self.shutdown = true;
            }
            None => {
                // No channel to end on - the host's own replica, which was
                // checked before the session started. Refuse to run degraded.
                self.local_state = SyncState::Degraded {
                    reason: error.message.clone(),
                };
            }
        }
        Ok(())
    }

    fn oversize_detail(&self) -> Result<Option<String>> {
        let mine = self.store.oversize()?;
        if mine.is_empty() {
            return Ok(None);
        }
        let list: Vec<String> = mine
            .iter()
            .map(|item| {
                format!(
                    "{} is {}, above the session limit of {}",
                    item.path,
                    crate::util::format_size(item.size),
                    crate::util::format_size(self.max_file_size)
                )
            })
            .collect();
        Ok(Some(list.join("; ")))
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
                // Read before the manifest is installed, which is what makes a
                // replica stop being new.
                let first_join = !self.store.has_manifest()?;
                self.session = session.clone();
                self.store.set_session(&session)?;
                if let Some(manifest) = manifest {
                    self.apply_manifest(snapshot_revision, manifest, &host_state_hash)?;
                }
                self.apply_control(*control)?;
                if first_join {
                    self.verify_join_against_limit()?;
                }
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
            HostMessage::RevisionBroadcast { revision } => self.enqueue_revision(*revision),
            HostMessage::OperationResult {
                operation_id,
                outcome,
            } => self.on_operation_result(operation_id, *outcome),
            HostMessage::Blob { blob } => {
                let emitted = self.traffic.on_control(blob);
                self.emit(emitted)
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
                pack_hash,
            } => self.on_publication(*publication, pack_hash),
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
        let missing: Vec<String> = entries
            .filter(|entry| !self.blobs.has(&entry.blob_hash))
            .map(|entry| entry.blob_hash.clone())
            .collect();
        self.want_blobs(missing)
    }

    /// Ask the host for content the working tree needs.
    ///
    /// Everything funnels through here so that one queue decides how many
    /// transfers are open at once, whatever revealed the gap.
    fn want_blobs<I: IntoIterator<Item = String>>(&mut self, hashes: I) -> Result<()> {
        let emitted = self.traffic.want(hashes);
        self.emit(emitted)?;
        if self.traffic.waiting_for_content() {
            self.materialization_blocked = true;
        }
        Ok(())
    }

    /// Carry out the consequences of one blob-plane event.
    fn emit(&mut self, emitted: Emitted) -> Result<()> {
        if emitted.is_empty() {
            return Ok(());
        }
        for message in emitted.messages {
            self.send(message);
        }
        if let Some(pump) = self.pump.clone() {
            for job in emitted.jobs {
                pump.start(job.transfer_id, job.hash, job.from_offset);
            }
        }
        for (hash, reason) in emitted.refused {
            self.note(format!(
                "content {} could not be transferred: {reason}",
                crate::util::short_oid(&hash)
            ));
        }
        if !emitted.installed.is_empty() {
            for hash in emitted.installed {
                self.install_awaited_publications(&hash)?;
            }
            for path in std::mem::take(&mut self.awaiting_restore) {
                self.restore_canonical_for_conflict(&path)?;
            }
            self.sync_working_tree()?;
            // The last blob a barrier was waiting for may have just arrived.
            self.check_barrier_ready()?;
        }
        Ok(())
    }

    /// True while anything this replica needs is still in transit.
    ///
    /// Three separate queues, one question. A blob the working tree is waiting
    /// for, a Git pack a publication cannot be applied without, and a canonical
    /// file that has to overwrite a conflict draft all mean the same thing: the
    /// replica cannot currently reproduce the state it has been told about.
    fn waiting_for_content(&self) -> bool {
        self.materialization_blocked
            || self.traffic.waiting_for_content()
            || !self.awaiting_pack.is_empty()
            || !self.awaiting_restore.is_empty()
    }

    fn apply_control(&mut self, control: ControlSnapshot) -> Result<()> {
        self.store.set_control_cache(&control)?;
        let limit_changed = control.max_file_size != self.max_file_size;
        self.max_file_size = control.max_file_size;
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

        // A new limit re-decides every path, in both directions: what was too
        // large may now be ordinary work to capture, and what was ordinary may
        // now be held back. The set is discarded rather than filtered so the
        // rescan derives it afresh from the files themselves.
        if limit_changed {
            self.note(format!(
                "The session file size limit is now {}.",
                crate::util::format_size(self.max_file_size)
            ));
            self.store.clear_all_oversize()?;
            self.full_rescan()?;
            self.sync_oversize_report()?;
        }
        Ok(())
    }

    // ------------------------------------------------------------- revisions

    fn enqueue_revision(&mut self, revision: Revision) -> Result<()> {
        let last = self.store.last_applied_revision()?;
        if revision.revision <= last {
            return Ok(());
        }
        self.pending_revisions.insert(revision.revision, revision);
        self.drain_pending_revisions()
    }

    /// Apply buffered revisions strictly in order. The watermark means "every
    /// revision up to and including this one has been applied", never "the
    /// highest revision seen" (specification section 105).
    fn drain_pending_revisions(&mut self) -> Result<()> {
        loop {
            let last = self.store.last_applied_revision()?;
            let Some(revision) = self.pending_revisions.remove(&(last + 1)) else {
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
        // Capture may have declined because the file is large and has not held
        // still yet. Nothing has been recorded from it, so overwriting it here
        // would discard whatever is being written into it - the one thing Weave
        // may never do. The path is re-examined every tick and materialized as
        // soon as it settles, one way or the other.
        if self.unstable.contains_key(path) {
            return Ok(());
        }
        // Or it declined because the local file is above the session limit.
        // Its content was never captured, so canonical content written over it
        // would be a silent deletion of bytes only this machine holds. The
        // session keeps the previous content and refuses to publish; this
        // working file is left exactly as its author last wrote it.
        if self.store.is_oversize(path)? {
            return Ok(());
        }
        self.write_canonical(path)
    }

    fn write_canonical(&mut self, path: &RepoPath) -> Result<()> {
        let mut state = self.store.path_state(path)?;
        match &state.confirmed {
            Some(entry) => {
                if !self.blobs.has(&entry.blob_hash) {
                    let hash = entry.blob_hash.clone();
                    self.want_blobs([hash])?;
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
                let missing: Vec<String> = canonical.iter().map(|e| e.blob_hash.clone()).collect();
                self.want_blobs(missing)?;
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
                // Both sides of the conflict have to reach the host before the
                // report that names them, or the host would hold a conflict it
                // cannot show anybody.
                let needs: Vec<String> = [base.as_ref(), pending.desired.as_ref()]
                    .into_iter()
                    .flatten()
                    .filter(|entry| self.blobs.has(&entry.blob_hash))
                    .map(|entry| entry.blob_hash.clone())
                    .collect();
                let emitted = self.traffic.send_when_uploaded(
                    conflict_id,
                    needs,
                    ClientMessage::ReportConflict {
                        report: Box::new(ConflictReport {
                            id: conflict_id,
                            path: path.clone(),
                            kind,
                            base_entry: base.clone(),
                            canonical_entry: canonical.clone(),
                            incoming_entry: pending.desired.clone(),
                            latest_local_candidate: pending.desired.clone(),
                            incoming_task_id: pending.task_id,
                        }),
                    },
                );
                self.emit(emitted)?;
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
            let needs: Vec<String> = pending
                .desired
                .iter()
                .filter(|entry| self.blobs.has(&entry.blob_hash))
                .map(|entry| entry.blob_hash.clone())
                .collect();
            let emitted = self.traffic.send_when_uploaded(
                conflict_id,
                needs,
                ClientMessage::AttachLocalCandidate {
                    conflict_id,
                    entry: pending.desired.clone(),
                },
            );
            self.emit(emitted)?;
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
                    if !self.awaiting_restore.contains(path) {
                        self.awaiting_restore.push(path.clone());
                    }
                    self.want_blobs([entry.blob_hash.clone()])?;
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

        // Content still on its way is unfinished work like any other. A
        // participant whose working tree is waiting for blobs has not converged
        // on canonical state, so answering the barrier would let a publication
        // commit a tree this replica cannot yet reproduce. Say nothing and wait:
        // `emit` re-checks on every install, and the retry tick re-checks
        // anyway, so readiness follows the last byte rather than a timeout.
        if self.waiting_for_content() {
            return Ok(());
        }

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

        let mut blockers: Vec<String> = Vec::new();
        if has_conflict {
            if open_conflicts.is_empty() {
                blockers.push("unresolved conflict from pre-barrier work".to_string());
            } else {
                blockers.push(open_conflicts.join("; "));
            }
        }
        // A file this replica is holding back is state the session cannot
        // reproduce here. Answering "ready" would let a publication commit a
        // tree this machine's disk does not match.
        if let Some(detail) = self.oversize_detail()? {
            blockers.push(detail);
        }

        self.send(ClientMessage::BarrierReady {
            barrier_id,
            ok: blockers.is_empty(),
            detail: blockers.join("; "),
        });
        if let Some(barrier) = &mut self.barrier {
            barrier.ready_sent = true;
        }
        Ok(())
    }

    // ----------------------------------------------------------- publication

    /// Install exact host-produced Git objects and advance local Git state,
    /// journalling each step (specification sections 131-135).
    /// A publication whose pack has not arrived yet waits for it.
    ///
    /// The pack travels on the blob plane like any other content, so a
    /// publication carrying a large file no longer inflates a control message.
    fn on_publication(
        &mut self,
        publication: GitPublication,
        pack_hash: Option<String>,
    ) -> Result<()> {
        match &pack_hash {
            Some(hash) if !self.blobs.has(hash) => {
                if !self
                    .awaiting_pack
                    .iter()
                    .any(|(h, p)| h == hash && p.sequence == publication.sequence)
                {
                    self.awaiting_pack.push((hash.clone(), publication));
                }
                self.want_blobs([hash.clone()])
            }
            _ => self.apply_publication(publication, pack_hash),
        }
    }

    fn install_awaited_publications(&mut self, hash: &str) -> Result<()> {
        let mut ready = Vec::new();
        self.awaiting_pack.retain(|(want, publication)| {
            if want == hash {
                ready.push(publication.clone());
                false
            } else {
                true
            }
        });
        ready.sort_by_key(|p| p.sequence);
        for publication in ready {
            self.apply_publication(publication, Some(hash.to_string()))?;
        }
        Ok(())
    }

    fn apply_publication(
        &mut self,
        publication: GitPublication,
        pack_hash: Option<String>,
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

        if let Some(hash) = &pack_hash {
            if !gitx::object_exists(&root, &descriptor.commit_oid)? {
                let pack = self.blobs.path_of(hash)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_client::PathState;

    /// A replica engine over a scratch directory, with no transport attached.
    /// `send` is a no-op without one, so what a decision produced is read off
    /// the engine's own state rather than off the wire.
    struct Fixture {
        dir: std::path::PathBuf,
        engine: ClientEngine,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!("weave-client-{}", crate::util::random_hex(6)));
        let repo_root = dir.join("repo");
        let git_dir = repo_root.join(".git");
        std::fs::create_dir_all(&repo_root).unwrap();
        let paths = Paths {
            weave_dir: git_dir.join("weave"),
            git_dir,
            repo_root,
        };
        paths.ensure().unwrap();
        let store = ClientStore::open(&paths.client_db()).unwrap();
        let blobs = BlobStore::open(paths.blobs()).unwrap();
        let actor_id = Uuid::new_v4();
        let session = SessionInfo {
            session_id: Uuid::new_v4(),
            repo_name: "repo".into(),
            branch: "main".into(),
            base_commit: "0".repeat(40),
            host_actor_id: Uuid::new_v4(),
            host_display_name: "Host".into(),
            created_at_ms: crate::util::now_ms(),
        };
        let engine = ClientEngine::new(
            paths,
            store,
            blobs,
            actor_id,
            "Tester".into(),
            "Tester".into(),
            "tester@example.com".into(),
            Role::Participant,
            session,
            "main".into(),
            "0".repeat(40),
        );
        Fixture { dir, engine }
    }

    fn open_barrier(engine: &mut ClientEngine) {
        engine.barrier = Some(BarrierLocal {
            barrier_id: Uuid::new_v4(),
            watermark: 0,
            ready_sent: false,
            conflicted: false,
        });
    }

    fn answered(engine: &ClientEngine) -> bool {
        engine
            .barrier
            .as_ref()
            .map(|b| b.ready_sent)
            .unwrap_or(false)
    }

    /// A replica that is still receiving content has not converged on
    /// canonical state, whichever queue is holding it up. Answering the
    /// barrier would let a publication commit a tree this replica cannot yet
    /// reproduce.
    #[test]
    fn a_barrier_is_not_answered_while_content_is_still_arriving() {
        type Blocker = fn(&mut ClientEngine);
        let blockers: Vec<(&str, Blocker)> = vec![
            ("materialization waiting on a blob", |e| {
                e.materialization_blocked = true
            }),
            ("a publication pack still in transit", |e| {
                e.awaiting_pack.push(("f".repeat(64), sample_publication()))
            }),
            ("canonical content owed to a conflict", |e| {
                e.awaiting_restore.push(RepoPath::new("notes.md").unwrap())
            }),
        ];
        for (what, block) in blockers {
            let mut f = fixture();
            open_barrier(&mut f.engine);
            block(&mut f.engine);
            f.engine.check_barrier_ready().unwrap();
            assert!(!answered(&f.engine), "barrier answered despite {what}");
        }
    }

    /// And it is answered as soon as the last of it arrives.
    #[test]
    fn a_barrier_is_answered_once_the_content_has_arrived() {
        let mut f = fixture();
        open_barrier(&mut f.engine);
        f.engine.materialization_blocked = true;
        f.engine.check_barrier_ready().unwrap();
        assert!(!answered(&f.engine));

        f.engine.materialization_blocked = false;
        f.engine.check_barrier_ready().unwrap();
        assert!(answered(&f.engine), "nothing was outstanding any more");
    }

    fn sample_publication() -> GitPublication {
        GitPublication {
            descriptor: CommitDescriptor {
                prepare_id: Uuid::new_v4(),
                target_revision: 1,
                parent_commit_oid: "0".repeat(40),
                tree_oid: "1".repeat(40),
                commit_oid: "2".repeat(40),
                author_name: "Tester".into(),
                author_email: "tester@example.com".into(),
                committer_name: "Tester".into(),
                committer_email: "tester@example.com".into(),
                timestamp: 0,
                timezone: "+0000".into(),
                message: "commit".into(),
                contributing_task_ids: Vec::new(),
                branch: "main".into(),
            },
            stage: PublicationStage::Pending,
            push_status: PushStatus::NotAttempted,
            push_error: None,
            created_at_ms: 0,
            sequence: 1,
        }
    }

    // ------------------------------------------------------------ stability

    #[test]
    fn a_large_file_is_captured_only_once_it_stops_changing() {
        let mut f = fixture();
        let path = RepoPath::new("asset.bin").unwrap();
        let fs_path = f.engine.paths.repo_root.join("asset.bin");
        let size = STABILITY_THRESHOLD as usize;

        std::fs::write(&fs_path, vec![7u8; size]).unwrap();
        assert!(
            !f.engine.has_settled(&path),
            "first sighting proves nothing"
        );
        assert!(
            !f.engine.has_settled(&path),
            "the window has not elapsed yet"
        );

        // Still being written: the clock starts again.
        std::fs::write(&fs_path, vec![7u8; size + 4096]).unwrap();
        assert!(!f.engine.has_settled(&path));

        std::thread::sleep(Duration::from_millis(STABILITY_WINDOW_MS as u64 + 200));
        assert!(
            f.engine.has_settled(&path),
            "unchanged for a full window, so it is finished"
        );
        assert!(f.engine.unstable.is_empty());
    }

    /// Settling is not a one-shot event.
    ///
    /// Every path that materialization touches is checked for stability first,
    /// so a file that has finished must answer "settled" every time it is
    /// asked, not once. Answering "first sighting" again on the next check
    /// would defer materialization for that path forever, one tick at a time.
    #[test]
    fn a_file_that_has_finished_stays_settled() {
        let mut f = fixture();
        let path = RepoPath::new("asset.bin").unwrap();
        std::fs::write(
            f.engine.paths.repo_root.join("asset.bin"),
            vec![3u8; STABILITY_THRESHOLD as usize],
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(STABILITY_WINDOW_MS as u64 + 200));
        for round in 0..3 {
            assert!(
                f.engine.has_settled(&path),
                "an untouched file stopped being settled on check {round}"
            );
            assert!(f.engine.unstable.is_empty());
        }
    }

    /// Waiting to capture must never become licence to overwrite.
    ///
    /// A large file that has not settled has nothing recorded from it yet, so
    /// materializing canonical content over it would discard whatever is being
    /// written into it — no local work would exist to rebase, and the bytes
    /// would simply be gone. The write waits instead.
    #[test]
    fn a_file_that_is_still_being_written_is_not_overwritten_by_canonical_content() {
        let mut f = fixture();
        let path = RepoPath::new("asset.bin").unwrap();
        let fs_path = f.engine.paths.repo_root.join("asset.bin");

        // Canonical content that is present and ready to be written out, so a
        // materialization here would certainly succeed.
        let canonical = vec![1u8; 64];
        let hash = f.engine.blobs.put(&canonical).unwrap();
        let state = PathState {
            confirmed: Some(FileEntry {
                blob_hash: hash,
                size: canonical.len() as u64,
                git_mode: GitMode::Regular,
                file_kind: FileKind::Binary,
            }),
            confirmed_revision: 1,
            ..PathState::default()
        };
        f.engine.store.put_path_state(&path, &state).unwrap();

        // And a large local file that has only just been seen.
        let local = vec![9u8; STABILITY_THRESHOLD as usize];
        std::fs::write(&fs_path, &local).unwrap();

        f.engine.materialize_if_safe(&path).unwrap();
        assert_eq!(
            std::fs::read(&fs_path).unwrap().len(),
            local.len(),
            "canonical content overwrote a file that was still being written"
        );
        assert!(f.engine.unstable.contains_key(&path));

        // Once it holds still it is captured as local work, and still not
        // overwritten.
        std::thread::sleep(Duration::from_millis(STABILITY_WINDOW_MS as u64 + 200));
        f.engine.materialize_if_safe(&path).unwrap();
        assert_eq!(std::fs::read(&fs_path).unwrap().len(), local.len());
        assert!(f.engine.store.path_state(&path).unwrap().has_local_work());
    }

    /// Waiting is only worth it for content large enough to be written in
    /// pieces. Everything else - and every deletion - is captured at once.
    #[test]
    fn small_files_and_deletions_never_wait() {
        let mut f = fixture();
        let small = RepoPath::new("notes.md").unwrap();
        std::fs::write(f.engine.paths.repo_root.join("notes.md"), b"a line\n").unwrap();
        assert!(f.engine.has_settled(&small));

        let gone = RepoPath::new("removed.bin").unwrap();
        assert!(f.engine.has_settled(&gone));
        assert!(f.engine.unstable.is_empty());
    }

    // ------------------------------------------------------- the size limit

    /// A file above the limit is noticed without being read.
    ///
    /// Proved through the blob store: nothing about the file's content may
    /// reach it, because Weave promises to leave an oversize file entirely
    /// alone rather than hash it on its way to refusing it.
    #[test]
    fn a_file_above_the_limit_is_recorded_without_being_read() {
        let mut f = fixture();
        f.engine.max_file_size = 4096;
        let path = RepoPath::new("huge.bin").unwrap();
        let fs_path = f.engine.paths.repo_root.join("huge.bin");
        let bytes = vec![5u8; 16 * 1024];
        std::fs::write(&fs_path, &bytes).unwrap();

        let (before, _) = f.engine.blobs.stats().unwrap();
        assert!(!f.engine.capture_path(&path).unwrap(), "it was captured");
        let (after, _) = f.engine.blobs.stats().unwrap();
        assert_eq!(after, before, "an oversize file was read into the store");

        let held = f.engine.store.oversize().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].path, path);
        assert_eq!(held[0].size, bytes.len() as u64);
        assert_eq!(std::fs::read(&fs_path).unwrap(), bytes, "the file changed");

        // Back under the limit, it is ordinary work again, with nothing to undo.
        std::fs::write(&fs_path, vec![5u8; 128]).unwrap();
        assert!(f.engine.capture_path(&path).unwrap());
        assert!(f.engine.store.oversize().unwrap().is_empty());
    }

    /// The one divergence Weave tolerates must not become a lost edit.
    ///
    /// A path that is already canonical and then grows past the limit holds
    /// bytes no replica has. Writing canonical content over it would delete the
    /// only copy of them, so the write waits - and the barrier refuses, so the
    /// divergence cannot be published while it lasts.
    #[test]
    fn canonical_content_never_overwrites_a_file_that_grew_past_the_limit() {
        let mut f = fixture();
        f.engine.max_file_size = 4096;
        let path = RepoPath::new("cut.mov").unwrap();
        let fs_path = f.engine.paths.repo_root.join("cut.mov");

        // Canonical content, present and ready to be written out.
        let canonical = vec![1u8; 64];
        let hash = f.engine.blobs.put(&canonical).unwrap();
        let state = PathState {
            confirmed: Some(FileEntry {
                blob_hash: hash,
                size: canonical.len() as u64,
                git_mode: GitMode::Regular,
                file_kind: FileKind::Binary,
            }),
            confirmed_revision: 2,
            ..PathState::default()
        };
        f.engine.store.put_path_state(&path, &state).unwrap();

        let local = vec![9u8; 16 * 1024];
        std::fs::write(&fs_path, &local).unwrap();

        f.engine.materialize_if_safe(&path).unwrap();
        assert_eq!(
            std::fs::read(&fs_path).unwrap(),
            local,
            "canonical content overwrote a file Weave never captured"
        );
        let held = f.engine.store.oversize().unwrap();
        assert_eq!(held.len(), 1);
        assert!(
            held[0].canonical,
            "the session was not told it already holds content for this path"
        );

        // And publication is refused for as long as that is true.
        open_barrier(&mut f.engine);
        assert!(f.engine.oversize_detail().unwrap().is_some());

        // Shrinking resolves it, and the canonical content the replica was owed
        // is written out on the next sweep.
        std::fs::write(&fs_path, &canonical).unwrap();
        f.engine.materialize_if_safe(&path).unwrap();
        assert!(f.engine.store.oversize().unwrap().is_empty());
        assert!(f.engine.oversize_detail().unwrap().is_none());
    }
}
