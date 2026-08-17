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
use std::collections::{BTreeMap, HashMap};
use std::fs::Metadata;
use std::path::Path;

/// A path Weave refuses to synchronize, with an actionable reason.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RejectedPath {
    pub path: String,
    pub reason: String,
}

/// Filesystem timestamp resolution, generously rounded up. Content whose
/// modification time is within this of being read is never cached.
const RACY_WINDOW_MS: i64 = 2_000;

/// What the filesystem said about a path the last time its content was hashed.
#[derive(Debug, Clone)]
struct Stat {
    size: u64,
    mtime_ms: i64,
    entry: FileEntry,
}

/// A metadata cache in front of the hash.
///
/// A rescan is authoritative and runs on a timer, which used to mean re-reading
/// every byte of the repository every few seconds. That is nothing for a deck of
/// Markdown files and absurd for a repository holding a few hundred megabytes,
/// so the scan asks the filesystem first: a file whose size and modification
/// time are exactly what they were when it was last hashed is taken to be the
/// same file.
///
/// The hash stays the only truth. Every entry served from here is one this
/// process hashed itself, in this session, from this file; the cache is never
/// persisted, so a restart re-reads the repository and cannot inherit a stale
/// belief. Three things keep it honest:
///
/// - an entry whose modification time is too close to when it was recorded is
///   not cached at all. Git calls these "racily clean": a file rewritten twice
///   within one timestamp tick would otherwise look untouched.
/// - an entry is refused if the blob store no longer holds its content, so a
///   hit always means the bytes are there to be sent or written.
/// - the file mode is re-read on every hit, because changing it need not move
///   the modification time.
///
/// What remains is the ordinary case it exists for: nothing changed, and Weave
/// says so without reading a single byte.
#[derive(Debug, Default)]
pub struct ScanCache {
    entries: HashMap<RepoPath, Stat>,
}

impl ScanCache {
    pub fn new() -> ScanCache {
        ScanCache::default()
    }

    pub fn forget(&mut self, path: &RepoPath) {
        self.entries.remove(path);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn hit(&self, path: &RepoPath, meta: &Metadata, blobs: &BlobStore) -> Option<FileEntry> {
        let stat = self.entries.get(path)?;
        if stat.size != meta.len() || Some(stat.mtime_ms) != mtime_ms(meta) {
            return None;
        }
        if !blobs.has(&stat.entry.blob_hash) {
            return None;
        }
        Some(stat.entry.clone())
    }

    fn remember(&mut self, path: &RepoPath, meta: &Metadata, entry: &FileEntry, now_ms: i64) {
        let Some(mtime_ms) = mtime_ms(meta) else {
            self.entries.remove(path);
            return;
        };
        if now_ms - mtime_ms < RACY_WINDOW_MS {
            self.entries.remove(path);
            return;
        }
        self.entries.insert(
            path.clone(),
            Stat {
                size: meta.len(),
                mtime_ms,
                entry: entry.clone(),
            },
        );
    }
}

/// Modification time in milliseconds since the epoch, or `None` on a platform
/// or filesystem that will not say. Without it there is no fast path, only the
/// hash.
pub fn mtime_ms(meta: &Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as i64,
    )
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
///
/// `cache` may answer from metadata alone for a file that has not moved since
/// it was last hashed. Callers with nothing to reuse pass a fresh
/// [`ScanCache`]; it is an optimization, never a source of truth.
pub fn read_path(
    root: &Path,
    path: &RepoPath,
    previous: Option<&FileEntry>,
    blobs: &BlobStore,
    cache: &mut ScanCache,
) -> Result<Option<FileEntry>> {
    path::ensure_no_indirection(root, path)?;
    let fs_path = path.to_fs_path(root);
    let meta = match std::fs::symlink_metadata(&fs_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            cache.forget(path);
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        cache.forget(path);
        return Err(crate::error::unsupported(format!(
            "{path} is a symlink; Weave V1 does not synchronize symlinks."
        )));
    }
    if meta.is_dir() {
        cache.forget(path);
        return Ok(None);
    }
    let mode = gitx::mode_for_disk_file(&fs_path, previous);
    if let Some(entry) = cache.hit(path, &meta, blobs) {
        return Ok(Some(FileEntry {
            git_mode: mode,
            ..entry
        }));
    }
    let observed = crate::util::now_ms();
    let Some(ingested) = blobs.ingest_file(&fs_path, CLASSIFY_PREFIX)? else {
        cache.forget(path);
        return Ok(None);
    };
    let entry = FileEntry::from_ingested(&ingested, mode);
    cache.remember(path, &meta, &entry, observed);
    Ok(Some(entry))
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
    cache: &mut ScanCache,
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
        match read_path(root, &repo_path, previous.get(&repo_path), blobs, cache) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("weave-scan-{}", crate::util::random_hex(6)));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn blobs(&self) -> BlobStore {
            BlobStore::open(self.0.join("blobs")).unwrap()
        }

