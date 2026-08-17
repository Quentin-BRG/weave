// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Content-addressed blob store (specification section 16).
//!
//! Every byte sequence Weave must be able to reproduce - canonical state,
//! outbox candidates, conflict candidates, historical revisions - lives here,
//! keyed by SHA-256 over the exact bytes. Identical content is stored once.

use crate::error::{integrity, Result};
use crate::util::{sha256_hex, write_atomic};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Unit of streamed I/O. Also the transport's chunk payload, which is what
/// bounds how long a bulk transfer may delay a control message
/// ([docs/BLOB-PLANE.md](../docs/BLOB-PLANE.md)).
pub const CHUNK: usize = 256 * 1024;

/// Resumable partial transfers, one file per content hash.
const PARTIAL_DIR: &str = ".partial";

/// Anonymous temporaries: a write that is not worth resuming, because nobody
/// can name what it was going to contain.
const TEMP_PREFIX: &str = ".part-";

/// How long unreferenced content is kept before collection.
///
/// Long enough that nothing in flight is ever collected out from under itself:
/// a blob uploaded just before the operation naming it, a pack a participant
/// has yet to ask for, a partial whose peer is reconnecting. Space is not
/// scarce enough to be worth racing over.
pub const GC_GRACE_MS: u64 = 60 * 60 * 1000;

/// Headroom kept beyond what a transfer itself needs, so accepting one does not
/// leave the machine with nothing to write a database page into.
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// Partial files claimed by a live writer somewhere in this process.
///
/// Two writers must never append to the same partial: each hashes what it
/// believes it wrote, and interleaved bytes would make both wrong. The claim is
/// process-wide rather than per-[`BlobStore`] because a daemon opens the same
/// directory several times - host engine, replica engine, socket handlers - and
/// those handles are separate values over one directory.
fn claimed() -> &'static Mutex<HashSet<PathBuf>> {
    static CLAIMS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    CLAIMS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn hold(path: &Path) -> bool {
    let mut held = claimed().lock().unwrap_or_else(|e| e.into_inner());
    held.insert(path.to_path_buf())
}

fn is_claimed(path: &Path) -> bool {
    let held = claimed().lock().unwrap_or_else(|e| e.into_inner());
    held.contains(path)
}

/// Releases its path back to the claim set when the writer holding it goes.
struct PartialClaim(PathBuf);

impl Drop for PartialClaim {
    fn drop(&mut self) {
        let mut held = claimed().lock().unwrap_or_else(|e| e.into_inner());
        held.remove(&self.0);
    }
}

/// What one garbage-collection pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    pub blobs: u64,
    pub bytes: u64,
    pub partials: u64,
    pub temps: u64,
}

impl GcReport {
    pub fn is_empty(&self) -> bool {
        self.blobs == 0 && self.partials == 0 && self.temps == 0
    }
}

/// Content read from a file while it was being hashed.
///
/// `prefix` holds at most `TEXT_MERGE_LIMIT + 1` bytes, which is all
/// [`crate::model::FileKind`] classification can need: anything longer is
/// binary by definition, so no caller has a reason to hold the whole file.
#[derive(Debug, Clone)]
pub struct Ingested {
    pub hash: String,
    pub size: u64,
    pub prefix: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<BlobStore> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(BlobStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        self.root.join(&hash[..2]).join(hash)
    }

