// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Durable per-participant replica state and outbox.
//!
//! Specification sections 34-38 (local sequence, persistent outbox,
//! per-path logical state, one in-flight operation per path), 105 (contiguous
//! revision watermark), 135 (publication journal), 147 (participant crash
//! recovery).

use crate::db;
use crate::error::Result;
use crate::model::*;
use crate::path::RepoPath;
use crate::proto::{ControlSnapshot, OversizeReport, SessionInfo};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use uuid::Uuid;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS replica (
    path               TEXT PRIMARY KEY,
    confirmed          TEXT,
    confirmed_revision INTEGER NOT NULL DEFAULT 0,
    materialized       TEXT,
    in_flight          TEXT,
    pending_local      TEXT,
    conflict_draft     TEXT
);

CREATE TABLE IF NOT EXISTS pub_journal (
    sequence   INTEGER PRIMARY KEY,
    commit_oid TEXT NOT NULL,
    stage      TEXT NOT NULL,
    data       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS control_cache (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    data TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS oversize (
    path      TEXT PRIMARY KEY,
    size      INTEGER NOT NULL,
    canonical INTEGER NOT NULL DEFAULT 0
);
"#;

const K_SCHEMA: &str = "schema_version";
const K_LOCAL_SEQ: &str = "local_seq";
const K_LAST_APPLIED: &str = "last_applied_revision";
const K_CONTROL_VERSION: &str = "control_version";
const K_SESSION: &str = "session";
const K_ROLE: &str = "role";
const K_ACTOR: &str = "actor_id";
const K_PUB_SEQ: &str = "last_publication_sequence";
const K_HAS_MANIFEST: &str = "has_manifest";

/// The single submitted operation awaiting a durable host result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlight {
    pub operation_id: Uuid,
    pub base_revision: u64,
    pub base_entry: Option<FileEntry>,
    pub desired: Option<FileEntry>,
    pub local_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
    /// Set once the operation has actually been handed to the transport, so a
    /// reconnect knows to retransmit it.
    #[serde(default)]
    pub sent: bool,
    /// When it was last handed to the transport. Operations are idempotent by
    /// `operation_id`, so an unanswered one is safely retransmitted.
    #[serde(default)]
    pub sent_at_ms: i64,
}

/// The newest durably captured local desired content produced after the
/// in-flight candidate (specification section 37).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingLocal {
    pub desired: Option<FileEntry>,
    pub local_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
}

/// A preserved local candidate being edited to resolve a conflict
/// (specification section 85).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictDraft {
    pub conflict_id: Uuid,
    pub entry: Option<FileEntry>,
    pub local_seq: u64,
}

/// Per-path logical state (specification section 37).
#[derive(Debug, Clone, Default)]
pub struct PathState {
    pub confirmed: Option<FileEntry>,
    /// The canonical revision at which `confirmed` was established. Used as the
    /// declared `base_revision`, which the host validates (section 25).
    pub confirmed_revision: u64,
    /// The last filesystem state Weave intentionally wrote or confirmed.
    pub materialized: Option<FileEntry>,
    pub in_flight: Option<InFlight>,
    pub pending_local: Option<PendingLocal>,
    pub conflict_draft: Option<ConflictDraft>,
}

impl PathState {
    pub fn has_local_work(&self) -> bool {
        self.in_flight.is_some() || self.pending_local.is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.confirmed.is_none()
            && self.materialized.is_none()
            && self.in_flight.is_none()
            && self.pending_local.is_none()
            && self.conflict_draft.is_none()
    }

    /// The newest locally desired state, whether or not it has been submitted.
    pub fn latest_local_desired(&self) -> Option<&Option<FileEntry>> {
        if let Some(p) = &self.pending_local {
            Some(&p.desired)
        } else {
            self.in_flight.as_ref().map(|f| &f.desired)
        }
    }

    /// Highest local sequence number represented by unresolved local work.
    pub fn max_local_seq(&self) -> u64 {
        let a = self.in_flight.as_ref().map(|f| f.local_seq).unwrap_or(0);
        let b = self
            .pending_local
            .as_ref()
            .map(|p| p.local_seq)
            .unwrap_or(0);
        a.max(b)
    }
}

pub struct ClientStore {
    conn: Connection,
}

