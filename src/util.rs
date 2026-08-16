// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small cross-platform helpers: time, durable writes, secrets, permissions.

use crate::error::{persistence, Result};
use base64::Engine as _;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> i64 {
    now_ms() / 1000
}

/// Lowercase hexadecimal SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `n` bytes from the OS CSPRNG, hex-encoded.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    hex(&buf)
}

pub const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;
pub const B64_URL_NOPAD: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn b64_decode(text: &str) -> Result<Vec<u8>> {
    B64.decode(text.as_bytes())
        .map_err(|e| crate::error::protocol(format!("Invalid base64 payload: {e}")))
}

/// Write `bytes` to `path` durably: temporary sibling, flush, atomic rename.
///
/// Specification section 45 (safe filesystem materialization) and section 68
/// (blob durability before acknowledgement) both depend on this.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        persistence(format!(
            "Cannot write to {}: no parent directory",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    let tmp = temp_sibling(path);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
    }
    // `fs::rename` replaces an existing destination on both Unix and Windows.
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Windows can transiently fail a replace if the destination is open
            // by an antivirus/indexer. Retry a small number of times.
            let mut last = e;
            for _ in 0..10 {
                std::thread::sleep(std::time::Duration::from_millis(25));
                match fs::rename(&tmp, path) {
                    Ok(()) => return Ok(()),
                    Err(e2) => last = e2,
                }
            }
            let _ = fs::remove_file(&tmp);
            Err(persistence(format!(
                "Could not replace {}: {last}",
                path.display()
            )))
        }
    }
}

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{name}.weave-tmp-{}", random_hex(8)))
}

/// Restrict a file to the current user as far as the platform allows.
///
/// Specification sections 29 and 172: runtime discovery data and the local IPC
/// token must not be world-readable.
pub fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    #[cfg(windows)]
    {
        // Remove inherited ACEs and grant full control only to the current user.
        // Best effort: a failure here is not fatal, the file still lives inside
        // the user's own repository.
        let user = std::env::var("USERNAME").unwrap_or_default();
        if !user.is_empty() {
            let _ = std::process::Command::new("icacls")
                .arg(path)
                .arg("/inheritance:r")
                .arg("/grant:r")
                .arg(format!("{user}:(F)"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    Ok(())
}

/// Format a revision the way the CLI and specification examples do (`r548`).
pub fn fmt_revision(revision: u64) -> String {
    format!("r{revision}")
}

/// Short display form of a UUID, e.g. `C-8F21` or `T-8F21`.
pub fn short_id(prefix: char, id: &uuid::Uuid) -> String {
    let s = id.simple().to_string().to_uppercase();
    format!("{prefix}-{}", &s[..4])
}

/// Truncate a git OID for display.
pub fn short_oid(oid: &str) -> String {
    oid.chars().take(7).collect()
}