    /// Store bytes and return their hash. Durable on return: the blob is
    /// flushed and atomically installed before any revision may reference it
    /// (specification section 68).
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let hash = sha256_hex(bytes);
        let path = self.path_for(&hash);
        if path.exists() {
            return Ok(hash);
        }
        write_atomic(&path, bytes)?;
        Ok(hash)
    }

    pub fn has(&self, hash: &str) -> bool {
        if hash.len() < 2 {
            return false;
        }
        self.path_for(hash).exists()
    }

    /// Read a blob, verifying its content hash.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        if hash.len() != 64 {
            return Err(integrity(format!("Malformed blob reference: {hash}")));
        }
        let path = self.path_for(hash);
        let bytes = std::fs::read(&path).map_err(|e| {
            integrity(format!("IntegrityError: blob {hash} is missing")).with_detail(format!(
                "{}: {e}\n\nRun `weave recover` to diagnose the Weave blob store.",
                path.display()
            ))
        })?;
        let actual = sha256_hex(&bytes);
        if actual != hash {
            return Err(integrity(format!("IntegrityError: blob {hash} is corrupt"))
                .with_detail(format!("Stored bytes hash to {actual}.")));
        }
        Ok(bytes)
    }

    /// Read a blob without verifying, for bulk operations where the caller
    /// already verified integrity.
    pub fn get_unverified(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.path_for(hash))?)
    }

    // -----------------------------------------------------------------------
    // Streaming
    //
    // Nothing below ever holds a whole file in memory. These are the paths a
    // large file takes; the byte-slice methods above remain for content that is
    // small by construction - merge output, which is capped at
    // `TEXT_MERGE_LIMIT`, and control payloads.
    // -----------------------------------------------------------------------

    /// Location of a stored blob, for callers that hand a path to another
    /// process rather than reading the bytes themselves (`git hash-object`).
    pub fn path_of(&self, hash: &str) -> Result<PathBuf> {
        let path = self.path_for(hash);
        if !path.exists() {
            return Err(
                integrity(format!("IntegrityError: blob {hash} is missing")).with_detail(format!(
                    "{}\n\nRun `weave recover` to diagnose the Weave blob store.",
                    path.display()
                )),
            );
        }
        Ok(path)
    }

    pub fn size_of(&self, hash: &str) -> Result<u64> {
        Ok(std::fs::metadata(self.path_of(hash)?)?.len())
    }

    /// Refuse an incoming transfer that this disk plainly cannot hold.
    ///
    /// Filling a disk is not a corruption risk here — nothing partial is ever
    /// installed — but it is a slow, repeated, confusing failure, and it takes
    /// the working tree down with it. Better to decline the transfer with a
    /// sentence that says what is wrong. The margin covers the working copy
    /// this content is destined for on top of the blob itself.
    ///
    /// A platform that will not report free space is not an obstacle: unknown
    /// means proceed.
    pub fn ensure_room_for(&self, size: u64) -> Result<()> {
        let Some(free) = crate::util::available_space(&self.root) else {
            return Ok(());
        };
        let needed = size.saturating_mul(2).saturating_add(SPACE_MARGIN);
        if free >= needed {
            return Ok(());
        }
        Err(crate::error::persistence(format!(
            "Not enough disk space for a {} file: {} free.",
            crate::util::format_size(size),
            crate::util::format_size(free)
        ))
        .with_detail(
            "Weave keeps a working copy and a content-addressed copy of every file. Free some \
             space; the transfer is retried on its own.",
        ))
    }

    /// Start a streamed write. The blob is installed only by
    /// [`BlobWriter::finish`], so an abandoned or crashed write leaves nothing
    /// but a temporary file (specification section 68).
    pub fn writer(&self) -> Result<BlobWriter> {
        std::fs::create_dir_all(&self.root)?;
        let tmp = self
            .root
            .join(format!("{TEMP_PREFIX}{}", crate::util::random_hex(8)));
        Ok(BlobWriter {
            root: self.root.clone(),
            file: Some(File::create(&tmp)?),
            tmp,
            hasher: Sha256::new(),
            written: 0,
            claim: None,
        })
    }

    /// Start, or continue, a resumable write of the content `hash` names.
    ///
    /// Unlike [`BlobStore::writer`], the partial file is named after the
    /// content it will eventually hold and survives being dropped, so a
    /// transfer cut short by a disconnection continues where it stopped.
    /// [`BlobWriter::written`] is then the offset to ask the sender to resume
    /// from.
    ///
    /// Nothing on disk is taken on trust: the bytes already held are re-read to
    /// seed the hasher, so the final hash still covers every byte that will be
    /// installed, whether this process wrote it or a previous one did. A prefix
    /// that cannot be read - a crash mid-write, a truncated file - is thrown
    /// away and the transfer starts again from zero.
    ///
    /// `Ok(None)` when another writer in this process already holds that
    /// partial. The caller falls back to an anonymous [`BlobStore::writer`],
    /// which is always safe, merely not resumable.
    pub fn resume_writer(&self, hash: &str) -> Result<Option<BlobWriter>> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(integrity(format!("Malformed blob reference: {hash}")));
        }
        let dir = self.root.join(PARTIAL_DIR);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(hash);
        if !hold(&tmp) {
            return Ok(None);
        }
        let claim = PartialClaim(tmp.clone());
        let (hasher, written) = match hash_prefix(&tmp) {
            Ok(state) => state,
            Err(_) => {
                let _ = std::fs::remove_file(&tmp);
                (Sha256::new(), 0)
            }
        };
        // Append, so a stray write can never land before what has been hashed.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp)?;
        Ok(Some(BlobWriter {
            root: self.root.clone(),
            tmp,
            file: Some(file),
            hasher,
            written,
            claim: Some(claim),
        }))
    }

    /// How many bytes of `hash` are already held as a resumable partial.
    ///
    /// Diagnostic only: the authority on the resume offset is the writer, which
    /// has re-hashed what it holds.
    pub fn partial_size(&self, hash: &str) -> u64 {
        std::fs::metadata(self.root.join(PARTIAL_DIR).join(hash))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Delete stored content that nothing can reach any more.
    ///
    /// `live` is every hash the caller can still name - canonical state, local
    /// candidates, conflicts, transfers in flight. Everything else in the store
    /// is unreachable by definition, since a blob is only ever found by its
    /// hash. `min_age_ms` protects content that is young enough that the
    /// reference to it may not exist yet: a blob is durable before the
    /// operation naming it is sent, which is the whole point of the ordering,
    /// but that leaves a window in which it is legitimately unreferenced.
    ///
    /// Deletion failures are not errors. A blob another handle has open is
    /// simply collected on the next pass.
    pub fn collect_garbage(&self, live: &HashSet<String>, min_age_ms: u64) -> Result<GcReport> {
        let mut report = GcReport::default();
        if !self.root.exists() {
            return Ok(report);
        }
        let now = SystemTime::now();
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?;
            let name = shard.file_name().to_string_lossy().into_owned();
            let meta = shard.metadata()?;
            if !meta.is_dir() {
                if name.starts_with(TEMP_PREFIX)
                    && stale(&meta, now, min_age_ms)
                    && std::fs::remove_file(shard.path()).is_ok()
                {
                    report.temps += 1;
                }
                continue;
            }
            if name == PARTIAL_DIR {
                report.partials += self.sweep_partials(&shard.path(), now, min_age_ms)?;
                continue;
            }
            if name.len() != 2 {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                let hash = entry.file_name().to_string_lossy().into_owned();
                if hash.len() != 64 || live.contains(&hash) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if !meta.is_file() || !stale(&meta, now, min_age_ms) {
                    continue;
                }
                if std::fs::remove_file(entry.path()).is_ok() {
                    report.blobs += 1;
                    report.bytes += meta.len();
                }
            }
        }
        Ok(report)
    }

    /// A partial is worth keeping only while someone may still resume it: it is
    /// claimed by a live writer, or it is recent enough that the peer holding
    /// the rest may still come back. One already installed is redundant
    /// whatever its age.
    fn sweep_partials(&self, dir: &Path, now: SystemTime, min_age_ms: u64) -> Result<u64> {
        let mut removed = 0;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if is_claimed(&path) {
                continue;
            }
            let hash = entry.file_name().to_string_lossy().into_owned();
            // Deliberately not the directory entry's own metadata: on Windows
            // that is a snapshot taken when the directory was enumerated and is
            // not refreshed while a file is open, which would make a partial
            // somebody is actively writing look untouched since it was created.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let redundant = hash.len() == 64 && self.has(&hash);
            if !redundant && !stale(&meta, now, min_age_ms) {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Store the contents of `src`, returning what was stored, or `Ok(None)` if
    /// the file no longer exists.
    ///
    /// Two passes: hash the file without allocating it, and copy it in only if
    /// that content is not already stored. Rescans re-read but never rewrite,
    /// and a repeated content - a revert, or a file another participant already
    /// sent - costs one read. Whichever pass produces the answer, `hash`, `size`
    /// and `prefix` all come from that same single read of the file, so they
    /// cannot disagree if the file changes underneath us; the watcher will
    /// simply capture the newer state afterwards.
    pub fn ingest_file(&self, src: &Path, prefix_limit: usize) -> Result<Option<Ingested>> {
        let Some(probe) = stream_file(src, prefix_limit, None)? else {
            return Ok(None);
        };
        if self.has(&probe.hash) {
            return Ok(Some(probe));
        }
        let mut writer = self.writer()?;
        let Some(ingested) = stream_file(src, prefix_limit, Some(&mut writer))? else {
            return Ok(None);
        };
        writer.finish()?;
        Ok(Some(ingested))
    }

    /// Write a blob to `dest` through a temporary sibling, verifying the
    /// content hash as it streams. A mismatch leaves `dest` untouched.
    pub fn copy_out(&self, hash: &str, dest: &Path) -> Result<()> {
        let src = self.path_of(hash)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = crate::util::temp_sibling(dest);
        let mut hasher = Sha256::new();
        let result = (|| -> Result<()> {
            let mut input = File::open(&src)?;
            let mut output = File::create(&tmp)?;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                output.write_all(&buf[..n])?;
            }
            output.flush()?;
            output.sync_all()?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let actual = crate::util::hex(&hasher.finalize());
        if actual != hash {
            let _ = std::fs::remove_file(&tmp);
            return Err(integrity(format!("IntegrityError: blob {hash} is corrupt"))
                .with_detail(format!("Stored bytes hash to {actual}.")));
        }
        crate::util::install_atomic(&tmp, dest)
    }

    /// Total number of stored blobs and total bytes, for diagnostics.
    pub fn stats(&self) -> Result<(u64, u64)> {
        let mut count = 0u64;
        let mut bytes = 0u64;
        if !self.root.exists() {
            return Ok((0, 0));
        }
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?;
            // Two hex characters and nothing else: `.partial` holds transfers
            // that are not blobs yet and must not be counted as if they were.
            if !shard.file_type()?.is_dir() || shard.file_name().len() != 2 {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        count += 1;
                        bytes += meta.len();
                    }
                }
            }
        }
        Ok((count, bytes))
    }
}