impl ClientStore {
    pub fn open(path: &Path) -> Result<ClientStore> {
        let conn = db::open(path)?;
        conn.execute_batch(SCHEMA)?;
        if db::get_meta(&conn, K_SCHEMA)?.is_none() {
            db::set_meta(&conn, K_SCHEMA, "1")?;
            db::set_u64(&conn, K_LOCAL_SEQ, 0)?;
            db::set_u64(&conn, K_LAST_APPLIED, 0)?;
            db::set_u64(&conn, K_CONTROL_VERSION, 0)?;
            db::set_u64(&conn, K_PUB_SEQ, 0)?;
            db::set_meta(&conn, K_HAS_MANIFEST, "false")?;
        }
        Ok(ClientStore { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ------------------------------------------------------------------ meta

    pub fn actor_id(&self) -> Result<Option<Uuid>> {
        Ok(db::get_meta(&self.conn, K_ACTOR)?.and_then(|v| Uuid::parse_str(&v).ok()))
    }

    pub fn set_actor_id(&self, id: &Uuid) -> Result<()> {
        db::set_meta(&self.conn, K_ACTOR, &id.to_string())
    }

    pub fn role(&self) -> Result<Option<Role>> {
        db::get_json(&self.conn, K_ROLE)
    }

    pub fn set_role(&self, role: Role) -> Result<()> {
        db::set_json(&self.conn, K_ROLE, &role)
    }

    pub fn session(&self) -> Result<Option<SessionInfo>> {
        db::get_json(&self.conn, K_SESSION)
    }

    pub fn set_session(&self, info: &SessionInfo) -> Result<()> {
        db::set_json(&self.conn, K_SESSION, info)
    }

    pub fn local_seq(&self) -> Result<u64> {
        db::get_u64(&self.conn, K_LOCAL_SEQ, 0)
    }

    /// Increment and return the local sequence number. Called whenever Weave
    /// durably captures a new local desired state (specification section 34).
    pub fn next_local_seq(&self) -> Result<u64> {
        let next = self.local_seq()? + 1;
        db::set_u64(&self.conn, K_LOCAL_SEQ, next)?;
        Ok(next)
    }

    pub fn last_applied_revision(&self) -> Result<u64> {
        db::get_u64(&self.conn, K_LAST_APPLIED, 0)
    }

    pub fn set_last_applied_revision(&self, revision: u64) -> Result<()> {
        db::set_u64(&self.conn, K_LAST_APPLIED, revision)
    }

    pub fn control_version(&self) -> Result<u64> {
        db::get_u64(&self.conn, K_CONTROL_VERSION, 0)
    }

    pub fn set_control_version(&self, v: u64) -> Result<()> {
        db::set_u64(&self.conn, K_CONTROL_VERSION, v)
    }

    pub fn last_publication_sequence(&self) -> Result<u64> {
        db::get_u64(&self.conn, K_PUB_SEQ, 0)
    }

    pub fn set_last_publication_sequence(&self, v: u64) -> Result<()> {
        db::set_u64(&self.conn, K_PUB_SEQ, v)
    }

    pub fn has_manifest(&self) -> Result<bool> {
        Ok(db::get_meta(&self.conn, K_HAS_MANIFEST)?.as_deref() == Some("true"))
    }

    pub fn set_has_manifest(&self, v: bool) -> Result<()> {
        db::set_meta(&self.conn, K_HAS_MANIFEST, if v { "true" } else { "false" })
    }

    // --------------------------------------------------------------- replica

    pub fn path_state(&self, path: &RepoPath) -> Result<PathState> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT confirmed, confirmed_revision, materialized, in_flight, pending_local,
                    conflict_draft
             FROM replica WHERE path = ?1",
        )?;
        let row = stmt
            .query_row([path.as_str()], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .optional()?;
        match row {
            None => Ok(PathState::default()),
            Some((confirmed, rev, materialized, in_flight, pending, draft)) => Ok(PathState {
                confirmed: db::parse_opt_json(confirmed)?,
                confirmed_revision: rev as u64,
                materialized: db::parse_opt_json(materialized)?,
                in_flight: db::parse_opt_json(in_flight)?,
                pending_local: db::parse_opt_json(pending)?,
                conflict_draft: db::parse_opt_json(draft)?,
            }),
        }
    }

    pub fn put_path_state(&self, path: &RepoPath, state: &PathState) -> Result<()> {
        if state.is_empty() {
            self.conn
                .execute("DELETE FROM replica WHERE path = ?1", [path.as_str()])?;
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO replica(path, confirmed, confirmed_revision, materialized, in_flight,
                                 pending_local, conflict_draft)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET
                confirmed = excluded.confirmed,
                confirmed_revision = excluded.confirmed_revision,
                materialized = excluded.materialized,
                in_flight = excluded.in_flight,
                pending_local = excluded.pending_local,
                conflict_draft = excluded.conflict_draft",
            params![
                path.as_str(),
                db::opt_json(&state.confirmed)?,
                state.confirmed_revision as i64,
                db::opt_json(&state.materialized)?,
                db::opt_json(&state.in_flight)?,
                db::opt_json(&state.pending_local)?,
                db::opt_json(&state.conflict_draft)?,
            ],
        )?;
        Ok(())
    }

    pub fn all_paths(&self) -> Result<Vec<RepoPath>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM replica ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(RepoPath::new(&row?)?);
        }
        Ok(out)
    }

