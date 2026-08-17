// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire protocol (specification sections 52-54, 99-106, 131, 137).
//!
//! JSON over one long-lived WebSocket per participant. Every frame carries
//! `protocol_version` and `message_type`.
//!
//! No file content appears here. Messages carry `FileEntry` metadata, and the
//! bytes behind a `blob_hash` are streamed on the data plane and referenced by
//! hash — see [`crate::blobwire`] and `docs/BLOB-PLANE.md`. That is what makes
//! the size of a file a resource question rather than a protocol one.

use crate::error::ErrorClass;
use crate::model::*;
use crate::path::RepoPath;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Immutable session facts shared with every participant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: Uuid,
    pub repo_name: String,
    pub branch: String,
    pub base_commit: String,
    pub host_actor_id: Uuid,
    pub host_display_name: String,
    pub created_at_ms: i64,
}

/// One manifest row (specification section 19).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: RepoPath,
    pub entry: FileEntry,
}

/// The control half of a blob transfer, identical in both directions.
///
/// Both ends send and receive blobs — a participant uploads before submitting
/// the operation that references its content, and downloads canonical content
/// it does not hold — so one vocabulary serves both.
///
/// The exchange is: `Offer`, then `Want` or `Have` from the receiver, then the
/// bytes on the data plane, then `Done` or `Failed`. The receiver chooses the
/// starting offset because only it knows what it already holds, and the sender
/// waits for `Done` before relying on the blob being present at the far end.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "blob_message", rename_all = "snake_case")]
pub enum BlobControl {
    Offer {
        transfer_id: u64,
        hash: String,
        size: u64,
    },
    Want {
        transfer_id: u64,
        from_offset: u64,
    },
    /// The receiver already holds this content; nothing will be sent.
    Have {
        transfer_id: u64,
        hash: String,
    },
    /// Received, verified against its hash, and durably installed.
    Done {
        transfer_id: u64,
        hash: String,
    },
    Failed {
        transfer_id: u64,
        hash: String,
        reason: String,
    },
    /// Answer to a request for content this side cannot produce.
    Unavailable {
        hash: String,
        reason: String,
    },
}

/// Everything a reconnecting or resuming client knows (specification
/// sections 103, 105).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientResumeState {
    /// Highest revision such that every revision up to and including it has
    /// been applied and persisted locally. Not "highest revision seen".
    pub last_applied_revision: u64,
    pub control_version: u64,
    pub last_publication_sequence: u64,
    #[serde(default)]
    pub pending_operation_ids: Vec<Uuid>,
    /// Deterministic hash over the local replica manifest (section 107).
    #[serde(default)]
    pub replica_hash: String,
    /// False on a first join: the client has no canonical manifest at all.
    #[serde(default)]
    pub has_manifest: bool,
}

/// A path one participant is holding back because it is above the session
/// limit (`docs/BLOB-PLANE.md`, section 4).
///
/// Carried in the control snapshot rather than kept local, because the file
/// blocks publication for everybody: whoever runs `weave commit prepare` has to
/// be able to see which file it is, how large, and whose machine it is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OversizePath {
    pub actor_id: Uuid,
    pub display_name: String,
    pub path: RepoPath,
    pub size: u64,
    /// The session already holds canonical content for this path, and the local
    /// file has since grown past the limit. The replicas differ until it
    /// shrinks or the limit rises — the one divergence Weave tolerates, and
    /// only because it refuses to publish while it lasts.
    #[serde(default)]
    pub canonical: bool,
}

/// What one participant reports about its own oversize paths.
///
/// A report replaces that actor's whole set, so a path that came back under the
/// limit needs no separate retraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OversizeReport {
    pub path: RepoPath,
    pub size: u64,
    #[serde(default)]
    pub canonical: bool,
}

/// Current control state (specification sections 99, 100).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub control_version: u64,
    pub tasks: Vec<Task>,
    pub conflicts: Vec<Conflict>,
    pub publication: Option<GitPublication>,
    pub publication_sequence: u64,
    pub session: SessionInfo,
    /// Largest file this session synchronizes. Canonical session state: every
    /// participant judges its own working tree against this value, at this
    /// control version, and never against a local preference.
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
    /// Every path any participant is currently holding back, session-wide.
    #[serde(default)]
    pub oversize: Vec<OversizePath>,
}