/// A blob being written incrementally.
///
/// The content hash is computed as bytes arrive, so the writer never needs the
/// whole blob and the caller never has to trust the sender: installation is
/// refused unless the bytes hash to what was announced.
pub struct BlobWriter {
    root: PathBuf,
    tmp: PathBuf,
    file: Option<File>,
    hasher: Sha256,
    written: u64,
    /// Present when the partial is named after its content and may be resumed;
    /// it is what keeps a second writer off the same file.
    claim: Option<PartialClaim>,
}

impl BlobWriter {
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| integrity("Blob writer used after it was finished."))?;
        file.write_all(bytes)?;
        self.hasher.update(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// True when this write survives being dropped and can be continued later.
    pub fn resumable(&self) -> bool {
        self.claim.is_some()
    }

    /// Throw away everything held so far and start again from offset zero.
    ///
    /// For when a resumed partial cannot be a prefix of what is now being
    /// offered - it is longer than the announced content - so continuing from
    /// it could only ever produce a hash mismatch.
    pub fn reset(&mut self) -> Result<()> {
        self.file.take();
        self.file = Some(File::create(&self.tmp)?);
        self.hasher = Sha256::new();
        self.written = 0;
        Ok(())
    }

    /// Flush, then atomically install under the hash of what was actually
    /// written.
    pub fn finish(mut self) -> Result<(String, u64)> {
        let hash = self.seal()?;
        let written = self.written;
        self.install(&hash)?;
        Ok((hash, written))
    }

    /// Same, but refuse to install unless the content hashes to `expected`.
    ///
    /// This is the integrity backstop for anything received from the network: a
    /// truncated, corrupt or hostile transfer leaves the store untouched. The
    /// partial goes with it, resumable or not: bytes that hash to the wrong
    /// thing are not a prefix worth continuing from, and keeping them would
    /// make every later attempt fail the same way.
    pub fn finish_expecting(mut self, expected: &str) -> Result<u64> {
        let hash = self.seal()?;
        if hash != expected {
            self.discard();
            return Err(integrity(format!(
                "IntegrityError: content announced as {expected} hashes to {hash}"
            )));
        }
        let written = self.written;
        self.install(&hash)?;
        Ok(written)
    }

    fn seal(&mut self) -> Result<String> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
        }
        Ok(crate::util::hex(&self.hasher.clone().finalize()))
    }

    fn install(&mut self, hash: &str) -> Result<()> {
        let dest = self.root.join(&hash[..2]).join(hash);
        if dest.exists() {
            // Identical content is stored once; the work was redundant, not
            // wrong.
            self.discard();
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::util::install_atomic(&self.tmp, &dest)
    }

    fn discard(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.tmp);
    }
}

