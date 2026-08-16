// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Session identity, on-disk layout, invites, runtime discovery and the
//! single-daemon lock.
//!
//! Specification sections 15 (metadata location), 27 (persistent actor
//! identity), 29 (local IPC discovery), 30 (single daemon lock), 55-57
//! (session identity and invites), 172 (local secrets).

use crate::error::{session as session_err, usage, Result};
use crate::proto::SessionInfo;
use crate::util::{restrict_permissions, write_atomic, B64_URL_NOPAD};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;
use zeroize::Zeroizing;

/// All Weave-owned locations for one repository.
///
/// Everything lives under `.git/weave`, so an ordinary Git repository stays
/// completely usable if Weave is removed (specification sections 15, 198).
#[derive(Debug, Clone)]
pub struct Paths {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub weave_dir: PathBuf,
}

impl Paths {
    pub fn discover(start: &Path) -> Result<Paths> {
        let repo_root = crate::gitx::discover_root(start)?;
        let git_dir = crate::gitx::git_dir(&repo_root)?;
        let weave_dir = git_dir.join("weave");
        Ok(Paths {
            repo_root,
            git_dir,
            weave_dir,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.weave_dir)?;
        std::fs::create_dir_all(self.blobs())?;
        std::fs::create_dir_all(self.conflicts())?;
        std::fs::create_dir_all(self.logs())?;
        std::fs::create_dir_all(self.scratch())?;
        Ok(())
    }

    pub fn blobs(&self) -> PathBuf {
        self.weave_dir.join("blobs")
    }
    pub fn conflicts(&self) -> PathBuf {
        self.weave_dir.join("conflicts")
    }
    pub fn logs(&self) -> PathBuf {
        self.weave_dir.join("logs")
    }
    pub fn scratch(&self) -> PathBuf {
        self.weave_dir.join("tmp")
    }
    pub fn host_db(&self) -> PathBuf {
        self.weave_dir.join("host.sqlite")
    }
    pub fn client_db(&self) -> PathBuf {
        self.weave_dir.join("state.sqlite")
    }
    pub fn runtime_json(&self) -> PathBuf {
        self.weave_dir.join("runtime.json")
    }
    pub fn session_json(&self) -> PathBuf {
        self.weave_dir.join("session.json")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.weave_dir.join("daemon.lock")
    }

    /// Repository display name, used only in CLI output.
    pub fn repo_name(&self) -> String {
        self.repo_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repository".into())
    }
}

// ---------------------------------------------------------------------------
// Persistent installation identity (specification section 27)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub actor_id: Uuid,
    /// Optional override for the display name derived from Git configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// User-level Weave data directory.
///
/// `WEAVE_HOME` overrides it, which is what lets one machine run several
/// independent Weave identities (separate profiles, and the integration tests).
pub fn weave_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("WEAVE_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let base = dirs::data_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| session_err("Could not determine a user application data directory."))?;
    Ok(base.join("weave"))
}

fn identity_path() -> Result<PathBuf> {
    Ok(weave_home()?.join("identity.json"))
}

/// Load, or create on first use, the installation-wide actor identity.
pub fn load_or_create_identity() -> Result<Identity> {
    let path = identity_path()?;
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(identity) = serde_json::from_str::<Identity>(&text) {
            return Ok(identity);
        }
    }
    let identity = Identity {
        actor_id: Uuid::new_v4(),
        display_name: None,
    };
    save_identity(&identity)?;
    Ok(identity)
}

pub fn save_identity(identity: &Identity) -> Result<()> {
    let path = identity_path()?;
    write_atomic(&path, serde_json::to_string_pretty(identity)?.as_bytes())?;
    let _ = restrict_permissions(&path);
    Ok(())
}

/// Git identity for this machine, with sensible fallbacks
/// (specification section 27).
#[derive(Debug, Clone)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
    pub email_usable: bool,
}

pub fn git_identity(repo_root: &Path) -> Result<GitIdentity> {
    let name = crate::gitx::config_get(repo_root, "user.name")?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(os_username);
    let email = crate::gitx::config_get(repo_root, "user.email")?.filter(|s| !s.trim().is_empty());
    match email {
        Some(email) => Ok(GitIdentity {
            name,
            email,
            email_usable: true,
        }),
        // Weave must never invent a real-looking personal address
        // (specification section 130), so an unset address stays empty and is
        // reported when a commit actually needs one.
        None => Ok(GitIdentity {
            name,
            email: String::new(),
            email_usable: false,
        }),
    }
}

pub fn os_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "weave-user".to_string())
}

// ---------------------------------------------------------------------------
// Local session record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    Tunnel,
    Lan,
    /// Host-only session with no remote listener.
    Local,
}

/// `.git/weave/session.json` — everything needed to resume this machine's
/// participation, including the session secret. Stored with restrictive
/// permissions and never logged (specification section 172).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub role: crate::model::Role,
    pub session: SessionInfo,
    pub secret: SessionSecret,
    /// WebSocket endpoint. For a host this is the last published endpoint; for
    /// a participant it is the host endpoint to reconnect to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub mode: TransportMode,
    pub created_at_ms: i64,
}