fn default_max_file_size() -> u64 {
    DEFAULT_MAX_FILE_SIZE
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrepareOutcome {
    Prepared(Box<CommitPreparation>),
    Rejected {
        class: ErrorClass,
        message: String,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitOutcome {
    Published {
        publication: Box<GitPublication>,
    },
    Rejected {
        class: ErrorClass,
        message: String,
        detail: Option<String>,
    },
}

/// A conflict discovered locally during a continuation rebase
/// (specification section 42).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictReport {
    /// Client-proposed identifier so the reporting machine can enter conflict
    /// draft mode immediately, without waiting for a round trip.
    pub id: Uuid,
    pub path: RepoPath,
    pub kind: ConflictKind,
    pub base_entry: Option<FileEntry>,
    pub canonical_entry: Option<FileEntry>,
    pub incoming_entry: Option<FileEntry>,
    pub latest_local_candidate: Option<FileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_task_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Client -> Host
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        session_id: Uuid,
        actor_id: Uuid,
        display_name: String,
        git_name: String,
        git_email: String,
        base_commit: String,
        branch: String,
        resume: ClientResumeState,
    },
    /// Submit an operation. Every blob it references must already be durably
    /// present at the host; the host refuses it otherwise.
    SubmitOperation {
        operation: Box<FileOperation>,
    },
    RequestBlobs {
        hashes: Vec<String>,
    },
    /// One step of a blob transfer in either direction.
    Blob {
        blob: BlobControl,
    },
    /// Ask for a fresh full canonical manifest (divergence, corruption, or a
    /// gap the client cannot close by replay).
    RequestManifest {
        reason: String,
    },
    RequestControlSnapshot,
    /// This participant's complete set of paths held back for being above the
    /// session limit. Sent whenever the set changes and once on every connect,
    /// so the host's view is rebuilt from the reporter rather than remembered
    /// across a socket it cannot vouch for.
    ReportOversize {
        paths: Vec<OversizeReport>,
    },
    /// Ask the session to change its file size limit.
    SetFileLimit {
        request_id: Uuid,
        max_file_size: u64,
    },
    ReportConflict {
        report: Box<ConflictReport>,
    },
    AttachLocalCandidate {
        conflict_id: Uuid,
        entry: Option<FileEntry>,
    },
    ResolveConflict {
        request_id: Uuid,
        conflict_id: Uuid,
        operation_id: Uuid,
        /// Canonical entry the resolution was based on (specification
        /// section 87): the host refuses the resolution if canonical moved.
        expected_canonical: Option<FileEntry>,
        resolved_entry: Option<FileEntry>,
    },
    DismissConflict {
        request_id: Uuid,
        conflict_id: Uuid,
    },
    TaskStart {
        request_id: Uuid,
        task_id: Uuid,
        description: String,
        scopes: Vec<TaskScope>,
    },
    TaskUpdate {
        request_id: Uuid,
        task_id: Uuid,
        description: Option<String>,
        scopes: Option<Vec<TaskScope>>,
    },
    TaskComplete {
        request_id: Uuid,
        task_id: Uuid,
    },
    TaskCancel {
        request_id: Uuid,
        task_id: Uuid,
    },
    CommitPrepare {
        request_id: Uuid,
        allow_active_tasks: bool,
    },
    CommitCreate {
        request_id: Uuid,
        prepare_id: Uuid,
        message: String,
    },
    PushRequest {
        request_id: Uuid,
    },
    BarrierAck {
        barrier_id: Uuid,
        watermark: u64,
    },
    BarrierReady {
        barrier_id: Uuid,
        ok: bool,
        detail: String,
    },
    ReplicaHash {
        revision: u64,
        hash: String,
    },
    Presence {
        last_applied_revision: u64,
        active_task_id: Option<Uuid>,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

// ---------------------------------------------------------------------------
// Host -> Client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_type", rename_all = "snake_case")]
pub enum HostMessage {
    Welcome {
        session: SessionInfo,
        /// The consistent snapshot point (specification section 101).
        snapshot_revision: u64,
        /// Full canonical manifest when the client needs to be rebuilt from
        /// scratch; `None` when the client will be caught up by replay.
        manifest: Option<Vec<ManifestEntry>>,
        control: Box<ControlSnapshot>,
        /// Publications the client has not yet installed, oldest first.
        pending_publications: Vec<GitPublication>,
        host_state_hash: String,
    },
    /// A fresh canonical manifest, replacing the client's replica.
    ManifestSnapshot {
        snapshot_revision: u64,
        manifest: Vec<ManifestEntry>,
        host_state_hash: String,
    },
    /// Metadata only. A participant that does not hold the referenced blob
    /// pulls it with `RequestBlobs`, through the same path a reconnecting
    /// replica uses — which keeps that path in the nominal regime rather than
    /// in a rarely exercised recovery branch.
    RevisionBroadcast {
        revision: Box<Revision>,
    },
    OperationResult {
        operation_id: Uuid,
        outcome: Box<OperationOutcome>,
    },
    /// One step of a blob transfer in either direction.
    Blob {
        blob: BlobControl,
    },
    Control {
        control: Box<ControlSnapshot>,
    },
    Presence {
        peers: Vec<PeerInfo>,
    },
    /// Commit preparation barrier (specification sections 112-115).
    BarrierStart {
        barrier_id: Uuid,
    },
    BarrierEnd {
        barrier_id: Uuid,
    },
    /// The publication descriptor, plus the hash of the pack holding the exact
    /// host-produced Git objects (specification sections 131, 192).
    ///
    /// The pack is an ordinary blob: a pack containing a large file is exactly
    /// as large as the file, and it sits on the critical path of
    /// `weave commit create`, so it travels the plane built for that.
    Publication {
        publication: Box<GitPublication>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pack_hash: Option<String>,
    },
    PrepareResult {
        request_id: Uuid,
        outcome: Box<PrepareOutcome>,
    },
    CommitResult {
        request_id: Uuid,
        outcome: Box<CommitOutcome>,
    },
    PushResult {
        request_id: Uuid,
        status: PushStatus,
        message: String,
    },
    /// Generic acknowledgement for control requests.
    Ack {
        request_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<Uuid>,
        class: ErrorClass,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// The host stopped accepting mutations (external Git change, integrity
    /// failure). Clients surface this rather than guessing.
    HostState {
        state: SyncState,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    Goodbye {
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientEnvelope {
    pub protocol_version: u32,
    #[serde(flatten)]
    pub message: ClientMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostEnvelope {
    pub protocol_version: u32,
    #[serde(flatten)]
    pub message: HostMessage,
}

impl ClientEnvelope {
    pub fn wrap(message: ClientMessage) -> ClientEnvelope {
        ClientEnvelope {
            protocol_version: PROTOCOL_VERSION,
            message,
        }
    }
}

impl HostEnvelope {
    pub fn wrap(message: HostMessage) -> HostEnvelope {
        HostEnvelope {
            protocol_version: PROTOCOL_VERSION,
            message,
        }
    }
}

/// Deterministic replica state hash (specification section 107): sort by
/// canonical path, hash `path`, `git_mode` and `blob_hash`.
pub fn state_hash<'a, I>(entries: I) -> String
where
    I: IntoIterator<Item = (&'a RepoPath, &'a FileEntry)>,
{
    let mut rows: Vec<(&RepoPath, &FileEntry)> = entries.into_iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    for (path, entry) in rows {
        sha2::Digest::update(&mut hasher, path.as_str().as_bytes());
        sha2::Digest::update(&mut hasher, b"\0");
        sha2::Digest::update(&mut hasher, entry.git_mode.as_str().as_bytes());
        sha2::Digest::update(&mut hasher, b"\0");
        sha2::Digest::update(&mut hasher, entry.blob_hash.as_bytes());
        sha2::Digest::update(&mut hasher, b"\n");
    }
    crate::util::hex(&sha2::Digest::finalize(hasher))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_carries_version_and_type() {
        let env = ClientEnvelope::wrap(ClientMessage::Ping { nonce: 7 });
        let text = serde_json::to_string(&env).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(value["message_type"], "ping");
        let back: ClientEnvelope = serde_json::from_str(&text).unwrap();
        matches!(back.message, ClientMessage::Ping { nonce: 7 });
    }

    #[test]
    fn state_hash_is_order_independent() {
        let p1 = RepoPath::new("b.txt").unwrap();
        let p2 = RepoPath::new("a.txt").unwrap();
        let e1 = FileEntry::from_bytes(b"one", GitMode::Regular);
        let e2 = FileEntry::from_bytes(b"two", GitMode::Regular);
        let a = state_hash(vec![(&p1, &e1), (&p2, &e2)]);
        let b = state_hash(vec![(&p2, &e2), (&p1, &e1)]);
        assert_eq!(a, b);
    }
}
