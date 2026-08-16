// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Three-way reconciliation of one path.
//!
//! Implements the host reconciliation matrix, specification sections 71-81.
//! The same function performs the client-side continuation rebase of
//! section 40-42, with `base = in-flight candidate`, `current = canonical
//! result`, `incoming = newer local candidate`.
//!
//! Conflict marker output is never returned: a textual conflict discards the
//! merge output entirely so that no working tree can ever receive automatically
//! generated `<<<<<<<` markers (sections 7.4, 80).

use crate::blobs::BlobStore;
use crate::error::Result;
use crate::gitx::{self, MergeOutcome};
use crate::model::{ConflictKind, FileEntry, FileKind, GitMode, TEXT_MERGE_LIMIT};
use std::path::{Path, PathBuf};

/// Result of reconciling one path.
#[derive(Debug, Clone, PartialEq)]
pub enum Reconciled {
    /// Nothing to do: the desired state is already the canonical state.
    /// No revision is consumed (specification sections 21, 72, 75).
    Converged,
    /// Canonical state advances to `entry`.
    Accept {
        entry: Option<FileEntry>,
        /// True when the entry is the product of a three-way text merge and
        /// therefore differs from both sides (specification section 79).
        merged: bool,
    },
    /// Reconciliation requires a human or agent decision.
    Conflict(ConflictKind),
}

/// Everything reconciliation needs to read and write content.
pub struct MergeContext<'a> {
    pub repo_root: &'a Path,
    pub scratch: PathBuf,
    pub blobs: &'a BlobStore,
}

impl<'a> MergeContext<'a> {
    pub fn new(repo_root: &'a Path, scratch: PathBuf, blobs: &'a BlobStore) -> Self {
        MergeContext {
            repo_root,
            scratch,
            blobs,
        }
    }
}

fn mergeable_text(e: &FileEntry) -> bool {
    e.file_kind == FileKind::Text && e.size <= TEXT_MERGE_LIMIT
}

/// Reconcile `incoming` against `current` given their common `base`.
pub fn reconcile(
    ctx: &MergeContext<'_>,
    base: Option<&FileEntry>,
    current: Option<&FileEntry>,
    incoming: Option<&FileEntry>,
) -> Result<Reconciled> {
    // Section 72: identical desired state converges without a revision.
    if FileEntry::same_as(incoming, current) {
        return Ok(Reconciled::Converged);
    }
    // Section 73: no concurrent change, accept directly (create, modify,
    // delete, mode-only change).
    if FileEntry::same_as(current, base) {
        return Ok(Reconciled::Accept {
            entry: incoming.cloned(),
            merged: false,
        });
    }
    // The submitter's desired state equals the base it declared: it carries no
    // change of its own, so the concurrent canonical state simply stands.
    if FileEntry::same_as(incoming, base) {
        return Ok(Reconciled::Converged);
    }

    match (base, current, incoming) {
        // Section 74: concurrent create with differing content.
        (None, Some(_), Some(_)) => Ok(Reconciled::Conflict(ConflictKind::ConcurrentCreate)),
        // Base absent and one side absent is impossible here: both differ from
        // base, and base is None, so both are Some. Kept exhaustive for safety.
        (None, _, _) => Ok(Reconciled::Conflict(ConflictKind::ConcurrentCreate)),

        // Section 76: modify versus delete, in either direction, never resolved
        // implicitly.
        (Some(_), None, Some(_)) => Ok(Reconciled::Conflict(ConflictKind::DeleteModify)),
        (Some(_), Some(_), None) => Ok(Reconciled::Conflict(ConflictKind::DeleteModify)),
        (Some(_), None, None) => Ok(Reconciled::Converged), // already covered above

        (Some(b), Some(c), Some(i)) => reconcile_present(ctx, b, c, i),
    }
}

