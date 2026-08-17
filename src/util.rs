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
    install_atomic(&tmp, path)
}

/// Move a already-flushed temporary file onto `path`, replacing it.
///
/// Split out of [`write_atomic`] because streamed writes - blob installation and
/// materialization of a large file - flush their own file and only need the
/// replacement half.
pub fn install_atomic(tmp: &Path, path: &Path) -> Result<()> {
    // `fs::rename` replaces an existing destination on both Unix and Windows.
    match fs::rename(tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Windows can transiently fail a replace if the destination is open
            // by an antivirus/indexer. Retry a small number of times.
            let mut last = e;
            for _ in 0..10 {
                std::thread::sleep(std::time::Duration::from_millis(25));
                match fs::rename(tmp, path) {
                    Ok(()) => return Ok(()),
                    Err(e2) => last = e2,
                }
            }
            let _ = fs::remove_file(tmp);
            Err(persistence(format!(
                "Could not replace {}: {last}",
                path.display()
            )))
        }
    }
}

pub fn temp_sibling(path: &Path) -> PathBuf {
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

// ---------------------------------------------------------------------------
// Sizes
// ---------------------------------------------------------------------------

/// Human form of a byte count, in the binary units people actually mean.
///
/// Deliberately short: this appears inside sentences a user reads once, not in
/// a table to be aligned.
pub fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        let value = bytes as f64 / GIB as f64;
        return format!("{value:.1} GiB");
    }
    if bytes >= MIB {
        let value = bytes as f64 / MIB as f64;
        if value >= 10.0 {
            return format!("{} MiB", value.round() as u64);
        }
        return format!("{value:.1} MiB");
    }
    if bytes >= KIB {
        return format!("{} KiB", bytes / KIB);
    }
    format!("{bytes} bytes")
}

/// Parse a size a person typed: `134217728`, `128MiB`, `128 MB`, `2g`.
///
/// Both spellings of every unit mean the binary one. Nobody typing `128MB` at a
/// file-size limit means 128,000,000, and quietly giving them 2.4% less than
/// they asked for would be worse than the pedantry it avoids.
pub fn parse_size(text: &str) -> Result<u64> {
    let raw = text.trim();
    let lower = raw.to_ascii_lowercase();
    let digits_end = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(digits_end);
    let unit = unit.trim();
    let multiplier: u64 = match unit {
        "" | "b" | "byte" | "bytes" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => {
            return Err(crate::error::usage(format!("Unknown size unit `{other}`."))
                .with_detail("Use bytes, or a size like 64MiB, 256MB or 2GiB."))
        }
    };
    let value: f64 = number.parse().map_err(|_| {
        crate::error::usage(format!("`{raw}` is not a size."))
            .with_detail("Give a size like 128MiB, 512MB or a plain number of bytes.")
    })?;
    if !value.is_finite() || value < 0.0 {
        return Err(crate::error::usage(format!("`{raw}` is not a size.")));
    }
    let bytes = value * multiplier as f64;
    if bytes > u64::MAX as f64 {
        return Err(crate::error::usage(format!("`{raw}` is too large.")));
    }
    Ok(bytes as u64)
}

/// Free space on the filesystem holding `path`, when the platform will say.
///
/// `None` means "not known", never "none left": every caller treats an unknown
/// answer as permission to continue. A check that cannot be made must not
/// become a refusal.
pub fn available_space(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        // Declared here rather than pulled in as a dependency: one call, one
        // signature, stable since Windows 2000.
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(
                directory: *const u16,
                free_bytes_available_to_caller: *mut u64,
                total_bytes: *mut u64,
                total_free_bytes: *mut u64,
            ) -> i32;
        }
        let directory = existing_ancestor(path)?;
        let mut wide: Vec<u16> = directory.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        Some(free)
    }
    #[cfg(not(windows))]
    {
        // `statvfs` would need a libc dependency for a struct whose layout
        // differs across the platforms Weave supports. `df` is POSIX, and this
        // runs at most once per check rather than per file.
        let directory = existing_ancestor(path)?;
        let out = std::process::Command::new("df")
            .arg("-Pk")
            .arg(&directory)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().nth(1)?;
        let available_kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
        Some(available_kib.saturating_mul(1024))
    }
}

/// The nearest ancestor of `path` that exists, so the probe can be aimed at a
/// directory Weave is about to create files in but has not created yet.
fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        cursor = candidate.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_the_way_people_write_them() {
        assert_eq!(parse_size("128MiB").unwrap(), 128 * 1024 * 1024);
        assert_eq!(parse_size("128 mb").unwrap(), 128 * 1024 * 1024);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("1048576").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1.5MiB").unwrap(), 1024 * 1024 + 512 * 1024);
        assert!(parse_size("many").is_err());
        assert!(parse_size("12 furlongs").is_err());
    }

    #[test]
    fn sizes_are_formatted_for_a_sentence() {
        assert_eq!(format_size(128 * 1024 * 1024), "128 MiB");
        assert_eq!(format_size(1024 * 1024 + 512 * 1024), "1.5 MiB");
        assert_eq!(format_size(4096), "4 KiB");
        assert_eq!(format_size(12), "12 bytes");
    }

    /// Whatever the platform answers, it must be an answer we can act on: a
    /// number, or an honest "not known".
    #[test]
    fn free_space_is_either_known_or_unknown() {
        let here = std::env::temp_dir();
        if let Some(bytes) = available_space(&here) {
            assert!(bytes > 0, "a writable temp directory with no space at all");
        }
    }
}
