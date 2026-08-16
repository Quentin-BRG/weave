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
use std::path::{Path, PathBuf};

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
}