impl Drop for BlobWriter {
    /// An abandoned write - a dropped connection, a panic - must not leave a
    /// temporary file behind. It never leaves a *blob* behind: installation is
    /// the only path into the store.
    ///
    /// A resumable partial is the deliberate exception. It is named after the
    /// content it holds, so the next attempt can continue from it instead of
    /// paying for the same bytes twice; it is still not a blob, and
    /// [`BlobStore::collect_garbage`] removes it if nobody comes back. What is
    /// flushed here is a courtesy - the prefix is re-hashed before it is
    /// resumed onto, so losing the tail of it costs a restart, never a wrong
    /// install.
    fn drop(&mut self) {
        if self.claim.is_some() {
            if let Some(mut file) = self.file.take() {
                let _ = file.flush();
                let _ = file.sync_all();
            }
            return;
        }
        if self.file.is_some() {
            self.discard();
        }
    }
}

/// Re-read a partial file, returning the hasher state and the byte count it
/// covers. A file that is not there is an empty prefix, not a failure.
fn hash_prefix(path: &Path) -> Result<(Sha256, u64)> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Sha256::new(), 0)),
        Err(e) => return Err(e.into()),
    };
    let mut hasher = Sha256::new();
    let mut read = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read += n as u64;
    }
    Ok((hasher, read))
}