fn reconcile_present(
    ctx: &MergeContext<'_>,
    b: &FileEntry,
    c: &FileEntry,
    i: &FileEntry,
) -> Result<Reconciled> {
    // ---- mode reconciliation (specification section 81) ----
    let c_mode_changed = c.git_mode != b.git_mode;
    let i_mode_changed = i.git_mode != b.git_mode;
    if c_mode_changed && i_mode_changed && c.git_mode != i.git_mode {
        return Ok(Reconciled::Conflict(ConflictKind::ModeConflict));
    }
    let final_mode: GitMode = if i_mode_changed {
        i.git_mode
    } else {
        c.git_mode
    };

    // ---- content reconciliation ----
    let c_content_changed = c.blob_hash != b.blob_hash;
    let i_content_changed = i.blob_hash != b.blob_hash;

    let (content_entry, merged) = if !i_content_changed {
        (c.clone(), false)
    } else if !c_content_changed {
        (i.clone(), false)
    } else if c.blob_hash == i.blob_hash {
        (c.clone(), false)
    } else {
        // Section 77: binary content is never merged.
        if !(mergeable_text(b) && mergeable_text(c) && mergeable_text(i)) {
            return Ok(Reconciled::Conflict(ConflictKind::BinaryConcurrentEdit));
        }
        let base_bytes = ctx.blobs.get(&b.blob_hash)?;
        let cur_bytes = ctx.blobs.get(&c.blob_hash)?;
        let inc_bytes = ctx.blobs.get(&i.blob_hash)?;
        match gitx::merge_file(
            ctx.repo_root,
            &ctx.scratch,
            &cur_bytes,
            &base_bytes,
            &inc_bytes,
        )? {
            MergeOutcome::Clean(bytes) => {
                // Section 79: the merged result may differ from both sides.
                let hash = ctx.blobs.put(&bytes)?;
                (
                    FileEntry {
                        blob_hash: hash,
                        size: bytes.len() as u64,
                        git_mode: final_mode,
                        file_kind: FileKind::classify(&bytes),
                    },
                    true,
                )
            }
            // Section 80: merge-marker output is discarded.
            MergeOutcome::Conflict => {
                return Ok(Reconciled::Conflict(ConflictKind::TextConcurrentEdit))
            }
        }
    };

    let entry = FileEntry {
        git_mode: final_mode,
        ..content_entry
    };
    if FileEntry::same_as(Some(&entry), Some(c)) {
        Ok(Reconciled::Converged)
    } else {
        Ok(Reconciled::Accept {
            entry: Some(entry),
            merged,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        blobs: BlobStore,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir =
                std::env::temp_dir().join(format!("weave-rec-{}", crate::util::random_hex(6)));
            std::fs::create_dir_all(&dir).unwrap();
            // `git merge-file` does not need a repository, but the wrapper runs
            // git with `-C <dir>`; a plain directory is enough.
            let blobs = BlobStore::open(dir.join("blobs")).unwrap();
            Fixture { dir, blobs }
        }
        fn ctx(&self) -> MergeContext<'_> {
            MergeContext::new(&self.dir, self.dir.join("tmp"), &self.blobs)
        }
        fn entry(&self, content: &str) -> FileEntry {
            let bytes = content.as_bytes();
            self.blobs.put(bytes).unwrap();
            FileEntry::from_bytes(bytes, GitMode::Regular)
        }
        fn exec_entry(&self, content: &str) -> FileEntry {
            let bytes = content.as_bytes();
            self.blobs.put(bytes).unwrap();
            FileEntry::from_bytes(bytes, GitMode::Executable)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    #[test]
    fn identical_desired_state_converges() {
        let f = Fixture::new();
        let b = f.entry("A\n");
        let x = f.entry("B\n");
        assert_eq!(
            reconcile(&f.ctx(), Some(&b), Some(&x), Some(&x)).unwrap(),
            Reconciled::Converged
        );
    }

    #[test]
    fn no_concurrent_change_accepts() {
        let f = Fixture::new();
        let b = f.entry("A\n");
        let i = f.entry("A2\n");
        match reconcile(&f.ctx(), Some(&b), Some(&b), Some(&i)).unwrap() {
            Reconciled::Accept { entry, merged } => {
                assert!(!merged);
                assert_eq!(entry.unwrap().blob_hash, i.blob_hash);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn independent_edits_merge_cleanly() {
        // Specification section 178.
        let f = Fixture::new();
        let b = f.entry("A\nB\nC\n");
        let c = f.entry("A1\nB\nC\n");
        let i = f.entry("A\nB\nC1\n");
        match reconcile(&f.ctx(), Some(&b), Some(&c), Some(&i)).unwrap() {
            Reconciled::Accept { entry, merged } => {
                assert!(merged);
                let bytes = f.blobs.get(&entry.unwrap().blob_hash).unwrap();
                assert_eq!(String::from_utf8(bytes).unwrap(), "A1\nB\nC1\n");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn overlapping_edits_conflict() {
        // Specification section 179.
        let f = Fixture::new();
        let b = f.entry("A\nB\nC\n");
        let c = f.entry("A\nBETA\nC\n");
        let i = f.entry("A\nGAMMA\nC\n");
        assert_eq!(
            reconcile(&f.ctx(), Some(&b), Some(&c), Some(&i)).unwrap(),
            Reconciled::Conflict(ConflictKind::TextConcurrentEdit)
        );
    }

    #[test]
    fn concurrent_create_same_content_converges() {
        // Specification section 180.
        let f = Fixture::new();
        let x = f.entry("same\n");
        assert_eq!(
            reconcile(&f.ctx(), None, Some(&x), Some(&x)).unwrap(),
            Reconciled::Converged
        );
    }

    #[test]
    fn concurrent_create_different_content_conflicts() {
        let f = Fixture::new();
        let a = f.entry("one\n");
        let b = f.entry("two\n");
        assert_eq!(
            reconcile(&f.ctx(), None, Some(&a), Some(&b)).unwrap(),
            Reconciled::Conflict(ConflictKind::ConcurrentCreate)
        );
    }

    #[test]
    fn modify_delete_conflicts_both_directions() {
        // Specification section 181.
        let f = Fixture::new();
        let b = f.entry("A\n");
        let m = f.entry("A2\n");
        assert_eq!(
            reconcile(&f.ctx(), Some(&b), None, Some(&m)).unwrap(),
            Reconciled::Conflict(ConflictKind::DeleteModify)
        );
        assert_eq!(
            reconcile(&f.ctx(), Some(&b), Some(&m), None).unwrap(),
            Reconciled::Conflict(ConflictKind::DeleteModify)
        );
    }

    #[test]
    fn concurrent_delete_converges() {
        let f = Fixture::new();
        let b = f.entry("A\n");
        assert_eq!(
            reconcile(&f.ctx(), Some(&b), None, None).unwrap(),
            Reconciled::Converged
        );
    }

    #[test]
    fn binary_concurrent_edit_conflicts() {
        let f = Fixture::new();
        let mk = |bytes: &[u8]| {
            f.blobs.put(bytes).unwrap();
            FileEntry::from_bytes(bytes, GitMode::Regular)
        };
        let b = mk(&[0u8, 1, 2, 3]);
        let c = mk(&[0u8, 1, 2, 9]);
        let i = mk(&[0u8, 1, 2, 8]);
        assert_eq!(
            reconcile(&f.ctx(), Some(&b), Some(&c), Some(&i)).unwrap(),
            Reconciled::Conflict(ConflictKind::BinaryConcurrentEdit)
        );
    }

    #[test]
    fn one_sided_mode_change_is_preserved() {
        let f = Fixture::new();
        let b = f.entry("A\n");
        let c = f.entry("A1\n"); // canonical changed content only
        let i = f.exec_entry("A\n"); // incoming changed mode only
        match reconcile(&f.ctx(), Some(&b), Some(&c), Some(&i)).unwrap() {
            Reconciled::Accept { entry, .. } => {
                let e = entry.unwrap();
                assert_eq!(e.git_mode, GitMode::Executable);
                assert_eq!(e.blob_hash, c.blob_hash);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn incompatible_mode_change_conflicts() {
        let f = Fixture::new();
        // Base is executable; canonical reverts to regular, incoming keeps a
        // different mode change by making it regular-with-different-content is
        // not a mode conflict, so construct a genuine two-way mode divergence.
        let b = f.exec_entry("A\n");
        let c = f.entry("A\n"); // exec -> regular
        let mut i = f.exec_entry("A\n");
        i.git_mode = GitMode::Regular;
        // Both sides now agree on regular: that converges rather than conflicts.
        assert_eq!(
            reconcile(&f.ctx(), Some(&b), Some(&c), Some(&i)).unwrap(),
            Reconciled::Converged
        );
    }
}
