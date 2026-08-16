// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Local control endpoint (specification sections 28, 29).
//!
//! `weave host`, `weave join` and `weave resume` run a long-lived daemon; every
//! other command is a short-lived client of that daemon. The endpoint is a
//! newline-delimited JSON protocol on loopback only, authenticated with a
//! random token stored in `.git/weave/runtime.json` with restrictive
//! permissions. One mechanism, identical on Windows, macOS and Linux.

use crate::error::{network, session as session_err, ErrorClass, Result, WeaveError};
use crate::session::{read_runtime, Paths};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Where the resolved bytes for a conflict resolution come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveSource {
    /// Whatever is in the working tree right now (the default).
    WorkingTree,
    /// Keep the canonical host content.
    Canonical,
    /// Use the latest preserved local candidate.
    LocalCandidate,
    /// Use the rejected incoming candidate.
    Incoming,
    /// Resolve by deleting the path.
    Delete,
    /// Use bytes supplied with the request.
    Supplied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum IpcCommand {
    Status,
    Peers,
    Invite,
    Rescan,
    TaskList,
    TaskStart {
        description: String,
        scopes: Vec<String>,
    },
    TaskShow {
        id: String,
    },
    TaskUpdate {
        id: String,
        description: Option<String>,
        scopes: Option<Vec<String>>,
    },
    TaskComplete {
        id: String,
    },
    TaskCancel {
        id: String,
    },
    ConflictList,
    ConflictShow {
        id: String,
    },
    ConflictResolve {
        id: String,
        source: ResolveSource,
        content_b64: Option<String>,
    },
    ConflictDismiss {
        id: String,
    },
    CommitPrepare {
        allow_active_tasks: bool,
    },
    CommitCreate {
        prepare_id: String,
        message: String,
    },
    Push,
    TunnelRestart,
    Stop,
    Leave,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    pub token: String,
    #[serde(flatten)]
    pub command: IpcCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IpcResponse {
    Ok {
        ok: bool,
        data: serde_json::Value,
    },
    Err {
        ok: bool,
        class: ErrorClass,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl IpcResponse {
    pub fn ok(data: serde_json::Value) -> IpcResponse {
        IpcResponse::Ok { ok: true, data }
    }

    pub fn empty() -> IpcResponse {
        IpcResponse::Ok {
            ok: true,
            data: serde_json::json!({}),
        }
    }

    pub fn error(e: &WeaveError) -> IpcResponse {
        IpcResponse::Err {
            ok: false,
            class: e.class,
            message: e.message.clone(),
            detail: e.detail.clone(),
        }
    }

    pub fn into_result(self) -> Result<serde_json::Value> {
        match self {
            IpcResponse::Ok { data, .. } => Ok(data),
            IpcResponse::Err {
                class,
                message,
                detail,
                ..
            } => {
                let mut e = WeaveError::new(class, message);
                e.detail = detail;
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Blocking client used by short-lived CLI commands
// ---------------------------------------------------------------------------

/// Send one command to the repository's running daemon.
pub fn call(paths: &Paths, command: IpcCommand) -> Result<serde_json::Value> {
    call_with_timeout(paths, command, Duration::from_secs(120))
}

pub fn call_with_timeout(
    paths: &Paths,
    command: IpcCommand,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let runtime = read_runtime(paths)?.ok_or_else(no_daemon)?;
    let addr = format!("127.0.0.1:{}", runtime.port);
    let stream = TcpStream::connect(&addr).map_err(|e| {
        no_daemon().with_detail(format!(
            "Could not reach the local Weave daemon at {addr}: {e}"
        ))
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let request = IpcRequest {
        token: runtime.token,
        command,
    };
    let mut writer = stream.try_clone()?;
    let line = serde_json::to_string(&request)?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    let read = reader.read_line(&mut response)?;
    if read == 0 {
        return Err(network(
            "The Weave daemon closed the connection without replying.",
        ));
    }
    let response: IpcResponse = serde_json::from_str(response.trim())?;
    response.into_result()
}

/// Is a daemon reachable for this repository?
pub fn daemon_is_running(paths: &Paths) -> bool {
    match read_runtime(paths) {
        Ok(Some(runtime)) => TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", runtime.port).parse().unwrap(),
            Duration::from_millis(400),
        )
        .is_ok(),
        _ => false,
    }
}

fn no_daemon() -> WeaveError {
    session_err("No Weave session is running for this repository.").with_detail(
        "Start one with `weave host` (or `weave join` to enter someone else's session).",
    )
}
