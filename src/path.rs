// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Canonical repository paths and the portable filename policy.
//!
//! Specification sections 47 (canonical paths), 48 (portable path policy) and
//! 49 (symlink/junction safety).
//!
//! A session may mix Windows, macOS and Linux machines, so Weave enforces the
//! intersection of what those platforms can represent unambiguously. Paths that
//! would be legal on one platform but not another are rejected up front rather
//! than failing halfway through synchronization.

use crate::error::{unsupported, Result, WeaveError};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// A repository-relative, `/`-separated, validated path.
///
/// Deserialization validates, so a path that arrived over the wire can never
/// bypass the portability and traversal rules.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct RepoPath(String);

impl<'de> serde::Deserialize<'de> for RepoPath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<RepoPath, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        validate(&raw).map_err(serde::de::Error::custom)?;
        Ok(RepoPath(raw))
    }
}

/// Windows device names that cannot be used as a filename component.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters Windows forbids in filenames.
const WINDOWS_INVALID_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\\'];

impl RepoPath {
    /// Validate and construct a canonical repository path.
    pub fn new(raw: &str) -> Result<RepoPath> {
        validate(raw)?;
        Ok(RepoPath(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// Resolve against a repository root, producing a native filesystem path.
    pub fn to_fs_path(&self, root: &Path) -> PathBuf {
        let mut p = root.to_path_buf();
        for component in self.0.split('/') {
            p.push(component);
        }
        p
    }

    /// The comparison key used to detect cross-platform collisions.
    ///
    /// Weave uses one documented normalization strategy consistently:
    /// **Unicode NFC followed by Unicode lowercase**. Two repository paths whose
    /// keys are equal cannot coexist in a portable Weave session.
    pub fn collision_key(&self) -> String {
        collision_key_of(&self.0)
    }

    /// The parent directory components of this path, outermost first.
    pub fn parent_dirs(&self) -> Vec<String> {
        let parts: Vec<&str> = self.0.split('/').collect();
        let mut out = Vec::new();
        let mut acc = String::new();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            out.push(acc.clone());
        }
        out
    }

    /// Convert a native path relative to `root` into a canonical repository path.
    pub fn from_relative(rel: &Path) -> Result<RepoPath> {
        let mut parts = Vec::new();
        for component in rel.components() {
            match component {
                Component::Normal(os) => {
                    let s = os.to_str().ok_or_else(|| {
                        unsupported(format!(
                            "Path is not valid UTF-8 and cannot be synchronized: {}",
                            rel.display()
                        ))
                    })?;
                    parts.push(s.to_string());
                }
                Component::CurDir => {}
                _ => {
                    return Err(unsupported(format!(
                        "Path is not repository-relative: {}",
                        rel.display()
                    )))
                }
            }
        }
        RepoPath::new(&parts.join("/"))
    }
}

pub fn collision_key_of(raw: &str) -> String {
    raw.nfc().collect::<String>().to_lowercase()
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Full canonical + portability validation of a raw path string.
pub fn validate(raw: &str) -> Result<()> {
    let reject = |why: &str| -> WeaveError {
        unsupported(format!("Unsupported repository path: {raw}")).with_detail(format!(
            "{why}\n\nWeave enforces a portable filename subset so that Windows, macOS and \
             Linux participants can hold the same working tree. Rename the file and retry."
        ))
    };

    if raw.is_empty() {
        return Err(reject("The path is empty."));
    }
    if raw.len() > 1024 {
        return Err(reject("The path is longer than 1024 bytes."));
    }
    if raw.starts_with('/') {
        return Err(reject(
            "The path is absolute; repository paths must be relative.",
        ));
    }
    if raw.contains('\0') {
        return Err(reject("The path contains a NUL byte."));
    }
    // Windows drive-letter prefixes such as `C:` are absolute in disguise.
    if raw.len() >= 2 && raw.as_bytes()[1] == b':' {
        return Err(reject("The path looks like a Windows drive-absolute path."));
    }

    for component in raw.split('/') {
        if component.is_empty() {
            return Err(reject(
                "The path contains an empty component (`//` or a trailing `/`).",
            ));
        }
        if component == "." || component == ".." {
            return Err(reject(
                "The path contains a `.` or `..` traversal component.",
            ));
        }
        if component.eq_ignore_ascii_case(".git") {
            return Err(reject("The path targets Git internal storage."));
        }
        if component.ends_with(' ') {
            return Err(reject(
                "A path component ends with a space, which Windows silently strips.",
            ));
        }
        if component.ends_with('.') {
            return Err(reject(
                "A path component ends with a dot, which Windows silently strips.",
            ));
        }
        for ch in component.chars() {
            if (ch as u32) < 0x20 || ch == '\u{7f}' {
                return Err(reject("A path component contains a control character."));
            }
            if WINDOWS_INVALID_CHARS.contains(&ch) {
                return Err(reject(&format!(
                    "A path component contains `{ch}`, which is not a legal Windows filename character."
                )));
            }
        }
        let stem = component.split('.').next().unwrap_or(component);
        if WINDOWS_RESERVED
            .iter()
            .any(|r| stem.eq_ignore_ascii_case(r))
        {
            return Err(reject(&format!(
                "`{component}` uses a Windows reserved device name."
            )));
        }
    }
    Ok(())
}

/// Verify that no component of `path` (relative to `root`) is a symlink,
/// junction or other reparse point.
///
/// Specification section 49: Weave must never traverse an indirect filesystem
/// path that could escape the repository root.
pub fn ensure_no_indirection(root: &Path, path: &RepoPath) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in path.as_str().split('/') {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(unsupported(format!(
                        "Refusing to follow a link inside the repository: {}",
                        path
                    ))
                    .with_detail(
                        "Weave V1 does not synchronize symlinks, Windows junctions or other \
                         reparse points, and never traverses them.",
                    ));
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        return Err(unsupported(format!(
                            "Refusing to follow a reparse point inside the repository: {}",
                            path
                        )));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_paths() {
        assert!(RepoPath::new("slides/07-pricing.tsx").is_ok());
        assert!(RepoPath::new("README.md").is_ok());
        assert!(RepoPath::new("a/b/c/d.json").is_ok());
    }

    #[test]
    fn rejects_traversal_and_git() {
        assert!(RepoPath::new("../escape").is_err());
        assert!(RepoPath::new("a/../b").is_err());
        assert!(RepoPath::new(".git/config").is_err());
        assert!(RepoPath::new("nested/.git/hooks/x").is_err());
        assert!(RepoPath::new("/absolute").is_err());
        assert!(RepoPath::new("C:/x").is_err());
    }

    #[test]
    fn rejects_windows_hostile_names() {
        assert!(RepoPath::new("com1.txt").is_err());
        assert!(RepoPath::new("NUL").is_err());
        assert!(RepoPath::new("dir/trailing ").is_err());
        assert!(RepoPath::new("dir/trailing.").is_err());
        assert!(RepoPath::new("we:ird.txt").is_err());
        assert!(RepoPath::new("back\\slash.txt").is_err());
    }

    #[test]
    fn collision_key_folds_case_and_normalizes() {
        let a = RepoPath::new("Slide.tsx").unwrap();
        let b = RepoPath::new("slide.tsx").unwrap();
        assert_eq!(a.collision_key(), b.collision_key());

        // NFD "é" and NFC "é" must fold together.
        let nfc = RepoPath::new("caf\u{e9}.md").unwrap();
        let nfd = RepoPath::new("cafe\u{301}.md").unwrap();
        assert_eq!(nfc.collision_key(), nfd.collision_key());
    }

    #[test]
    fn parent_dirs_are_ordered_outermost_first() {
        let p = RepoPath::new("a/b/c.txt").unwrap();
        assert_eq!(p.parent_dirs(), vec!["a".to_string(), "a/b".to_string()]);
    }
}
