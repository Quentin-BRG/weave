// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SQLite helpers shared by the host and client stores.
//!
//! Durability matters more than throughput here: an acknowledged operation
//! must survive an immediate crash (specification sections 68, 69, 144), so
//! the connection runs in WAL mode with `synchronous = FULL`.

use crate::error::Result;
use rusqlite::Connection;
use std::path::Path;

/// Read-only probe for callers such as `recover`, which may inspect an active
/// current-version session but must lock before upgrading a legacy database.
pub fn needs_migration(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta')",
        [],
        |row| row.get(0),
    )?)
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // FULL, not NORMAL: WAL+NORMAL can lose the most recent commits on power
    // loss, which would break the meaning of an operation acknowledgement.
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    migrate(&conn, path)?;
    Ok(conn)
}

fn migrate(conn: &Connection, path: &Path) -> Result<()> {
    let legacy: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta')",
        [],
        |r| r.get(0),
    )?;
    if legacy {
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .ok();
        if version.as_deref().is_some_and(|v| v != "1") {
            return Err(crate::error::integrity(
                "Unsupported Weave database schema; use a matching version.",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let dir = path.parent().unwrap().join("backups").join(&id);
        std::fs::create_dir_all(&dir)?;
        crate::backup::restrict_directory(&dir)?;
        conn.execute(
            "VACUUM INTO ?1",
            [dir.join(path.file_name().unwrap())
                .to_string_lossy()
                .as_ref()],
        )?;
        crate::util::write_atomic(
            &dir.join("backup.json"),
            &serde_json::to_vec(&serde_json::json!({"id":id,"reason":"schema-v1-migration"}))?,
        )?;
        crate::backup::sync_directory(&dir)?;
        crate::backup::sync_directory(dir.parent().unwrap())?;
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch("ALTER TABLE meta RENAME TO meta_v4; UPDATE meta_v4 SET value='2' WHERE key='schema_version';")?;
        // Old releases ignore the stored schema version. Their first metadata
        // query must fail instead of silently opening a migrated replica.
        tx.execute_batch(
            "CREATE VIEW meta AS SELECT key,value FROM meta_v4 WHERE weave_requires_protocol_4()",
        )?;
        tx.commit()?;
    } else {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS meta_v4 (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE VIEW IF NOT EXISTS meta AS SELECT key,value FROM meta_v4 WHERE weave_requires_protocol_4();")?;
    }
    if let Some(version) = get_meta(conn, "schema_version")? {
        if version != "2" {
            return Err(crate::error::integrity(
                "Unsupported Weave database schema.",
            ));
        }
    }
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare_cached("SELECT value FROM meta_v4 WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get::<_, String>(0)?)),
        None => Ok(None),
    }
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta_v4(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

pub fn get_u64(conn: &Connection, key: &str, default: u64) -> Result<u64> {
    Ok(get_meta(conn, key)?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default))
}

pub fn set_u64(conn: &Connection, key: &str, value: u64) -> Result<()> {
    set_meta(conn, key, &value.to_string())
}

pub fn get_json<T: serde::de::DeserializeOwned>(conn: &Connection, key: &str) -> Result<Option<T>> {
    match get_meta(conn, key)? {
        Some(text) => Ok(Some(serde_json::from_str(&text)?)),
        None => Ok(None),
    }
}

pub fn set_json<T: serde::Serialize>(conn: &Connection, key: &str, value: &T) -> Result<()> {
    set_meta(conn, key, &serde_json::to_string(value)?)
}

/// Serialize an optional value to JSON text, or SQL NULL when absent.
pub fn opt_json<T: serde::Serialize>(value: &Option<T>) -> Result<Option<String>> {
    match value {
        Some(v) => Ok(Some(serde_json::to_string(v)?)),
        None => Ok(None),
    }
}

pub fn parse_opt_json<T: serde::de::DeserializeOwned>(text: Option<String>) -> Result<Option<T>> {
    match text {
        Some(t) => Ok(Some(serde_json::from_str(&t)?)),
        None => Ok(None),
    }
}

/// Run `PRAGMA integrity_check` and return the messages if it is not "ok".
pub fn integrity_check(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut problems = Vec::new();
    for row in rows {
        let v = row?;
        if v != "ok" {
            problems.push(v);
        }
    }
    Ok(problems)
}