        /// Write a file and backdate it out of the racy window, as a file that
        /// has been sitting there for a while would be.
        fn settled_file(&self, rel: &str, bytes: &[u8]) -> RepoPath {
            let path = self.0.join(rel);
            std::fs::write(&path, bytes).unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::now() - Duration::from_secs(30))
                .unwrap();
            file.sync_all().unwrap();
            RepoPath::new(rel).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read(scratch: &Scratch, path: &RepoPath, cache: &mut ScanCache) -> Option<FileEntry> {
        read_path(&scratch.0, path, None, &scratch.blobs(), cache).unwrap()
    }

    /// The whole point: a file whose metadata has not moved is not read again.
    ///
    /// Proved by changing the content behind the cache's back while restoring
    /// the size and modification time it recorded. A hit answers with the old
    /// hash - here, deliberately the wrong one - and a miss could not.
    #[test]
    fn an_unchanged_file_is_answered_from_metadata() {
        let scratch = Scratch::new();
        let file_path = scratch.0.join("asset.bin");
        let path = scratch.settled_file("asset.bin", b"first content");
        let mut cache = ScanCache::new();

        let first = read(&scratch, &path, &mut cache).expect("present");
        assert_eq!(cache.len(), 1);
        let mtime = std::fs::metadata(&file_path).unwrap().modified().unwrap();

        std::fs::write(&file_path, b"other content").unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(&file_path)
            .unwrap();
        file.set_modified(mtime).unwrap();
        drop(file);

        let second = read(&scratch, &path, &mut cache).expect("present");
        assert_eq!(
            second.blob_hash, first.blob_hash,
            "the metadata said nothing changed, so nothing was read"
        );
    }

    /// The hash is still the truth: a real change is seen, even one that keeps
    /// the length identical.
    #[test]
    fn a_changed_file_is_read_again() {
        let scratch = Scratch::new();
        let path = scratch.settled_file("asset.bin", b"aaaaaaaa");
        let mut cache = ScanCache::new();
        let first = read(&scratch, &path, &mut cache).expect("present");

        let second_path = scratch.settled_file("asset.bin", b"bbbbbbbb");
        assert_eq!(second_path, path);
        let second = read(&scratch, &path, &mut cache).expect("present");
        assert_ne!(first.blob_hash, second.blob_hash);
        assert_eq!(second.size, 8);
    }

    /// A file written a moment ago is racily clean: its timestamp cannot
    /// distinguish this content from the next write within the same tick, so
    /// it is not cached at all.
    #[test]
    fn a_freshly_written_file_is_not_cached() {
        let scratch = Scratch::new();
        std::fs::write(scratch.0.join("hot.bin"), b"just now").unwrap();
        let path = RepoPath::new("hot.bin").unwrap();
        let mut cache = ScanCache::new();
        read(&scratch, &path, &mut cache).expect("present");
        assert!(cache.is_empty(), "a racily clean file must not be cached");
    }

    /// A cache hit promises the bytes are available, so content that has left
    /// the blob store cannot be served from it.
    #[test]
    fn a_hit_is_refused_when_the_blob_is_gone() {
        let scratch = Scratch::new();
        let path = scratch.settled_file("asset.bin", b"content that will vanish");
        let mut cache = ScanCache::new();
        let entry = read(&scratch, &path, &mut cache).expect("present");
        assert_eq!(cache.len(), 1);

        let blobs = scratch.blobs();
        std::fs::remove_file(blobs.path_of(&entry.blob_hash).unwrap()).unwrap();
        assert!(!blobs.has(&entry.blob_hash));

        let again = read(&scratch, &path, &mut cache).expect("present");
        assert_eq!(again.blob_hash, entry.blob_hash);
        assert!(blobs.has(&entry.blob_hash), "the content was stored again");
    }

    #[test]
    fn a_vanished_path_is_dropped_from_the_cache() {
        let scratch = Scratch::new();
        let path = scratch.settled_file("asset.bin", b"here for now");
        let mut cache = ScanCache::new();
        read(&scratch, &path, &mut cache).expect("present");
        assert_eq!(cache.len(), 1);
        std::fs::remove_file(scratch.0.join("asset.bin")).unwrap();
        assert!(read(&scratch, &path, &mut cache).is_none());
        assert!(cache.is_empty());
    }
}
