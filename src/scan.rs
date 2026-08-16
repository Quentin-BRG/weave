// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reading the working tree and writing canonical bytes back to it.
//!
//! Specification sections 31-32 (watcher is a hint, full rescan is
//! authoritative), 45 (safe materialization), 46 (Git ignore semantics),
//! 49 (symlink safety), 51 (file limits).

use crate::blobs::BlobStore;
use crate::error::Result;
use crate::gitx;
use crate::model::{FileEntry, GitMode, CLASSIFY_PREFIX};
use crate::path::{self, RepoPath};
use std::collections::BTreeMap;
use std::path::Path;

/// A path Weave refuses to synchronize, with an actionable reason.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RejectedPath {
    pub path: String,
    pub reason: String,
}

/// Read one repository path from disk into the blob store.
///
/// Returns `Ok(None)` when the path does not exist. `previous` supplies the
/// mode to preserve on platforms that cannot represent the executable bit.
///
/// The content is streamed: on return the bytes are in `blobs`, addressed by
/// the returned entry's `blob_hash`, and were never held whole in memory. Size
/// is not judged here - a file too large for the session is a session-level
/// decision, made against canonical state rather than by the scanner
/// ([docs/BLOB-PLANE.md](../docs/BLOB-PLANE.md)).
pub fn read_path(
    root: &Path,
    path: &RepoPath,
    previous: Option<&FileEntry>,
    blobs: &BlobStore,
) -> Result<Option<FileEntry>> {
    path::ensure_no_indirection(root, path)?;
    let fs_path = path.to_fs_path(root);
    let meta = match std::fs::symlink_metadata(&fs_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(crate::error::unsupported(format!(
            "{path} is a symlink; Weave V1 does not synchronize symlinks."
        )));
    }
    if meta.is_dir() {
        return Ok(None);
    }
    let Some(ingested) = blobs.ingest_file(&fs_path, CLASSIFY_PREFIX)? else {
        return Ok(None);
    };
    let mode = gitx::mode_for_disk_file(&fs_path, previous);
    Ok(Some(FileEntry::from_ingested(&ingested, mode)))
}

/// Result of a full repository rescan.
pub struct ScanResult {
    pub entries: BTreeMap<RepoPath, FileEntry>,
    pub rejected: Vec<RejectedPath>,
}

/// Enumerate every synchronizable file, using Git's own ignore rules.
///
/// A rescan is authoritative: it is how Weave recovers from a watcher that
/// dropped an event (specification sections 32, 185).
pub fn scan_repository(
    root: &Path,
    previous: &BTreeMap<RepoPath, FileEntry>,
    blobs: &BlobStore,
) -> Result<ScanResult> {
    let raw_paths = gitx::list_repository_paths(root)?;
    let mut entries = BTreeMap::new();
    let mut rejected = Vec::new();
    let mut collision_index: std::collections::HashMap<String, RepoPath> =
        std::collections::HashMap::new();

    for raw in raw_paths {
        let repo_path = match RepoPath::new(&raw) {
            Ok(p) => p,
            Err(e) => {
                rejected.push(RejectedPath {
                    path: raw,
                    reason: e.message,
                });
                continue;
            }
        };
        let key = repo_path.collision_key();
        if let Some(other) = collision_index.get(&key) {
            if other != &repo_path {
                rejected.push(RejectedPath {
                    path: repo_path.to_string(),
                    reason: format!(
                        "Collides with {other} under portable case-insensitive comparison."
                    ),
                });
                continue;
            }
        }
        match read_path(root, &repo_path, previous.get(&repo_path), blobs) {
            Ok(Some(entry)) => {
                collision_index.insert(key, repo_path.clone());
                entries.insert(repo_path, entry);
            }
            Ok(None) => {}
            Err(e) => rejected.push(RejectedPath {
                path: repo_path.to_string(),
                reason: e.message,
            }),
        }
    }

    Ok(ScanResult { entries, rejected })
}

/// Write a canonical blob to the working tree using safe replacement semantics.
///
/// Streams out of the blob store, verifying the content hash on the way, so the
/// working file is replaced only by bytes that are provably the canonical ones
/// and no file is ever held whole in memory.
pub fn materialize_file(
    root: &Path,
    path: &RepoPath,
    blobs: &BlobStore,
    blob_hash: &str,
    mode: GitMode,
) -> Result<()> {
    path::ensure_no_indirection(root, path)?;
    let fs_path = path.to_fs_path(root);
    blobs.copy_out(blob_hash, &fs_path)?;
    apply_mode(&fs_path, mode)?;
    Ok(())
}

/// Remove a path from the working tree, pruning directories it emptied.
pub fn materialize_delete(root: &Path, path: &RepoPath) -> Result<()> {
    let fs_path = path.to_fs_path(root);
    match std::fs::remove_file(&fs_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // Prune now-empty parent directories, never touching the root itself.
    let mut cursor = fs_path.parent().map(|p| p.to_path_buf());
    while let Some(dir) = cursor {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        match std::fs::read_dir(&dir) {
            Ok(mut it) => {
                if it.next().is_some() {
                    break;
                }
            }
            Err(_) => break,
        }
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
        cursor = dir.parent().map(|p| p.to_path_buf());
    }
    Ok(())
}

fn apply_mode(fs_path: &Path, mode: GitMode) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(fs_path)?;
        let mut perms = meta.permissions();
        let current = perms.mode();
        let desired = if mode.is_executable() {
            current | 0o111
        } else {
            current & !0o111
        };
        if current != desired {
            perms.set_mode(desired);
            std::fs::set_permissions(fs_path, perms)?;
        }
    }
    #[cfg(not(unix))]
    {
        // Windows checkouts cannot represent the executable bit per file; the
        // canonical mode is carried in Weave metadata and reproduced by the
        // host when Git objects are constructed.
        let _ = (fs_path, mode);
    }
    Ok(())
}
