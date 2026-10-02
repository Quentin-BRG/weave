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
    InviteRefresh,
    JoinEndpoint {
        invite: String,
    },
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
        /// A file on this machine holding the resolved bytes.
        ///
        /// A path rather than the content: the CLI and the daemon share a
        /// filesystem, and a resolution of a large binary has no business being
        /// base64-encoded through a local socket.
        content_file: Option<String>,
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
    LimitShow,
    LimitSet {
        max_file_size: u64,
    },
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

/// A stopped daemon is established by acquiring its OS lock, never by a failed socket.
#[derive(Debug, PartialEq, Eq)]
pub enum DaemonState {
    Active,
    Stopped,
}

pub fn daemon_state(paths: &Paths) -> Result<DaemonState> {
    // A malformed record is an explicit diagnostic even when the lock is free.
    let runtime = read_runtime(paths)?;
    if crate::session::DaemonLock::try_acquire(paths)?.is_some() {
        return Ok(DaemonState::Stopped);
    }
    if runtime.is_none() {
        return Err(session_err(
            "Daemon state is unknown: repository locked but no runtime is available.",
        ));
    }
    call_with_timeout(paths, IpcCommand::Status, Duration::from_secs(2)).map_err(|e| {
        session_err("Daemon state is unknown: local control is unreachable.")
            .with_detail(e.to_string())
    })?;
    Ok(DaemonState::Active)
}

/// Wait for shutdown, then detach while holding the exclusive repository lock.
/// The daemon's reply only acknowledges the request, not its completion.
pub fn stop_and_wait(paths: &Paths, leave: bool) -> Result<serde_json::Value> {
    let expected_session =
        crate::session::load_session_record(paths)?.map(|r| r.session.session_id);
    let mut lock = crate::session::DaemonLock::try_acquire(paths)?;
    if lock.is_none() {
        call(paths, IpcCommand::Stop)?;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(_lock) = lock.take() {
            if leave
                && crate::session::load_session_record(paths)?.map(|r| r.session.session_id)
                    != expected_session
            {
                return Err(session_err(
                    "The saved session changed during shutdown; departure was not confirmed.",
                ));
            }
            let backup = if leave && crate::session::load_session_record(paths)?.is_some() {
                let backup = crate::backup::archive_session(paths, "leave")?;
                crate::backup::capture_worktree(paths, &backup)?;
                crate::session::clear_session_record(paths)?;
                Some(backup)
            } else {
                None
            };
            crate::session::clear_runtime(paths)?;
            return Ok(serde_json::json!({"stopped": true, "left": leave, "backup": backup}));
        }
        if std::time::Instant::now() >= deadline {
            return Err(session_err(
                "Weave has not finished stopping; departure was not confirmed.",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
        lock = crate::session::DaemonLock::try_acquire(paths)?;
    }
}

fn no_daemon() -> WeaveError {
    session_err("No Weave session is running for this repository.").with_detail(
        "Start one with `weave host` (or `weave join` to enter someone else's session).",
    )
}