pub fn load_session_record(paths: &Paths) -> Result<Option<SessionRecord>> {
    let p = paths.session_json();
    match std::fs::read_to_string(&p) {
        Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn save_session_record(paths: &Paths, record: &SessionRecord) -> Result<()> {
    let p = paths.session_json();
    write_atomic(&p, serde_json::to_string_pretty(record)?.as_bytes())?;
    let _ = restrict_permissions(&p);
    Ok(())
}

pub fn clear_session_record(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(paths.session_json()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Runtime discovery (specification section 29)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Runtime {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub role: String,
    pub session_id: Uuid,
    pub started_at_ms: i64,
}

pub fn write_runtime(paths: &Paths, runtime: &Runtime) -> Result<()> {
    let p = paths.runtime_json();
    write_atomic(&p, serde_json::to_string_pretty(runtime)?.as_bytes())?;
    restrict_permissions(&p)?;
    Ok(())
}

pub fn read_runtime(paths: &Paths) -> Result<Option<Runtime>> {
    match std::fs::read_to_string(paths.runtime_json()) {
        Ok(text) => Ok(serde_json::from_str(&text).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn clear_runtime(paths: &Paths) -> Result<()> {
    match std::fs::remove_file(paths.runtime_json()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Single-daemon lock (specification section 30)
// ---------------------------------------------------------------------------

/// An exclusive advisory lock on `.git/weave/daemon.lock`, released when the
/// process exits for any reason. A stale lock therefore cannot outlive a dead
/// daemon.
pub struct DaemonLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl DaemonLock {
    pub fn acquire(paths: &Paths) -> Result<DaemonLock> {
        std::fs::create_dir_all(&paths.weave_dir)?;
        let path = paths.lock_file();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        // An OS-level exclusive lock is released when the process exits for any
        // reason, so a stale lock file cannot outlive a dead daemon.
        match file.try_lock() {
            Ok(()) => {}
            Err(_) => {
                let detail = match read_runtime(paths)? {
                    Some(rt) => format!(
                        "Another Weave daemon (pid {}) already controls this working tree.\n\
                         Run `weave status` to inspect it, or `weave stop` to shut it down.",
                        rt.pid
                    ),
                    None => "Another Weave daemon already controls this working tree.".to_string(),
                };
                return Err(
                    session_err("A Weave daemon is already running for this repository.")
                        .with_detail(detail),
                );
            }
        }
        Ok(DaemonLock { _file: file, path })
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = &self.path;
    }
}

// ---------------------------------------------------------------------------
// The session secret
// ---------------------------------------------------------------------------

/// The 256-bit session secret, in the only container it is allowed to live in.
///
/// The secret is the root of trust for the whole session, so a plain `String`
/// is the wrong home for it: dropping one frees the heap buffer without
/// touching the bytes, which leaves the secret readable in whatever allocation
/// reuses that memory, and its `Debug` prints the secret in full. This wrapper
/// zeroizes on drop, redacts under `{:?}`, and derefs to `&str` so callers that
/// legitimately need the characters — HKDF, serialization, the invite encoder —
/// read it without ceremony.
///
/// It cannot make Weave forget a secret that has already been copied elsewhere:
/// `serde_json` builds an ordinary `String` while parsing an invite, the OS may
/// have paged either copy, and the invite itself sits in a file or a terminal
/// scrollback. This closes the copies Weave owns, not the ones it does not.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionSecret(Zeroizing<String>);

impl SessionSecret {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SessionSecret {
    fn from(value: String) -> SessionSecret {
        SessionSecret(Zeroizing::new(value))
    }
}

impl From<&str> for SessionSecret {
    fn from(value: &str) -> SessionSecret {
        SessionSecret(Zeroizing::new(value.to_string()))
    }
}

impl std::ops::Deref for SessionSecret {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

/// Redacted on purpose. `SessionRecord` and `InvitePayload` both derive
/// `Debug`, so without this a single `{:?}` anywhere would print the secret.
impl std::fmt::Debug for SessionSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionSecret(<redacted>)")
    }
}

impl Serialize for SessionSecret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionSecret {
    fn deserialize<D: serde::Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<SessionSecret, D::Error> {
        Ok(SessionSecret::from(String::deserialize(d)?))
    }
}

// ---------------------------------------------------------------------------
// Invites (specification sections 56, 57)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePayload {
    #[serde(rename = "v")]
    pub protocol_version: u32,
    #[serde(rename = "u")]
    pub url: String,
    #[serde(rename = "s")]
    pub session_id: Uuid,
    #[serde(rename = "k")]
    pub secret: SessionSecret,
    #[serde(rename = "b")]
    pub base_commit: String,
    #[serde(rename = "r")]
    pub branch: String,
    #[serde(rename = "n")]
    pub repo_name: String,
}

pub const INVITE_PREFIX: &str = "weave2_";
/// Invites minted before the protocol carried end-to-end encryption.
const LEGACY_INVITE_PREFIX: &str = "weave1_";

pub fn encode_invite(payload: &InvitePayload) -> Result<String> {
    let json = serde_json::to_vec(payload)?;
    Ok(format!("{INVITE_PREFIX}{}", B64_URL_NOPAD.encode(json)))
}

pub fn decode_invite(text: &str) -> Result<InvitePayload> {
    let trimmed = text.trim();
    // Named explicitly rather than swept into "malformed": a `weave1_` invite is
    // a well-formed invite for a protocol that had no application-level
    // encryption, and refusing it is the whole point.
    if trimmed.starts_with(LEGACY_INVITE_PREFIX) {
        return Err(crate::error::protocol(
            "This invite is from Weave 1, which did not encrypt the application protocol.",
        )
        .with_detail(
            "Weave 2 sessions are end-to-end encrypted and cannot accept a Weave 1 invite. \
             Ask the host to upgrade Weave and start a new session, then join with the new \
             invite.",
        ));
    }
    let body = trimmed.strip_prefix(INVITE_PREFIX).ok_or_else(|| {
        usage("That does not look like a Weave invite.")
            .with_detail("A Weave invite starts with `weave2_`. Ask the host to resend it.")
    })?;
    let bytes = B64_URL_NOPAD
        .decode(body.as_bytes())
        .map_err(|_| usage("The Weave invite is malformed or truncated."))?;
    let payload: InvitePayload = serde_json::from_slice(&bytes)
        .map_err(|_| usage("The Weave invite is malformed or truncated."))?;
    if payload.protocol_version != crate::model::PROTOCOL_VERSION {
        return Err(crate::error::protocol(format!(
            "This invite uses Weave protocol version {}, but this build speaks version {}.",
            payload.protocol_version,
            crate::model::PROTOCOL_VERSION
        ))
        .with_detail("Every participant must run a matching Weave version."));
    }
    Ok(payload)
}

/// A 256-bit session secret from the OS CSPRNG (specification section 55).
pub fn new_session_secret() -> SessionSecret {
    SessionSecret::from(crate::util::random_hex(32))
}

/// A local IPC bearer token (specification section 172).
pub fn new_local_token() -> String {
    crate::util::random_hex(24)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_round_trips() {
        let payload = InvitePayload {
            protocol_version: crate::model::PROTOCOL_VERSION,
            url: "wss://example.trycloudflare.com/weave/v2".into(),
            session_id: Uuid::new_v4(),
            secret: new_session_secret(),
            base_commit: "8f21abc".into(),
            branch: "main".into(),
            repo_name: "investor-deck".into(),
        };
        let text = encode_invite(&payload).unwrap();
        assert!(text.starts_with(INVITE_PREFIX));
        let back = decode_invite(&text).unwrap();
        assert_eq!(back.session_id, payload.session_id);
        assert_eq!(back.secret, payload.secret);
        assert_eq!(back.url, payload.url);
    }

    #[test]
    fn a_session_secret_does_not_print_itself() {
        let secret = SessionSecret::from("SENTINEL_SECRET_VALUE_9f2c");
        let shown = format!("{secret:?}");
        assert!(
            !shown.contains("SENTINEL"),
            "Debug leaked the secret: {shown}"
        );

        // A record printed whole must not leak it either — that is the shape
        // that actually reaches a log line.
        let record = SessionRecord {
            role: crate::model::Role::Host,
            session: crate::proto::SessionInfo {
                session_id: Uuid::new_v4(),
                repo_name: "investor-deck".into(),
                branch: "main".into(),
                base_commit: "8f21abc".into(),
                host_actor_id: Uuid::new_v4(),
                host_display_name: "ana".into(),
                created_at_ms: 0,
            },
            secret: secret.clone(),
            endpoint: None,
            mode: TransportMode::Local,
            created_at_ms: 0,
        };
        assert!(!format!("{record:?}").contains("SENTINEL"));

        // Redaction must not have changed what goes on the wire or on disk.
        assert_eq!(
            serde_json::to_string(&secret).unwrap(),
            "\"SENTINEL_SECRET_VALUE_9f2c\""
        );
        assert_eq!(secret.as_str(), "SENTINEL_SECRET_VALUE_9f2c");
    }

    #[test]
    fn rejects_garbage_invite() {
        assert!(decode_invite("not-an-invite").is_err());
        assert!(decode_invite("weave2_%%%%").is_err());
    }

    #[test]
    fn refuses_an_unencrypted_weave_1_invite_by_name() {
        let err = decode_invite("weave1_eyJ2IjoxfQ").unwrap_err();
        assert_eq!(err.class, crate::error::ErrorClass::ProtocolError);
        assert!(err.message.contains("Weave 1"));
        // No silent downgrade: the payload is never even parsed.
        assert!(err.detail.unwrap().contains("end-to-end encrypted"));
    }
}
