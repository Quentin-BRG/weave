// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Core Weave domain types.
//!
//! These mirror the conceptual structures given in the specification
//! (sections 17-22, 82, 90, 120, 129) as closely as Rust allows.

use crate::path::RepoPath;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Limits (specification section 51)
// ---------------------------------------------------------------------------

/// Files larger than this are never three-way merged as text.
pub const TEXT_MERGE_LIMIT: u64 = 1024 * 1024; // 1 MiB
/// Files larger than this are not synchronized at all.
pub const MAX_SYNCED_FILE: u64 = 10 * 1024 * 1024; // 10 MiB
/// Maximum WebSocket frame accepted, generous enough for a maximal file plus
/// base64 expansion and protocol overhead (specification section 66).
pub const MAX_PROTOCOL_MESSAGE: usize = 48 * 1024 * 1024;
/// Backpressure bounds per remote connection (specification section 65).
pub const MAX_QUEUED_MESSAGES: usize = 256;
pub const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;
/// Wire protocol version (specification section 54).
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// File entries (specification sections 17-19)
// ---------------------------------------------------------------------------

/// The Git file modes Weave supports. Tracked symlinks and gitlinks are not
/// representable on purpose (specification sections 12 and 17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GitMode {
    #[serde(rename = "100644")]
    Regular,
    #[serde(rename = "100755")]
    Executable,
}

impl GitMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GitMode::Regular => "100644",
            GitMode::Executable => "100755",
        }
    }

    pub fn parse(s: &str) -> Option<GitMode> {
        match s {
            "100644" => Some(GitMode::Regular),
            "100755" => Some(GitMode::Executable),
            _ => None,
        }
    }

    pub fn is_executable(self) -> bool {
        matches!(self, GitMode::Executable)
    }
}

impl std::fmt::Display for GitMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Deterministic text/binary classification (specification section 18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Text,
    Binary,
}

impl FileKind {
    /// Classify bytes. Deterministic from the bytes and the configured limit.
    pub fn classify(bytes: &[u8]) -> FileKind {
        if bytes.len() as u64 > TEXT_MERGE_LIMIT {
            return FileKind::Binary;
        }
        if bytes.contains(&0) {
            return FileKind::Binary;
        }
        if std::str::from_utf8(bytes).is_err() {
            return FileKind::Binary;
        }
        FileKind::Text
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Text => "text",
            FileKind::Binary => "binary",
        }
    }
}

/// A repository entry. Absence of a `FileEntry` represents a missing path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileEntry {
    /// Lowercase hex SHA-256 over the exact file bytes.
    pub blob_hash: String,
    pub size: u64,
    pub git_mode: GitMode,
    pub file_kind: FileKind,
}

impl FileEntry {
    pub fn from_bytes(bytes: &[u8], git_mode: GitMode) -> FileEntry {
        FileEntry {
            blob_hash: crate::util::sha256_hex(bytes),
            size: bytes.len() as u64,
            git_mode,
            file_kind: FileKind::classify(bytes),
        }
    }

    /// Same content and same mode.
    pub fn same_as(a: Option<&FileEntry>, b: Option<&FileEntry>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => x.blob_hash == y.blob_hash && x.git_mode == y.git_mode,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Revisions (specification sections 20, 21)
// ---------------------------------------------------------------------------

/// One accepted canonical filesystem mutation.
///
/// Complete before/after entry information is retained so that creation,
/// deletion, content change and executable-mode change are all distinguishable
/// and every revision is reconstructible (specification sections 20 and 7.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub revision: u64,
    pub operation_id: Uuid,
    pub actor_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
    pub timestamp_ms: i64,
    pub path: RepoPath,
    pub before: Option<FileEntry>,
    pub after: Option<FileEntry>,
}

// ---------------------------------------------------------------------------
// Operations (specification section 22)
// ---------------------------------------------------------------------------

/// A participant's desired state for one path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOperation {
    pub operation_id: Uuid,
    /// Advisory only: the host rebinds this to the authenticated connection
    /// identity (specification section 26).
    pub actor_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
    pub local_seq: u64,
    pub base_revision: u64,
    pub base_entry: Option<FileEntry>,
    pub path: RepoPath,
    /// `None` means deletion.
    pub desired_entry: Option<FileEntry>,
    /// Full file content, base64 in JSON. Present for creates and updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_b64: Option<String>,
}

impl FileOperation {
    /// Stable hash over the semantically meaningful payload, used to detect a
    /// retransmitted `operation_id` carrying a different payload
    /// (specification sections 24 and 183).
    pub fn payload_hash(&self) -> String {
        let canonical = serde_json::json!({
            "task_id": self.task_id,
            "local_seq": self.local_seq,
            "base_revision": self.base_revision,
            "base_entry": self.base_entry,
            "path": self.path,
            "desired_entry": self.desired_entry,
        });
        crate::util::sha256_hex(canonical.to_string().as_bytes())
    }
}