    pub fn all_states(&self) -> Result<BTreeMap<RepoPath, PathState>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, confirmed, confirmed_revision, materialized, in_flight, pending_local,
                    conflict_draft FROM replica",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (path, confirmed, rev, materialized, in_flight, pending, draft) = row?;
            out.insert(
                RepoPath::new(&path)?,
                PathState {
                    confirmed: db::parse_opt_json(confirmed)?,
                    confirmed_revision: rev as u64,
                    materialized: db::parse_opt_json(materialized)?,
                    in_flight: db::parse_opt_json(in_flight)?,
                    pending_local: db::parse_opt_json(pending)?,
                    conflict_draft: db::parse_opt_json(draft)?,
                },
            );
        }
        Ok(out)
    }

    /// Every blob this replica can still reach through its durable state.
    ///
    /// The live set for garbage collection, so it errs towards keeping: an
    /// entry counts whether it is canonical, merely materialized, in flight, a
    /// base this replica may have to rebase against, or a conflict draft it has
    /// not resolved yet. Content is cheap to keep and expensive to lose.
    pub fn referenced_blobs(&self) -> Result<HashSet<String>> {
        let mut live = HashSet::new();
        let mut add = |entry: Option<&FileEntry>| {
            if let Some(entry) = entry {
                live.insert(entry.blob_hash.clone());
            }
        };
        for state in self.all_states()?.values() {
            add(state.confirmed.as_ref());
            add(state.materialized.as_ref());
            if let Some(flight) = &state.in_flight {
                add(flight.base_entry.as_ref());
                add(flight.desired.as_ref());
            }
            if let Some(pending) = &state.pending_local {
                add(pending.desired.as_ref());
            }
            if let Some(draft) = &state.conflict_draft {
                add(draft.entry.as_ref());
            }
        }
        Ok(live)
    }

    /// The confirmed canonical manifest as this replica knows it.
    pub fn replica_manifest(&self) -> Result<BTreeMap<RepoPath, FileEntry>> {
        let mut out = BTreeMap::new();
        for (path, state) in self.all_states()? {
            if let Some(entry) = state.confirmed {
                out.insert(path, entry);
            }
        }
        Ok(out)
    }

    /// Replace the confirmed manifest wholesale, preserving local work.
    pub fn replace_confirmed_manifest(
        &mut self,
        snapshot_revision: u64,
        entries: &BTreeMap<RepoPath, FileEntry>,
    ) -> Result<()> {
        let existing = self.all_states()?;
        let tx = self.conn.transaction()?;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO replica(path, confirmed, confirmed_revision, materialized, in_flight,
                                     pending_local, conflict_draft)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET
                    confirmed = excluded.confirmed,
                    confirmed_revision = excluded.confirmed_revision",
            )?;
            for (path, entry) in entries {
                let prior = existing.get(path);
                upsert.execute(params![
                    path.as_str(),
                    serde_json::to_string(entry)?,
                    snapshot_revision as i64,
                    db::opt_json(&prior.and_then(|p| p.materialized.clone()))?,
                    db::opt_json(&prior.and_then(|p| p.in_flight.clone()))?,
                    db::opt_json(&prior.and_then(|p| p.pending_local.clone()))?,
                    db::opt_json(&prior.and_then(|p| p.conflict_draft.clone()))?,
                ])?;
            }
            // Paths that vanished from canonical state.
            let mut clear = tx.prepare(
                "UPDATE replica SET confirmed = NULL, confirmed_revision = ?2 WHERE path = ?1",
            )?;
            for path in existing.keys() {
                if !entries.contains_key(path) {
                    clear.execute(params![path.as_str(), snapshot_revision as i64])?;
                }
            }
            tx.execute(
                "DELETE FROM replica WHERE confirmed IS NULL AND materialized IS NULL
                 AND in_flight IS NULL AND pending_local IS NULL AND conflict_draft IS NULL",
                [],
            )?;
        }
        tx.commit()?;
        self.set_has_manifest(true)?;
        Ok(())
    }

    // ---------------------------------------------------------------- oversize

    /// Paths this replica is holding back for being above the session limit.
    ///
    /// Durable rather than in-memory: the condition outlives the daemon that
    /// noticed it, and a restart must not quietly resume publishing a session
    /// whose state one machine still cannot represent.
    pub fn oversize(&self) -> Result<Vec<OversizeReport>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, canonical FROM oversize ORDER BY path")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, size, canonical) = row?;
            out.push(OversizeReport {
                path: RepoPath::new(&path)?,
                size: size as u64,
                canonical: canonical != 0,
            });
        }
        Ok(out)
    }

    pub fn put_oversize(&self, path: &RepoPath, size: u64, canonical: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO oversize(path, size, canonical) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET size = excluded.size,
                                            canonical = excluded.canonical",
            params![path.as_str(), size as i64, canonical as i64],
        )?;
        Ok(())
    }

    /// Forget a path's oversize condition. Returns whether there was one.
    pub fn clear_oversize(&self, path: &RepoPath) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM oversize WHERE path = ?1", [path.as_str()])?;
        Ok(removed > 0)
    }

    pub fn is_oversize(&self, path: &RepoPath) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT 1 FROM oversize WHERE path = ?1")?;
        Ok(stmt
            .query_row([path.as_str()], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Drop every oversize record, so the next scan re-derives the set against
    /// a limit that has just changed.
    pub fn clear_all_oversize(&self) -> Result<()> {
        self.conn.execute("DELETE FROM oversize", [])?;
        Ok(())
    }

    // ------------------------------------------------------- publication journal

    pub fn put_publication_journal(
        &self,
        pubb: &GitPublication,
        stage: PublicationStage,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pub_journal(sequence, commit_oid, stage, data) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(sequence) DO UPDATE SET stage = excluded.stage, data = excluded.data",
            params![
                pubb.sequence as i64,
                pubb.descriptor.commit_oid,
                stage.as_str(),
                serde_json::to_string(pubb)?
            ],
        )?;
        Ok(())
    }

    pub fn publication_journal_stage(&self, sequence: u64) -> Result<Option<PublicationStage>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT stage FROM pub_journal WHERE sequence = ?1")?;
        let text: Option<String> = stmt.query_row([sequence as i64], |r| r.get(0)).optional()?;
        Ok(text.and_then(|t| match t.as_str() {
            "pending" => Some(PublicationStage::Pending),
            "objects_installed" => Some(PublicationStage::ObjectsInstalled),
            "ref_updated" => Some(PublicationStage::RefUpdated),
            "index_updated" => Some(PublicationStage::IndexUpdated),
            "complete" => Some(PublicationStage::Complete),
            _ => None,
        }))
    }

    /// Publications whose local application never reached `complete`
    /// (specification sections 135, 195).
    pub fn incomplete_publications(&self) -> Result<Vec<(GitPublication, PublicationStage)>> {
        let mut stmt = self.conn.prepare(
            "SELECT data, stage FROM pub_journal WHERE stage <> 'complete' ORDER BY sequence ASC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (data, stage) = row?;
            let pubb: GitPublication = serde_json::from_str(&data)?;
            let stage = match stage.as_str() {
                "objects_installed" => PublicationStage::ObjectsInstalled,
                "ref_updated" => PublicationStage::RefUpdated,
                "index_updated" => PublicationStage::IndexUpdated,
                _ => PublicationStage::Pending,
            };
            out.push((pubb, stage));
        }
        Ok(out)
    }

    pub fn latest_journal_publication(&self) -> Result<Option<GitPublication>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM pub_journal ORDER BY sequence DESC LIMIT 1")?;
        let text: Option<String> = stmt.query_row([], |r| r.get(0)).optional()?;
        db::parse_opt_json(text)
    }

    // ----------------------------------------------------------- control cache

    pub fn control_cache(&self) -> Result<Option<ControlSnapshot>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT data FROM control_cache WHERE id = 1")?;
        let text: Option<String> = stmt.query_row([], |r| r.get(0)).optional()?;
        db::parse_opt_json(text)
    }

    pub fn set_control_cache(&self, snapshot: &ControlSnapshot) -> Result<()> {
        self.conn.execute(
            "INSERT INTO control_cache(id, data) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![serde_json::to_string(snapshot)?],
        )?;
        self.set_control_version(snapshot.control_version)?;
        Ok(())
    }

    pub fn integrity_check(&self) -> Result<Vec<String>> {
        db::integrity_check(&self.conn)
    }

    /// Discard replica, outbox and publication journal so this machine can join
    /// a different session cleanly. The installation actor identity is kept.
    ///
    /// Only called when the recorded session differs from the one being started,
    /// never on resume or rejoin, so no unconfirmed local work is ever dropped
    /// from a session that is still live.
    pub fn reset(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        for table in ["replica", "pub_journal", "control_cache", "oversize"] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.commit()?;
        db::set_u64(&self.conn, K_LOCAL_SEQ, 0)?;
        db::set_u64(&self.conn, K_LAST_APPLIED, 0)?;
        db::set_u64(&self.conn, K_CONTROL_VERSION, 0)?;
        db::set_u64(&self.conn, K_PUB_SEQ, 0)?;
        self.set_has_manifest(false)?;
        Ok(())
    }
}
