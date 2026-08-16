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
    Ok(conn)
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get::<_, String>(0)?)),
        None => Ok(None),
    }
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
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