/// The durable result of an operation, replayable for idempotent retransmits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum OperationOutcome {
    /// Canonical state advanced. A new revision was created.
    Accepted {
        revision: u64,
        canonical_entry: Option<FileEntry>,
    },
    /// The desired state already matched canonical state; no revision consumed
    /// (specification sections 21, 72, 75).
    Converged {
        revision: u64,
        canonical_entry: Option<FileEntry>,
    },
    /// A clean three-way merge produced content different from the request.
    Merged {
        revision: u64,
        canonical_entry: Option<FileEntry>,
    },
    /// Reconciliation could not proceed automatically. Canonical state is
    /// unchanged and a Weave conflict was created.
    Conflicted {
        conflict_id: Uuid,
        kind: ConflictKind,
        revision: u64,
        canonical_entry: Option<FileEntry>,
    },
    /// The operation was refused. Canonical state is unchanged.
    Rejected {
        class: crate::error::ErrorClass,
        message: String,
    },
}

impl OperationOutcome {
    pub fn canonical(&self) -> Option<(u64, Option<FileEntry>)> {
        match self {
            OperationOutcome::Accepted {
                revision,
                canonical_entry,
            }
            | OperationOutcome::Converged {
                revision,
                canonical_entry,
            }
            | OperationOutcome::Merged {
                revision,
                canonical_entry,
            }
            | OperationOutcome::Conflicted {
                revision,
                canonical_entry,
                ..
            } => Some((*revision, canonical_entry.clone())),
            OperationOutcome::Rejected { .. } => None,
        }
    }

    pub fn is_resolved(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Conflicts (specification sections 74-82)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    ConcurrentCreate,
    DeleteModify,
    BinaryConcurrentEdit,
    TextConcurrentEdit,
    ModeConflict,
}

impl ConflictKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictKind::ConcurrentCreate => "concurrent_create",
            ConflictKind::DeleteModify => "delete_modify",
            ConflictKind::BinaryConcurrentEdit => "binary_concurrent_edit",
            ConflictKind::TextConcurrentEdit => "text_concurrent_edit",
            ConflictKind::ModeConflict => "mode_conflict",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            ConflictKind::ConcurrentCreate => {
                "The same path was created independently with different content."
            }
            ConflictKind::DeleteModify => "One side deleted the file while the other modified it.",
            ConflictKind::BinaryConcurrentEdit => {
                "A binary file was modified concurrently; binary content is never merged."
            }
            ConflictKind::TextConcurrentEdit => {
                "Overlapping line-level edits could not be merged automatically."
            }
            ConflictKind::ModeConflict => {
                "The executable bit was changed incompatibly on both sides."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStatus {
    Open,
    Resolved,
    Dismissed,
}

impl ConflictStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictStatus::Open => "open",
            ConflictStatus::Resolved => "resolved",
            ConflictStatus::Dismissed => "dismissed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub id: Uuid,
    pub path: RepoPath,
    pub kind: ConflictKind,
    pub base_entry: Option<FileEntry>,
    pub canonical_entry: Option<FileEntry>,
    pub incoming_entry: Option<FileEntry>,
    /// The newest local candidate on the originating machine. Attached
    /// separately when the user kept editing (specification sections 42-43).
    pub latest_local_candidate: Option<FileEntry>,
    pub incoming_actor_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_task_id: Option<Uuid>,
    pub canonical_revision: u64,
    pub created_at_ms: i64,
    pub status: ConflictStatus,
    /// Revision that resolved the conflict, when resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_revision: Option<u64>,
}

impl Conflict {
    pub fn short_id(&self) -> String {
        crate::util::short_id('C', &self.id)
    }
}

// ---------------------------------------------------------------------------
// Tasks (specification sections 89-97)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Active,
    Completed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Active => "active",
            TaskStatus::Completed => "completed",
            TaskStatus::Cancelled => "cancelled",
        }
    }
}

/// An advisory soft lock: one file, optionally one line range
/// (specification sections 93, 94, 96).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskScope {
    pub path: RepoPath,
    /// Inclusive 1-based line range, when declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
    /// The entry the range was declared against. When the file moves on and the
    /// range can no longer be mapped safely, the scope degrades to file level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_against: Option<FileEntry>,
    #[serde(default)]
    pub stale: bool,
}

impl TaskScope {
    pub fn display(&self) -> String {
        match (self.line_start, self.line_end) {
            (Some(a), Some(b)) if !self.stale => format!("{}:{a}-{b}", self.path),
            (Some(a), Some(b)) => format!("{}:{a}-{b} (stale, file-level)", self.path),
            _ => self.path.to_string(),
        }
    }

    /// Two scopes overlap when they name the same file and either scope is
    /// file-level, stale, or their line ranges intersect.
    pub fn overlaps(&self, other: &TaskScope) -> bool {
        if self.path != other.path {
            return false;
        }
        match (self.effective_range(), other.effective_range()) {
            (Some((a1, a2)), Some((b1, b2))) => a1 <= b2 && b1 <= a2,
            _ => true,
        }
    }

