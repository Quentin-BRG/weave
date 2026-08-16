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
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Unit of streamed I/O. Also the transport's chunk payload, which is what
/// bounds how long a bulk transfer may delay a control message
/// ([docs/BLOB-PLANE.md](../docs/BLOB-PLANE.md)).
pub const CHUNK: usize = 256 * 1024;

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

    /// Start a streamed write. The blob is installed only by
    /// [`BlobWriter::finish`], so an abandoned or crashed write leaves nothing
    /// but a temporary file (specification section 68).
    pub fn writer(&self) -> Result<BlobWriter> {
        std::fs::create_dir_all(&self.root)?;
        let tmp = self
            .root
            .join(format!(".part-{}", crate::util::random_hex(8)));
        Ok(BlobWriter {
            root: self.root.clone(),
            file: Some(File::create(&tmp)?),
            tmp,
            hasher: Sha256::new(),
            written: 0,
        })
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
            if !shard.file_type()?.is_dir() {
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
    /// truncated, corrupt or hostile transfer leaves the store untouched.
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
    fn drop(&mut self) {
        if self.file.is_some() {
            self.discard();
        }
    }
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
            .filter(|e| e.file_name().to_string_lossy().starts_with(".part-"))
            .count()
    }
}