/// Old enough to collect. Something modified in the future - a clock that went
/// backwards, a copied tree - is treated as young, which only delays it.
fn stale(meta: &Metadata, now: SystemTime, min_age_ms: u64) -> bool {
    if min_age_ms == 0 {
        return true;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    now.duration_since(modified)
        .map(|age| age.as_millis() as u64 >= min_age_ms)
        .unwrap_or(false)
}

/// Read `src` once, hashing it, optionally copying it into `sink`, and keeping
/// the first `prefix_limit` bytes.
///
/// `Ok(None)` when the file does not exist: a path can vanish between being
/// listed and being read, and that is ordinary rather than exceptional.
fn stream_file(
    src: &Path,
    prefix_limit: usize,
    mut sink: Option<&mut BlobWriter>,
) -> Result<Option<Ingested>> {
    let mut file = match File::open(src) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut prefix = Vec::new();
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let bytes = &buf[..n];
        hasher.update(bytes);
        size += n as u64;
        if prefix.len() < prefix_limit {
            let take = (prefix_limit - prefix.len()).min(n);
            prefix.extend_from_slice(&bytes[..take]);
        }
        if let Some(writer) = sink.as_deref_mut() {
            writer.write(bytes)?;
        }
    }
    Ok(Some(Ingested {
        hash: crate::util::hex(&hasher.finalize()),
        size,
        prefix,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicates_and_verifies() {
        let dir = std::env::temp_dir().join(format!("weave-blobs-{}", crate::util::random_hex(6)));
        let store = BlobStore::open(&dir).unwrap();
        let h1 = store.put(b"hello").unwrap();
        let h2 = store.put(b"hello").unwrap();
        assert_eq!(h1, h2);
        assert_eq!(store.get(&h1).unwrap(), b"hello");
        assert!(store.has(&h1));
        assert_eq!(store.stats().unwrap().0, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("weave-blobs-{}", crate::util::random_hex(6)));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn store(&self) -> BlobStore {
            BlobStore::open(self.0.join("blobs")).unwrap()
        }
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// The streamed path and the in-memory path must agree, or a file's
    /// identity would depend on how it happened to be read.
    #[test]
    fn streaming_and_in_memory_agree_across_the_chunk_boundary() {
        let scratch = Scratch::new();
        let store = scratch.store();
        for size in [0usize, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17] {
            let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let src = scratch.file("input.bin", &bytes);
            let ingested = store.ingest_file(&src, 8).unwrap().expect("file exists");
            assert_eq!(
                ingested.hash,
                crate::util::sha256_hex(&bytes),
                "size {size}"
            );
            assert_eq!(ingested.size, size as u64, "size {size}");
            assert_eq!(ingested.prefix, bytes[..bytes.len().min(8)], "size {size}");
            assert!(store.has(&ingested.hash));
            assert_eq!(store.get(&ingested.hash).unwrap(), bytes, "size {size}");
        }
    }

    #[test]
    fn ingesting_the_same_content_twice_stores_it_once() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes = vec![9u8; CHUNK * 2];
        let a = scratch.file("a.bin", &bytes);
        let b = scratch.file("b.bin", &bytes);
        let first = store.ingest_file(&a, 0).unwrap().expect("file exists");
        let second = store.ingest_file(&b, 0).unwrap().expect("file exists");
        assert_eq!(first.hash, second.hash);
        assert_eq!(store.stats().unwrap().0, 1);
    }

    #[test]
    fn copy_out_reproduces_the_exact_bytes() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes: Vec<u8> = (0..CHUNK * 2 + 5).map(|i| (i % 256) as u8).collect();
        let hash = store.put(&bytes).unwrap();
        let dest = scratch.0.join("nested").join("out.bin");
        store.copy_out(&hash, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    }

    /// The integrity backstop for anything arriving from the network.
    #[test]
    fn a_mismatched_announcement_installs_nothing() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let lie = crate::util::sha256_hex(b"something else");
        let mut writer = store.writer().unwrap();
        writer.write(b"actual content").unwrap();
        let err = writer.finish_expecting(&lie).unwrap_err();
        assert_eq!(err.class, crate::error::ErrorClass::IntegrityError);
        assert!(!store.has(&lie));
        assert_eq!(store.stats().unwrap().0, 0);
        assert_eq!(leftover_parts(&store), 0);
    }

    /// A transfer that dies mid-flight - dropped connection, panic, crash -
    /// must leave neither a blob nor a temporary file.
    #[test]
    fn an_abandoned_write_leaves_nothing_behind() {
        let scratch = Scratch::new();
        let store = scratch.store();
        {
            let mut writer = store.writer().unwrap();
            writer.write(&vec![1u8; CHUNK + 3]).unwrap();
            assert_eq!(writer.written(), (CHUNK + 3) as u64);
        }
        assert_eq!(store.stats().unwrap().0, 0);
        assert_eq!(leftover_parts(&store), 0);
    }

    fn leftover_parts(store: &BlobStore) -> usize {
        std::fs::read_dir(store.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .count()
    }

    // ----------------------------------------------------------- resumption

    /// The point of the whole mechanism: a transfer that dies halfway is
    /// continued, not repaid.
    #[test]
    fn a_dropped_resumable_write_is_continued_from_where_it_stopped() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes: Vec<u8> = (0..CHUNK * 3 + 11).map(|i| (i % 253) as u8).collect();
        let hash = crate::util::sha256_hex(&bytes);
        let cut = CHUNK + 700;

        {
            let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
            assert_eq!(writer.written(), 0);
            assert!(writer.resumable());
            writer.write(&bytes[..cut]).unwrap();
        }
        assert!(!store.has(&hash), "a partial must never install");

        let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
        assert_eq!(writer.written(), cut as u64, "resumed offset");
        writer.write(&bytes[cut..]).unwrap();
        assert_eq!(writer.finish_expecting(&hash).unwrap(), bytes.len() as u64);
        assert_eq!(store.get(&hash).unwrap(), bytes);
    }

    /// The hasher must be seeded from the bytes on disk, not from the bytes
    /// this process happened to write, or a resumed transfer would install
    /// content whose hash covers only its tail.
    #[test]
    fn a_resumed_write_hashes_the_prefix_it_inherited() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes = b"the quick brown fox jumps over the lazy dog".to_vec();
        let hash = crate::util::sha256_hex(&bytes);
        std::fs::create_dir_all(scratch.0.join("blobs").join(PARTIAL_DIR)).unwrap();
        // A prefix written by a previous run of the daemon.
        std::fs::write(
            scratch.0.join("blobs").join(PARTIAL_DIR).join(&hash),
            &bytes[..10],
        )
        .unwrap();

        let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
        assert_eq!(writer.written(), 10);
        writer.write(&bytes[10..]).unwrap();
        writer.finish_expecting(&hash).unwrap();
        assert_eq!(store.get(&hash).unwrap(), bytes);
    }

    /// A prefix that is not a prefix - a crash left garbage, or a peer lied -
    /// must not be resumed onto forever. The mismatch takes the partial with
    /// it, so the next attempt starts clean.
    #[test]
    fn a_corrupt_partial_is_discarded_rather_than_resumed_onto() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes = vec![7u8; 4096];
        let hash = crate::util::sha256_hex(&bytes);
        let partial = scratch.0.join("blobs").join(PARTIAL_DIR).join(&hash);
        std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
        std::fs::write(&partial, vec![0u8; 100]).unwrap();

        let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
        assert_eq!(writer.written(), 100);
        writer.write(&bytes[100..]).unwrap();
        let err = writer.finish_expecting(&hash).unwrap_err();
        assert_eq!(err.class, crate::error::ErrorClass::IntegrityError);
        assert!(!store.has(&hash));
        assert!(!partial.exists(), "a corrupt prefix must not survive");

        // And the retry, starting from zero, succeeds.
        let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
        assert_eq!(writer.written(), 0);
        writer.write(&bytes).unwrap();
        writer.finish_expecting(&hash).unwrap();
        assert!(store.has(&hash));
    }

    /// Held bytes longer than the content now being offered cannot be a prefix
    /// of it.
    #[test]
    fn a_partial_can_be_reset_to_zero() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes = b"short".to_vec();
        let hash = crate::util::sha256_hex(&bytes);
        {
            let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
            writer.write(b"a much longer sequence of bytes").unwrap();
        }
        let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
        assert_eq!(writer.written(), 31);
        writer.reset().unwrap();
        assert_eq!(writer.written(), 0);
        writer.write(&bytes).unwrap();
        writer.finish_expecting(&hash).unwrap();
        assert_eq!(store.get(&hash).unwrap(), bytes);
    }

    /// Two receivers asking for the same content at once must not append into
    /// one file. The second is told to use an anonymous writer instead.
    #[test]
    fn a_partial_is_claimed_by_at_most_one_writer() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let hash = crate::util::sha256_hex(b"contended");
        let first = store.resume_writer(&hash).unwrap();
        assert!(first.is_some());
        assert!(store.resume_writer(&hash).unwrap().is_none());
        // A second handle on the same directory is still the same process.
        let other = BlobStore::open(scratch.0.join("blobs")).unwrap();
        assert!(other.resume_writer(&hash).unwrap().is_none());
        drop(first);
        assert!(store.resume_writer(&hash).unwrap().is_some());
    }

    #[test]
    fn a_malformed_hash_is_refused_a_partial() {
        let scratch = Scratch::new();
        let store = scratch.store();
        assert!(store.resume_writer("../escape").is_err());
        assert!(store.resume_writer(&"z".repeat(64)).is_err());
    }

    // ------------------------------------------------------------------- gc

    #[test]
    fn collection_keeps_referenced_content_and_removes_the_rest() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let live_hash = store.put(b"still referenced").unwrap();
        let dead_hash = store.put(b"nothing points here").unwrap();
        let live: HashSet<String> = [live_hash.clone()].into_iter().collect();

        // Nothing is old enough yet.
        let report = store.collect_garbage(&live, GC_GRACE_MS).unwrap();
        assert!(report.is_empty());
        assert!(store.has(&dead_hash));

        let report = store.collect_garbage(&live, 0).unwrap();
        assert_eq!(report.blobs, 1);
        assert_eq!(report.bytes, b"nothing points here".len() as u64);
        assert!(store.has(&live_hash), "referenced content must survive");
        assert!(!store.has(&dead_hash));
    }

    /// The collector must not pull the floor out from under a transfer in
    /// progress, however aggressive its age threshold.
    #[test]
    fn collection_leaves_a_claimed_partial_alone() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let hash = crate::util::sha256_hex(b"in flight");
        let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
        writer.write(b"in fl").unwrap();

        let report = store.collect_garbage(&HashSet::new(), 0).unwrap();
        assert_eq!(report.partials, 0);
        assert_eq!(store.partial_size(&hash), 5);

        // Once the writer is gone the partial is collectable like anything
        // else.
        drop(writer);
        let report = store.collect_garbage(&HashSet::new(), 0).unwrap();
        assert_eq!(report.partials, 1);
        assert_eq!(store.partial_size(&hash), 0);
    }

    /// Anonymous temporaries have no name anyone could resume from, so a
    /// crashed daemon's leftovers are pure waste.
    #[test]
    fn collection_removes_abandoned_temporaries() {
        let scratch = Scratch::new();
        let store = scratch.store();
        std::fs::write(store.root().join(format!("{TEMP_PREFIX}deadbeef")), b"x").unwrap();
        assert_eq!(leftover_parts(&store), 1);
        let report = store.collect_garbage(&HashSet::new(), 0).unwrap();
        assert_eq!(report.temps, 1);
        assert_eq!(leftover_parts(&store), 0);
    }

    /// A partial for content that arrived by another route is redundant at any
    /// age.
    #[test]
    fn collection_removes_a_partial_whose_blob_is_already_installed() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes = b"arrived twice".to_vec();
        let hash = store.put(&bytes).unwrap();
        {
            let mut writer = store.resume_writer(&hash).unwrap().expect("unclaimed");
            writer.write(&bytes[..4]).unwrap();
        }
        let live: HashSet<String> = [hash.clone()].into_iter().collect();
        let report = store.collect_garbage(&live, GC_GRACE_MS).unwrap();
        assert_eq!(report.partials, 1);
        assert!(store.has(&hash), "the blob itself is referenced");
    }
}