    fn effective_range(&self) -> Option<(u32, u32)> {
        if self.stale {
            return None;
        }
        match (self.line_start, self.line_end) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub actor_id: Uuid,
    pub description: String,
    pub status: TaskStatus,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub created_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_revision: Option<u64>,
    #[serde(default)]
    pub scopes: Vec<TaskScope>,
    #[serde(default)]
    pub touched_paths: Vec<RepoPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_accepted_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_accepted_revision: Option<u64>,
}

impl Task {
    pub fn short_id(&self) -> String {
        crate::util::short_id('T', &self.id)
    }
}

// ---------------------------------------------------------------------------
// Git publication (specification sections 120, 129, 135, 136)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrepareStatus {
    Prepared,
    Published,
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitPreparation {
    pub prepare_id: Uuid,
    pub target_revision: u64,
    pub parent_commit_oid: String,
    pub requesting_actor: Uuid,
    #[serde(default)]
    pub included_task_ids: Vec<Uuid>,
    /// The same Tasks with the detail an agent needs to write a semantic commit
    /// message (specification section 121).
    #[serde(default)]
    pub included_tasks: Vec<PreparedTask>,
    /// Union of every path the included Tasks touched.
    #[serde(default)]
    pub touched_paths: Vec<RepoPath>,
    #[serde(default)]
    pub unassigned_revisions: Vec<u64>,
    pub created_at_ms: i64,
    pub status: PrepareStatus,
    /// Revision published previously, for reporting.
    pub previous_published_revision: u64,
    /// Paths added/modified/removed between the previous publication and target.
    #[serde(default)]
    pub diff_summary: DiffSummary,
    /// Actors that contributed accepted revisions to this range.
    #[serde(default)]
    pub contributors: Vec<Contributor>,
    /// Participants that were not connected when the barrier ran.
    #[serde(default)]
    pub disconnected_participants: Vec<String>,
}

impl CommitPreparation {
    pub fn short_id(&self) -> String {
        crate::util::short_id('P', &self.prepare_id)
    }
}

/// A Task that contributed accepted revisions to a prepared publication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedTask {
    pub id: Uuid,
    pub short_id: String,
    pub description: String,
    pub status: TaskStatus,
    pub actor_id: Uuid,
    pub actor_display_name: String,
    #[serde(default)]
    pub touched_paths: Vec<RepoPath>,
    #[serde(default)]
    pub revisions: Vec<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffSummary {
    #[serde(default)]
    pub added: Vec<RepoPath>,
    #[serde(default)]
    pub modified: Vec<RepoPath>,
    #[serde(default)]
    pub deleted: Vec<RepoPath>,
}

impl DiffSummary {
    pub fn total(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contributor {
    pub actor_id: Uuid,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub revisions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitDescriptor {
    pub prepare_id: Uuid,
    pub target_revision: u64,
    pub parent_commit_oid: String,
    pub tree_oid: String,
    pub commit_oid: String,
    pub author_name: String,
    pub author_email: String,
    pub committer_name: String,
    pub committer_email: String,
    pub timestamp: i64,
    pub timezone: String,
    pub message: String,
    #[serde(default)]
    pub contributing_task_ids: Vec<Uuid>,
    pub branch: String,
}

/// Local application progress of a publication (specification section 135).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationStage {
    Pending,
    ObjectsInstalled,
    RefUpdated,
    IndexUpdated,
    Complete,
}

impl PublicationStage {
    pub fn as_str(self) -> &'static str {
        match self {
            PublicationStage::Pending => "pending",
            PublicationStage::ObjectsInstalled => "objects_installed",
            PublicationStage::RefUpdated => "ref_updated",
            PublicationStage::IndexUpdated => "index_updated",
            PublicationStage::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushStatus {
    NotAttempted,
    NoUpstream,
    Pushed,
    Failed,
    Diverged,
}

impl PushStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PushStatus::NotAttempted => "not_attempted",
            PushStatus::NoUpstream => "no_upstream",
            PushStatus::Pushed => "pushed",
            PushStatus::Failed => "failed",
            PushStatus::Diverged => "diverged",
        }
    }
}

/// The canonical publication record persisted by the host (section 136) and
/// mirrored by every client as control state (section 137).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitPublication {
    pub descriptor: CommitDescriptor,
    pub stage: PublicationStage,
    pub push_status: PushStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_error: Option<String>,
    pub created_at_ms: i64,
    /// Monotonic index of this publication within the session, so clients can
    /// apply publications in order after a disconnect.
    pub sequence: u64,
}

// ---------------------------------------------------------------------------
// Presence and status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Host,
    Participant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::Participant => "participant",
        }
    }
}

/// Synchronization state of one replica (specification section 14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum SyncState {
    /// Normal operation.
    Live,
    /// External Git mutation detected; Weave stops synchronizing until the
    /// expected Git state is restored. Weave never repairs it automatically.
    Paused { reason: String, detail: String },
    /// Persistence or integrity failure; no new mutations are accepted.
    Degraded { reason: String },
}

impl SyncState {
    pub fn label(&self) -> &'static str {
        match self {
            SyncState::Live => "live",
            SyncState::Paused { .. } => "paused",
            SyncState::Degraded { .. } => "degraded",
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, SyncState::Live)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub actor_id: Uuid,
    pub display_name: String,
    pub role: Role,
    pub online: bool,
    pub last_known_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task_description: Option<String>,
    pub last_seen_ms: i64,
}
