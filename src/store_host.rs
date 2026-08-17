// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Durable canonical state owned by the host coordinator.
//!
//! Specification sections 7 (invariants), 19-21 (manifest and revisions),
//! 24-25 (operation identity and base validation), 68 (persistence ordering),
//! 82 (conflicts), 90 (Tasks), 120/136 (publication records), 175-176
//! (retention and reconstruction).

use crate::db;
use crate::error::{integrity, protocol, Result};
use crate::model::*;
use crate::path::RepoPath;
use crate::proto::SessionInfo;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use uuid::Uuid;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Baseline manifest at r0, the host working tree at session creation.
CREATE TABLE IF NOT EXISTS base_manifest (
    path  TEXT PRIMARY KEY,
    entry TEXT NOT NULL
);

-- Current canonical manifest: RepoPath -> FileEntry.
CREATE TABLE IF NOT EXISTS manifest (
    path          TEXT PRIMARY KEY,
    collision_key TEXT NOT NULL,
    entry         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS manifest_collision ON manifest(collision_key);

-- Total order of accepted filesystem mutations, with full before/after entries.
CREATE TABLE IF NOT EXISTS revisions (
    revision     INTEGER PRIMARY KEY,
    operation_id TEXT NOT NULL,
    actor_id     TEXT NOT NULL,
    task_id      TEXT,
    timestamp_ms INTEGER NOT NULL,
    path         TEXT NOT NULL,
    before_entry TEXT,
    after_entry  TEXT
);
CREATE INDEX IF NOT EXISTS revisions_path ON revisions(path, revision);
CREATE INDEX IF NOT EXISTS revisions_task ON revisions(task_id);

-- operation_id -> durable result, for idempotent retransmission.
CREATE TABLE IF NOT EXISTS operations (
    operation_id  TEXT PRIMARY KEY,
    actor_id      TEXT NOT NULL,
    payload_hash  TEXT NOT NULL,
    result        TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tasks (
    id       TEXT PRIMARY KEY,
    actor_id TEXT NOT NULL,
    status   TEXT NOT NULL,
    data     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conflicts (
    id            TEXT PRIMARY KEY,
    path          TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    data          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS preparations (
    prepare_id    TEXT PRIMARY KEY,
    created_at_ms INTEGER NOT NULL,
    data          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS publications (
    sequence   INTEGER PRIMARY KEY,
    commit_oid TEXT NOT NULL,
    data       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS actors (
    actor_id     TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    git_name     TEXT NOT NULL,
    git_email    TEXT NOT NULL,
    last_seen_ms INTEGER NOT NULL
);
"#;

const K_CURRENT_REVISION: &str = "current_revision";
const K_CONTROL_VERSION: &str = "control_version";
const K_SESSION: &str = "session";
const K_SCHEMA: &str = "schema_version";
const K_PUB_SEQ: &str = "publication_sequence";
const K_MAX_FILE_SIZE: &str = "max_file_size";

#[derive(Debug, Clone)]
pub struct ActorRecord {
    pub actor_id: Uuid,
    pub display_name: String,
    pub git_name: String,
    pub git_email: String,
    pub last_seen_ms: i64,
}

pub struct HostStore {
    conn: Connection,
}

impl HostStore {
    pub fn open(path: &Path) -> Result<HostStore> {
        let conn = db::open(path)?;
        conn.execute_batch(SCHEMA)?;
        if db::get_meta(&conn, K_SCHEMA)?.is_none() {
            db::set_meta(&conn, K_SCHEMA, "1")?;
            db::set_u64(&conn, K_CURRENT_REVISION, 0)?;
            db::set_u64(&conn, K_CONTROL_VERSION, 1)?;
            db::set_u64(&conn, K_PUB_SEQ, 0)?;
        }
        Ok(HostStore { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ---------------------------------------------------------------- session

    pub fn session(&self) -> Result<Option<SessionInfo>> {
        db::get_json(&self.conn, K_SESSION)
    }

    pub fn set_session(&self, info: &SessionInfo) -> Result<()> {
        db::set_json(&self.conn, K_SESSION, info)
    }

    pub fn current_revision(&self) -> Result<u64> {
        db::get_u64(&self.conn, K_CURRENT_REVISION, 0)
    }

    pub fn control_version(&self) -> Result<u64> {
        db::get_u64(&self.conn, K_CONTROL_VERSION, 1)
    }

    pub fn bump_control_version(&self) -> Result<u64> {
        let v = self.control_version()? + 1;
        db::set_u64(&self.conn, K_CONTROL_VERSION, v)?;
        Ok(v)
    }

    /// The session's file size limit. Durable, so it survives a host restart
    /// and cannot silently fall back to the default under a replica that has
    /// already been told otherwise.
    pub fn max_file_size(&self) -> Result<u64> {
        db::get_u64(&self.conn, K_MAX_FILE_SIZE, DEFAULT_MAX_FILE_SIZE)
    }

    pub fn set_max_file_size(&self, bytes: u64) -> Result<()> {
        db::set_u64(&self.conn, K_MAX_FILE_SIZE, bytes)
    }

    /// The largest canonical entry, if there is one. What a proposed lowering
    /// of the limit has to clear.
    pub fn largest_manifest_entry(&self) -> Result<Option<(RepoPath, u64)>> {
        let mut largest: Option<(RepoPath, u64)> = None;
        for (path, entry) in self.manifest_all()? {
            let beats = match &largest {
                Some((_, size)) => entry.size > *size,
                None => true,
            };
            if beats {
                largest = Some((path, entry.size));
            }
        }
        Ok(largest)
    }

    // --------------------------------------------------------------- manifest

    /// Discard all canonical state so a brand new session can start from r0.
    ///
    /// Called only when creating a session that is not a continuation of the one
    /// this store holds; a resumed session never touches it.
    pub fn reset(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        for table in [
            "base_manifest",
            "manifest",
            "revisions",
            "operations",
            "tasks",
            "conflicts",
            "preparations",
            "publications",
            "actors",
        ] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.commit()?;
        db::set_u64(&self.conn, K_CURRENT_REVISION, 0)?;
        db::set_u64(&self.conn, K_CONTROL_VERSION, 1)?;
        db::set_u64(&self.conn, K_PUB_SEQ, 0)?;
        db::set_u64(&self.conn, K_MAX_FILE_SIZE, DEFAULT_MAX_FILE_SIZE)?;
        Ok(())
    }

    /// Install the r0 baseline. Only valid before any revision exists.
    pub fn install_base_manifest(&mut self, entries: &BTreeMap<RepoPath, FileEntry>) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM base_manifest", [])?;
        tx.execute("DELETE FROM manifest", [])?;
        {
            let mut base_stmt =
                tx.prepare("INSERT INTO base_manifest(path, entry) VALUES (?1, ?2)")?;
            let mut man_stmt =
                tx.prepare("INSERT INTO manifest(path, collision_key, entry) VALUES (?1, ?2, ?3)")?;
            for (path, entry) in entries {
                let json = serde_json::to_string(entry)?;
                base_stmt.execute(params![path.as_str(), json])?;
                man_stmt.execute(params![path.as_str(), path.collision_key(), json])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn manifest_entry(&self, path: &RepoPath) -> Result<Option<FileEntry>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT entry FROM manifest WHERE path = ?1")?;
        let text: Option<String> = stmt.query_row([path.as_str()], |r| r.get(0)).optional()?;
        db::parse_opt_json(text)
    }

    pub fn manifest_all(&self) -> Result<BTreeMap<RepoPath, FileEntry>> {
        let mut stmt = self.conn.prepare("SELECT path, entry FROM manifest")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (path, entry) = row?;
            out.insert(RepoPath::new(&path)?, serde_json::from_str(&entry)?);
        }
        Ok(out)
    }

    pub fn manifest_len(&self) -> Result<u64> {
        let mut stmt = self.conn.prepare_cached("SELECT COUNT(*) FROM manifest")?;
        Ok(stmt.query_row([], |r| r.get::<_, i64>(0))? as u64)
    }

    /// A path whose portable collision key equals `key` but which is not
    /// `path` itself (specification section 48).
    pub fn colliding_path(&self, path: &RepoPath) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path FROM manifest WHERE collision_key = ?1 AND path <> ?2 LIMIT 1",
        )?;
        Ok(stmt
            .query_row(params![path.collision_key(), path.as_str()], |r| r.get(0))
            .optional()?)
    }

    /// `manifest(revision)[path]` — the entry for one path at one historical
    /// revision. Used to validate every incoming operation's declared base
    /// (specification section 25).
    pub fn historical_entry(&self, path: &RepoPath, revision: u64) -> Result<Option<FileEntry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT after_entry FROM revisions
             WHERE path = ?1 AND revision <= ?2
             ORDER BY revision DESC LIMIT 1",
        )?;
        let found: Option<Option<String>> = stmt
            .query_row(params![path.as_str(), revision as i64], |r| r.get(0))
            .optional()?;
        match found {
            Some(text) => db::parse_opt_json(text),
            None => {
                let mut base = self
                    .conn
                    .prepare_cached("SELECT entry FROM base_manifest WHERE path = ?1")?;
                let text: Option<String> =
                    base.query_row([path.as_str()], |r| r.get(0)).optional()?;
                db::parse_opt_json(text)
            }
        }
    }

    /// Reconstruct the complete manifest at `revision` (specification
    /// sections 7.5, 127, 176).
    pub fn manifest_at(&self, revision: u64) -> Result<BTreeMap<RepoPath, FileEntry>> {
        let mut out = BTreeMap::new();
        {
            let mut stmt = self.conn.prepare("SELECT path, entry FROM base_manifest")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (path, entry) = row?;
                out.insert(RepoPath::new(&path)?, serde_json::from_str(&entry)?);
            }
        }
        let mut stmt = self.conn.prepare(
            "SELECT path, after_entry FROM revisions WHERE revision <= ?1 ORDER BY revision ASC",
        )?;
        let rows = stmt.query_map([revision as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (path, after) = row?;
            let key = RepoPath::new(&path)?;
            match db::parse_opt_json::<FileEntry>(after)? {
                Some(entry) => {
                    out.insert(key, entry);
                }
                None => {
                    out.remove(&key);
                }
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------- revisions

    pub fn revision(&self, revision: u64) -> Result<Option<Revision>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT revision, operation_id, actor_id, task_id, timestamp_ms, path,
                    before_entry, after_entry
             FROM revisions WHERE revision = ?1",
        )?;
        let row = stmt
            .query_row([revision as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            })
            .optional()?;
        match row {
            None => Ok(None),
            Some((rev, op, actor, task, ts, path, before, after)) => Ok(Some(Revision {
                revision: rev as u64,
                operation_id: parse_uuid(&op)?,
                actor_id: parse_uuid(&actor)?,
                task_id: match task {
                    Some(t) => Some(parse_uuid(&t)?),
                    None => None,
                },
                timestamp_ms: ts,
                path: RepoPath::new(&path)?,
                before: db::parse_opt_json(before)?,
                after: db::parse_opt_json(after)?,
            })),
        }
    }

    pub fn revisions_in_range(&self, from: u64, to: u64) -> Result<Vec<Revision>> {
        let mut out = Vec::new();
        if from > to {
            return Ok(out);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT revision FROM revisions WHERE revision >= ?1 AND revision <= ?2 ORDER BY revision ASC")?;
        let rows = stmt.query_map(params![from as i64, to as i64], |r| r.get::<_, i64>(0))?;
        let ids: Vec<i64> = rows.collect::<rusqlite::Result<_>>()?;
        for id in ids {
            if let Some(rev) = self.revision(id as u64)? {
                out.push(rev);
            }
        }
        Ok(out)
    }

    /// Revisions attributed to `task_id` within `(after, through]`.
    pub fn task_revisions_since(
        &self,
        task_id: &Uuid,
        after: u64,
        through: u64,
    ) -> Result<Vec<u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT revision FROM revisions
             WHERE task_id = ?1 AND revision > ?2 AND revision <= ?3
             ORDER BY revision ASC",
        )?;
        let rows = stmt.query_map(
            params![task_id.to_string(), after as i64, through as i64],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<i64>>>()?
            .into_iter()
            .map(|v| v as u64)
            .collect())
    }

    pub fn unassigned_revisions(&self, after: u64, through: u64) -> Result<Vec<u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT revision FROM revisions
             WHERE task_id IS NULL AND revision > ?1 AND revision <= ?2
             ORDER BY revision ASC",
        )?;
        let rows = stmt.query_map(params![after as i64, through as i64], |r| {
            r.get::<_, i64>(0)
        })?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<i64>>>()?
            .into_iter()
            .map(|v| v as u64)
            .collect())
    }

    /// Distinct Task IDs that contributed accepted revisions in `(after, through]`.
    pub fn tasks_contributing(&self, after: u64, through: u64) -> Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT task_id FROM revisions
             WHERE task_id IS NOT NULL AND revision > ?1 AND revision <= ?2",
        )?;
        let rows = stmt.query_map(params![after as i64, through as i64], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(parse_uuid(&row?)?);
        }
        Ok(out)
    }

    /// Per-actor revision counts in `(after, through]`.
    pub fn contributor_counts(&self, after: u64, through: u64) -> Result<Vec<(Uuid, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT actor_id, COUNT(*) FROM revisions
             WHERE revision > ?1 AND revision <= ?2 GROUP BY actor_id",
        )?;
        let rows = stmt.query_map(params![after as i64, through as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (actor, count) = row?;
            out.push((parse_uuid(&actor)?, count as u64));
        }
        Ok(out)
    }

    // ------------------------------------------------------------- operations

    pub fn lookup_operation(
        &self,
        operation_id: &Uuid,
    ) -> Result<Option<(String, OperationOutcome)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT payload_hash, result FROM operations WHERE operation_id = ?1",
        )?;
        let row: Option<(String, String)> = stmt
            .query_row([operation_id.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match row {
            None => Ok(None),
            Some((hash, result)) => Ok(Some((hash, serde_json::from_str(&result)?))),
        }
    }

    /// Record an operation result that consumes no revision (no-op,
    /// convergence, conflict, rejection). Durable before acknowledgement.
    pub fn record_operation_result(
        &self,
        operation_id: &Uuid,
        actor_id: &Uuid,
        payload_hash: &str,
        outcome: &OperationOutcome,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO operations(operation_id, actor_id, payload_hash, result, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(operation_id) DO UPDATE SET result = excluded.result",
            params![
                operation_id.to_string(),
                actor_id.to_string(),
                payload_hash,
                serde_json::to_string(outcome)?,
                crate::util::now_ms()
            ],
        )?;
        Ok(())
    }

    /// Atomically append a revision, update the canonical manifest, and record
    /// the operation result (specification section 68, steps 4-8).
    ///
    /// The blob referenced by `after` must already be durably installed.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_revision(
        &mut self,
        operation_id: &Uuid,
        actor_id: &Uuid,
        task_id: Option<&Uuid>,
        payload_hash: &str,
        path: &RepoPath,
        before: Option<&FileEntry>,
        after: Option<&FileEntry>,
        make_outcome: impl FnOnce(u64) -> OperationOutcome,
    ) -> Result<(u64, Revision)> {
        let next = self.current_revision()? + 1;
        let now = crate::util::now_ms();
        let outcome = make_outcome(next);
        let before_json = db::opt_json(&before.cloned())?;
        let after_json = db::opt_json(&after.cloned())?;

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO revisions(revision, operation_id, actor_id, task_id, timestamp_ms,
                                   path, before_entry, after_entry)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                next as i64,
                operation_id.to_string(),
                actor_id.to_string(),
                task_id.map(|t| t.to_string()),
                now,
                path.as_str(),
                before_json,
                after_json
            ],
        )?;
        match after {
            Some(entry) => {
                tx.execute(
                    "INSERT INTO manifest(path, collision_key, entry) VALUES (?1, ?2, ?3)
                     ON CONFLICT(path) DO UPDATE SET entry = excluded.entry",
                    params![
                        path.as_str(),
                        path.collision_key(),
                        serde_json::to_string(entry)?
                    ],
                )?;
            }
            None => {
                tx.execute("DELETE FROM manifest WHERE path = ?1", [path.as_str()])?;
            }
        }
        tx.execute(
            "INSERT INTO operations(operation_id, actor_id, payload_hash, result, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(operation_id) DO UPDATE SET result = excluded.result",
            params![
                operation_id.to_string(),
                actor_id.to_string(),
                payload_hash,
                serde_json::to_string(&outcome)?,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![K_CURRENT_REVISION, next.to_string()],
        )?;
        tx.commit()?;

        Ok((
            next,
            Revision {
                revision: next,
                operation_id: *operation_id,
                actor_id: *actor_id,
                task_id: task_id.copied(),
                timestamp_ms: now,
                path: path.clone(),
                before: before.cloned(),
                after: after.cloned(),
            },
        ))
    }

    // ------------------------------------------------------------------ tasks

    pub fn put_task(&self, task: &Task) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tasks(id, actor_id, status, data) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET status = excluded.status, data = excluded.data",
            params![
                task.id.to_string(),
                task.actor_id.to_string(),
                task.status.as_str(),
                serde_json::to_string(task)?
            ],
        )?;
        Ok(())
    }

    pub fn task(&self, id: &Uuid) -> Result<Option<Task>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT data FROM tasks WHERE id = ?1")?;
        let text: Option<String> = stmt.query_row([id.to_string()], |r| r.get(0)).optional()?;
        db::parse_opt_json(text)
    }

    pub fn tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare("SELECT data FROM tasks")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<Task>(&row?)?);
        }
        out.sort_by_key(|t| t.created_at_ms);
        Ok(out)
    }

    pub fn active_task_for_actor(&self, actor_id: &Uuid) -> Result<Option<Task>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT data FROM tasks WHERE actor_id = ?1 AND status = 'active' LIMIT 1",
        )?;
        let text: Option<String> = stmt
            .query_row([actor_id.to_string()], |r| r.get(0))
            .optional()?;
        db::parse_opt_json(text)
    }

    pub fn active_tasks(&self) -> Result<Vec<Task>> {
        Ok(self
            .tasks()?
            .into_iter()
            .filter(|t| t.status == TaskStatus::Active)
            .collect())
    }

    // -------------------------------------------------------------- conflicts

    pub fn put_conflict(&self, conflict: &Conflict) -> Result<()> {
        self.conn.execute(
            "INSERT INTO conflicts(id, path, status, created_at_ms, data)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET status = excluded.status, data = excluded.data",
            params![
                conflict.id.to_string(),
                conflict.path.as_str(),
                conflict.status.as_str(),
                conflict.created_at_ms,
                serde_json::to_string(conflict)?
            ],
        )?;
        Ok(())
    }

    pub fn conflict(&self, id: &Uuid) -> Result<Option<Conflict>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT data FROM conflicts WHERE id = ?1")?;
        let text: Option<String> = stmt.query_row([id.to_string()], |r| r.get(0)).optional()?;
        db::parse_opt_json(text)
    }

    pub fn conflicts(&self) -> Result<Vec<Conflict>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM conflicts ORDER BY created_at_ms ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<Conflict>(&row?)?);
        }
        Ok(out)
    }

    pub fn open_conflicts(&self) -> Result<Vec<Conflict>> {
        Ok(self
            .conflicts()?
            .into_iter()
            .filter(|c| c.status == ConflictStatus::Open)
            .collect())
    }

    // ----------------------------------------------------------- preparations

    pub fn put_preparation(&self, prep: &CommitPreparation) -> Result<()> {
        self.conn.execute(
            "INSERT INTO preparations(prepare_id, created_at_ms, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(prepare_id) DO UPDATE SET data = excluded.data",
            params![
                prep.prepare_id.to_string(),
                prep.created_at_ms,
                serde_json::to_string(prep)?
            ],
        )?;
        Ok(())
    }

    pub fn preparation(&self, id: &Uuid) -> Result<Option<CommitPreparation>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT data FROM preparations WHERE prepare_id = ?1")?;
        let text: Option<String> = stmt.query_row([id.to_string()], |r| r.get(0)).optional()?;
        db::parse_opt_json(text)
    }

    pub fn preparations(&self) -> Result<Vec<CommitPreparation>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM preparations ORDER BY created_at_ms ASC")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<CommitPreparation>(&row?)?);
        }
        Ok(out)
    }

    // ----------------------------------------------------------- publications

    pub fn next_publication_sequence(&self) -> Result<u64> {
        Ok(db::get_u64(&self.conn, K_PUB_SEQ, 0)? + 1)
    }

    pub fn put_publication(&self, pubb: &GitPublication) -> Result<()> {
        self.conn.execute(
            "INSERT INTO publications(sequence, commit_oid, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(sequence) DO UPDATE SET data = excluded.data,
                                                 commit_oid = excluded.commit_oid",
            params![
                pubb.sequence as i64,
                pubb.descriptor.commit_oid,
                serde_json::to_string(pubb)?
            ],
        )?;
        let highest = db::get_u64(&self.conn, K_PUB_SEQ, 0)?;
        if pubb.sequence > highest {
            db::set_u64(&self.conn, K_PUB_SEQ, pubb.sequence)?;
        }
        Ok(())
    }

    pub fn latest_publication(&self) -> Result<Option<GitPublication>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM publications ORDER BY sequence DESC LIMIT 1")?;
        let text: Option<String> = stmt.query_row([], |r| r.get(0)).optional()?;
        db::parse_opt_json(text)
    }

    pub fn publications_after(&self, sequence: u64) -> Result<Vec<GitPublication>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM publications WHERE sequence > ?1 ORDER BY sequence ASC")?;
        let rows = stmt.query_map([sequence as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<GitPublication>(&row?)?);
        }
        Ok(out)
    }

    pub fn publication_count(&self) -> Result<u64> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM publications")?;
        Ok(stmt.query_row([], |r| r.get::<_, i64>(0))? as u64)
    }

    // ----------------------------------------------------------------- actors

    pub fn upsert_actor(&self, actor: &ActorRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO actors(actor_id, display_name, git_name, git_email, last_seen_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(actor_id) DO UPDATE SET display_name = excluded.display_name,
                                                 git_name = excluded.git_name,
                                                 git_email = excluded.git_email,
                                                 last_seen_ms = excluded.last_seen_ms",
            params![
                actor.actor_id.to_string(),
                actor.display_name,
                actor.git_name,
                actor.git_email,
                actor.last_seen_ms
            ],
        )?;
        Ok(())
    }

    pub fn actor(&self, actor_id: &Uuid) -> Result<Option<ActorRecord>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT actor_id, display_name, git_name, git_email, last_seen_ms
             FROM actors WHERE actor_id = ?1",
        )?;
        let row = stmt
            .query_row([actor_id.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .optional()?;
        match row {
            None => Ok(None),
            Some((id, name, gname, gmail, seen)) => Ok(Some(ActorRecord {
                actor_id: parse_uuid(&id)?,
                display_name: name,
                git_name: gname,
                git_email: gmail,
                last_seen_ms: seen,
            })),
        }
    }

    pub fn actors(&self) -> Result<Vec<ActorRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT actor_id, display_name, git_name, git_email, last_seen_ms FROM actors",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, gname, gmail, seen) = row?;
            out.push(ActorRecord {
                actor_id: parse_uuid(&id)?,
                display_name: name,
                git_name: gname,
                git_email: gmail,
                last_seen_ms: seen,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------- integrity

    /// Verify that every entry referenced by canonical state has a blob.
    pub fn verify_blob_references(&self, blobs: &crate::blobs::BlobStore) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        let mut stmt = self.conn.prepare("SELECT path, entry FROM manifest")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (path, entry) = row?;
            let entry: FileEntry = serde_json::from_str(&entry)?;
            if !blobs.has(&entry.blob_hash) {
                missing.push(format!("{path} -> {}", entry.blob_hash));
            }
        }
        let mut stmt = self.conn.prepare(
            "SELECT revision, path, after_entry FROM revisions WHERE after_entry IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (rev, path, entry) = row?;
            let entry: FileEntry = serde_json::from_str(&entry)?;
            if !blobs.has(&entry.blob_hash) {
                missing.push(format!("r{rev} {path} -> {}", entry.blob_hash));
            }
        }
        Ok(missing)
    }

    /// Every blob canonical state can still reach.
    ///
    /// The live set for garbage collection. It is the whole revision history
    /// rather than only the current manifest, because a participant that joins
    /// late, or reconnects after being away, catches up through revisions and
    /// must be able to fetch the content each one names. Both sides of every
    /// revision count: `before_entry` is what a rebase or a conflict is
    /// resolved against.
    ///
    /// Publication packs are deliberately absent. A pack is derived state,
    /// reproducible from the Git objects it was built from, and once every
    /// participant has applied it nothing will ask for it again.
    pub fn referenced_blobs(&self) -> Result<HashSet<String>> {
        let mut live = HashSet::new();
        let mut add = |json: Option<String>| -> Result<()> {
            if let Some(json) = json {
                let entry: FileEntry = serde_json::from_str(&json)?;
                live.insert(entry.blob_hash);
            }
            Ok(())
        };
        for table in ["manifest", "base_manifest"] {
            let mut stmt = self.conn.prepare(&format!("SELECT entry FROM {table}"))?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                add(Some(row?))?;
            }
        }
        let mut stmt = self
            .conn
            .prepare("SELECT before_entry, after_entry FROM revisions")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        })?;
        for row in rows {
            let (before, after) = row?;
            add(before)?;
            add(after)?;
        }
        let mut stmt = self.conn.prepare("SELECT data FROM conflicts")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let conflict: Conflict = serde_json::from_str(&row?)?;
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
        Ok(live)
    }

    pub fn integrity_check(&self) -> Result<Vec<String>> {
        db::integrity_check(&self.conn)
    }

    /// Rebuild the derived manifest from the durable revision history
    /// (specification section 146).
    pub fn rebuild_manifest(&mut self) -> Result<u64> {
        let current = self.current_revision()?;
        let rebuilt = self.manifest_at(current)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM manifest", [])?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO manifest(path, collision_key, entry) VALUES (?1, ?2, ?3)")?;
            for (path, entry) in &rebuilt {
                stmt.execute(params![
                    path.as_str(),
                    path.collision_key(),
                    serde_json::to_string(entry)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(rebuilt.len() as u64)
    }
}

/// The coordinator's live blob set, read from its database without owning it.
///
/// A daemon that hosts a session runs a coordinator and a replica over one blob
/// store, so neither half alone knows what is reachable. Collection happens on
/// the replica side - there is exactly one replica per daemon, whatever its
/// role - and this is how it sees the other half. `Ok(empty)` when there is no
/// host database, which is every participant.
pub fn referenced_blobs_at(path: &Path) -> Result<HashSet<String>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let store = HostStore {
        conn: db::open(path)?,
    };
    store.referenced_blobs()
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text)
        .map_err(|e| integrity(format!("Corrupt identifier in Weave storage: {e}")))
}

/// Validate an incoming operation's declared base (specification section 25).
pub fn validate_base(store: &HostStore, op: &FileOperation) -> Result<()> {
    let historical = store.historical_entry(&op.path, op.base_revision)?;
    if !FileEntry::same_as(historical.as_ref(), op.base_entry.as_ref()) {
        return Err(protocol("ProtocolError::InvalidBase").with_detail(format!(
            "Operation for {} declared a base entry that does not match manifest(r{})[{}].\n\
             The client must resynchronize before submitting further operations.",
            op.path, op.base_revision, op.path
        )));
    }
    Ok(())
}
