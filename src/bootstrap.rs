// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `weave agent bootstrap` (specification sections 161-163).
//!
//! Writes a clearly delimited managed block into the repository's root
//! `AGENTS.md`, leaving every unrelated instruction untouched. The block states
//! "check `weave status`", never "a session is active", so it can stay
//! committed permanently.

use crate::error::Result;
use std::path::Path;

pub const BEGIN: &str = "<!-- weave:begin -->";
pub const END: &str = "<!-- weave:end -->";

/// The body between the markers. Kept as a plain literal so the Markdown
/// indentation survives exactly as written.
const BODY: &str = r#"
## Weave collaboration

This repository supports Weave live collaboration.

Before substantial file modifications, run:

    weave status --json

If a Weave session is active:

- Treat the working tree as shared live state; other people and agents are editing it now.
- Follow the installed Weave collaboration/task/conflict/commit skills.
- Create a Weave Task before substantial changes:
  `weave task start --description "..." --file <path>`.
- Inspect overlapping active Tasks with `weave task list --json`; overlap is context, not a lock.
- Re-read important files before finalizing changes when concurrent activity occurred.
- Never perform raw Git write operations: no `git add`, `commit`, `pull`, `push`, `merge`,
  `rebase`, `cherry-pick`, `reset`, `checkout`, `switch` or `stash`.
  Read-only Git (`status`, `diff`, `log`, `show`) stays allowed.
- Use Weave for Git publication: `weave commit prepare` then `weave commit create <prepare_id>`.
- Resolve conflicts with `weave conflict show/resolve`; never write Git conflict markers by hand.

A non-host may request a Weave commit, but only the host coordinator builds the canonical Git
objects, updates the branch and pushes.

If no Weave session is active, normal Git workflows apply.
"#;

pub fn managed_block() -> String {
    format!("{BEGIN}\n{BODY}\n{END}")
}

pub struct BootstrapResult {
    pub path: String,
    pub created: bool,
    pub updated: bool,
}

/// Create or update the managed block in `<repo>/AGENTS.md`.
pub fn apply(repo_root: &Path) -> Result<BootstrapResult> {
    let path = repo_root.join("AGENTS.md");
    let block = managed_block();

    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };

    let (content, created, updated) = match existing {
        None => (format!("{block}\n"), true, true),
        Some(text) => match (text.find(BEGIN), text.find(END)) {
            (Some(start), Some(end)) if end > start => {
                let end = end + END.len();
                let current = &text[start..end];
                if current == block {
                    return Ok(BootstrapResult {
                        path: path.display().to_string(),
                        created: false,
                        updated: false,
                    });
                }
                let mut next = String::new();
                next.push_str(&text[..start]);
                next.push_str(&block);
                next.push_str(&text[end..]);
                (next, false, true)
            }
            _ => {
                let mut next = text.clone();
                if !next.ends_with('\n') {
                    next.push('\n');
                }
                if !next.is_empty() {
                    next.push('\n');
                }
                next.push_str(&block);
                next.push('\n');
                (next, false, true)
            }
        },
    };

    crate::util::write_atomic(&path, content.as_bytes())?;
    Ok(BootstrapResult {
        path: path.display().to_string(),
        created,
        updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("weave-boot-{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_then_updates_without_touching_other_text() {
        let dir = temp_dir();
        let result = apply(&dir).unwrap();
        assert!(result.created);

        let path = dir.join("AGENTS.md");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.insert_str(0, "# Project instructions\n\nDo the thing.\n\n");
        std::fs::write(&path, &text).unwrap();

        // Corrupt the managed block and re-apply.
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("## Weave collaboration", "## Stale heading");
        std::fs::write(&path, text).unwrap();

        let result = apply(&dir).unwrap();
        assert!(result.updated);
        let final_text = std::fs::read_to_string(&path).unwrap();
        assert!(final_text.starts_with("# Project instructions"));
        assert!(final_text.contains("Do the thing."));
        assert!(final_text.contains("## Weave collaboration"));
        assert_eq!(final_text.matches(BEGIN).count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn appends_when_no_block_present() {
        let dir = temp_dir();
        std::fs::write(dir.join("AGENTS.md"), "# Existing\n").unwrap();
        apply(&dir).unwrap();
        let text = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert!(text.starts_with("# Existing"));
        assert!(text.contains(BEGIN));
        std::fs::remove_dir_all(&dir).ok();
    }
}
