// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin wrappers around the installed `git` executable.
//!
//! Specification section 8: external Git behaviour is delegated to Git rather
//! than reimplemented. Section 128: publication uses Git plumbing
//! (`hash-object`, `update-index`, `write-tree`, `commit-tree`, `update-ref`,
//! `read-tree`, `pack-objects`) instead of staging the live working tree.

use crate::error::{git as git_err, repository, Result};
use crate::model::{FileEntry, GitMode};
use crate::path::RepoPath;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Raw result of a git invocation.
pub struct GitOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitOutput {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }
    pub fn trimmed(&self) -> String {
        self.stdout_str().trim().to_string()
    }
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

fn base_command(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root);
    // Never let a Weave-issued read touch the index lock.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    // Deterministic, non-interactive behaviour.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_PAGER", "cat");
    cmd.env("LC_ALL", "C");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Run git, capturing output. Never fails on non-zero exit; inspect `status`.
pub fn run(root: &Path, args: &[&str]) -> Result<GitOutput> {
    run_env(root, args, &[])
}

pub fn run_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<GitOutput> {
    let mut cmd = base_command(root);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| {
        git_err(format!("Could not run git: {e}"))
            .with_detail("Weave requires the `git` executable on PATH. Install Git and retry.")
    })?;
    Ok(GitOutput {
        status: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Run git feeding `input` on stdin.
pub fn run_stdin(root: &Path, args: &[&str], input: &[u8]) -> Result<GitOutput> {
    run_stdin_env(root, args, input, &[])
}

pub fn run_stdin_env(
    root: &Path,
    args: &[&str],
    input: &[u8],
    env: &[(&str, &str)],
) -> Result<GitOutput> {
    let mut cmd = base_command(root);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| git_err(format!("Could not run git: {e}")))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| git_err("Could not write to git stdin"))?;
        stdin.write_all(input)?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| git_err(format!("git failed: {e}")))?;
    Ok(GitOutput {
        status: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Run git and require success.
pub fn require(root: &Path, args: &[&str]) -> Result<GitOutput> {
    let out = run(root, args)?;
    if !out.ok() {
        return Err(
            git_err(format!("git {} failed", args.join(" "))).with_detail(
                if out.stderr.is_empty() {
                    out.stdout_str()
                } else {
                    out.stderr.clone()
                },
            ),
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Locate the repository working-tree root containing `start`.
pub fn discover_root(start: &Path) -> Result<PathBuf> {
    let out = run(start, &["rev-parse", "--show-toplevel"])?;
    if !out.ok() {
        return Err(
            repository("This directory is not inside a Git repository.").with_detail(
                "Weave operates on an existing Git repository. Run `git init` or change into a \
             repository and retry.",
            ),
        );
    }
    let top = out.trimmed();
    if top.is_empty() {
        return Err(repository("This directory is not inside a Git repository."));
    }
    Ok(PathBuf::from(top))
}

/// The `.git` directory for `root` (absolute).
pub fn git_dir(root: &Path) -> Result<PathBuf> {
    let out = require(root, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(out.trimmed()))
}

pub fn git_common_dir(root: &Path) -> Result<PathBuf> {
    let out = require(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(out.trimmed()))
}

pub fn version(root: &Path) -> Result<String> {
    Ok(require(root, &["--version"])?.trimmed())
}

// ---------------------------------------------------------------------------
// Repository state
// ---------------------------------------------------------------------------

pub fn current_branch(root: &Path) -> Result<Option<String>> {
    let out = run(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if out.ok() {
        Ok(Some(out.trimmed()))
    } else {
        Ok(None) // detached HEAD
    }
}

pub fn head_oid(root: &Path) -> Result<Option<String>> {
    let out = run(root, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    if out.ok() {
        let oid = out.trimmed();
        if oid.is_empty() {
            Ok(None)
        } else {
            Ok(Some(oid))
        }
    } else {
        Ok(None)
    }
}

pub fn rev_parse(root: &Path, rev: &str) -> Result<Option<String>> {
    let out = run(root, &["rev-parse", "--verify", "--quiet", rev])?;
    if out.ok() {
        let s = out.trimmed();
        if s.is_empty() {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    } else {
        Ok(None)
    }
}

pub fn object_exists(root: &Path, oid: &str) -> Result<bool> {
    Ok(run(root, &["cat-file", "-e", &format!("{oid}^{{object}}")])?.ok())
}

/// Names of any Git operation currently in progress.
pub fn operation_in_progress(root: &Path) -> Result<Option<String>> {
    let gd = git_dir(root)?;
    let checks: &[(&str, &str)] = &[
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
    ];
    for (file, name) in checks {
        if gd.join(file).exists() {
            return Ok(Some((*name).to_string()));
        }
    }
    Ok(None)
}

/// Returns the porcelain status lines that make the tree unclean.
pub fn dirty_entries(root: &Path) -> Result<Vec<String>> {
    let out = require(
        root,
        &["status", "--porcelain=v1", "--untracked-files=normal", "-z"],
    )?;
    let text = out.stdout_str();
    Ok(text
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// True when the index differs from HEAD (i.e. something was staged).
pub fn has_staged_changes(root: &Path) -> Result<bool> {
    let out = run(
        root,
        &["diff", "--cached", "--quiet", "--no-ext-diff", "HEAD"],
    )?;
    // 0 = no differences, 1 = differences, other = error (e.g. no HEAD yet)
    Ok(out.status == 1)
}

/// Upstream tracking branch of `branch`, e.g. `origin/main`.
pub fn upstream_of(root: &Path, branch: &str) -> Result<Option<String>> {
    let spec = format!("{branch}@{{upstream}}");
    let out = run(
        root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", &spec],
    )?;
    if out.ok() {
        let s = out.trimmed();
        if s.is_empty() {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    } else {
        Ok(None)
    }
}

pub fn config_get(root: &Path, key: &str) -> Result<Option<String>> {
    let out = run(root, &["config", "--get", key])?;
    if out.ok() {
        let s = out.trimmed();
        if s.is_empty() {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Unsupported repository features (specification section 12)
// ---------------------------------------------------------------------------

/// A single unsupported-feature finding.
#[derive(Debug, Clone)]
pub struct Unsupported {
    pub feature: String,
    pub detail: String,
}

/// Detect every repository feature Weave V1 refuses to synchronize.
pub fn detect_unsupported(root: &Path) -> Result<Vec<Unsupported>> {
    let mut found = Vec::new();

    // Secondary worktree: the git dir differs from the common dir.
    let gd = git_dir(root)?;
    let common = git_common_dir(root)?;
    if gd != common {
        found.push(Unsupported {
            feature: "secondary Git worktree".into(),
            detail: format!(
                "This checkout is a linked worktree ({}). Run Weave from the primary worktree.",
                gd.display()
            ),
        });
    }

    // Submodules / gitlinks / tracked symlinks, from the index.
    let out = require(root, &["ls-files", "--stage", "-z"])?;
    let text = out.stdout_str();
    let mut symlinks = Vec::new();
    let mut gitlinks = Vec::new();
    for record in text.split('\0').filter(|s| !s.is_empty()) {
        // "<mode> <oid> <stage>\t<path>"
        let (meta, path) = match record.split_once('\t') {
            Some(v) => v,
            None => continue,
        };
        let mode = meta.split_whitespace().next().unwrap_or("");
        match mode {
            "120000" => symlinks.push(path.to_string()),
            "160000" => gitlinks.push(path.to_string()),
            _ => {}
        }
    }
    if !symlinks.is_empty() {
        found.push(Unsupported {
            feature: "tracked symlinks".into(),
            detail: format!(
                "{} tracked symlink(s), e.g. {}",
                symlinks.len(),
                symlinks[0]
            ),
        });
    }
    if !gitlinks.is_empty() {
        found.push(Unsupported {
            feature: "submodules / gitlinks".into(),
            detail: format!("{} gitlink(s), e.g. {}", gitlinks.len(), gitlinks[0]),
        });
    }
    if root.join(".gitmodules").exists() {
        found.push(Unsupported {
            feature: "submodules".into(),
            detail: ".gitmodules is present.".into(),
        });
    }

    // Sparse checkout.
    for key in ["core.sparseCheckout", "core.sparsecheckout"] {
        if let Some(v) = config_get(root, key)? {
            if v.eq_ignore_ascii_case("true") {
                found.push(Unsupported {
                    feature: "sparse checkout".into(),
                    detail: "core.sparseCheckout is enabled.".into(),
                });
                break;
            }
        }
    }

    // Attributes that request transformations Weave cannot reproduce. Asking
    // Git itself is the only reliable test: it accounts for repository,
    // `.git/info` and global attribute sources, and it does not misreport a
    // globally installed filter (such as git-lfs) that no path actually uses.
    let paths = list_repository_paths(root)?;
    let mut lfs = Vec::new();
    let mut filters = Vec::new();
    let mut encodings = Vec::new();
    for (path, attribute, value) in check_attr(root, &paths, &["filter", "working-tree-encoding"])?
    {
        match attribute.as_str() {
            "filter" => {
                if value == "lfs" {
                    lfs.push(path);
                } else {
                    filters.push(format!("{path} (filter={value})"));
                }
            }
            "working-tree-encoding" => encodings.push(format!("{path} ({value})")),
            _ => {}
        }
    }
    if !lfs.is_empty() {
        found.push(Unsupported {
            feature: "Git LFS".into(),
            detail: format!("{} path(s) use filter=lfs, e.g. {}", lfs.len(), lfs[0]),
        });
    }
    if !filters.is_empty() {
        found.push(Unsupported {
            feature: "custom clean/smudge filters".into(),
            detail: format!("{} path(s), e.g. {}", filters.len(), filters[0]),
        });
    }
    if !encodings.is_empty() {
        found.push(Unsupported {
            feature: "working-tree-encoding".into(),
            detail: format!("{} path(s), e.g. {}", encodings.len(), encodings[0]),
        });
    }

    Ok(found)
}

/// Ask Git which of `attributes` are set for each of `paths`. Only attributes
/// with a concrete value are returned; `unspecified` and `unset` are omitted.
pub fn check_attr(
    root: &Path,
    paths: &[String],
    attributes: &[&str],
) -> Result<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    if paths.is_empty() || attributes.is_empty() {
        return Ok(out);
    }
    let mut args: Vec<&str> = vec!["check-attr", "-z", "--stdin"];
    args.extend_from_slice(attributes);
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let result = run_stdin(root, &args, &input)?;
    if !result.ok() {
        return Ok(out);
    }
    // `-z` output is a flat NUL-separated stream of (path, attribute, value).
    let text = result.stdout_str();
    let fields: Vec<&str> = text.split('\0').collect();
    for chunk in fields.chunks(3) {
        if chunk.len() < 3 {
            break;
        }
        let (path, attribute, value) = (chunk[0], chunk[1], chunk[2]);
        if path.is_empty() {
            continue;
        }
        if value == "unspecified" || value == "unset" || value.is_empty() {
            continue;
        }
        out.push((path.to_string(), attribute.to_string(), value.to_string()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// File enumeration and ignore semantics (specification section 46)
// ---------------------------------------------------------------------------

/// Every path Git considers part of the repository: tracked files plus
/// untracked files that are not ignored. This reuses Git's own ignore rules
/// rather than reimplementing `.gitignore`.
pub fn list_repository_paths(root: &Path) -> Result<Vec<String>> {
    let out = require(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut seen = std::collections::BTreeSet::new();
    for p in out.stdout_str().split('\0') {
        if !p.is_empty() {
            seen.insert(p.to_string());
        }
    }
    Ok(seen.into_iter().collect())
}

/// Subset of `paths` that Git considers ignored.
pub fn filter_ignored(root: &Path, paths: &[String]) -> Result<std::collections::HashSet<String>> {
    let mut ignored = std::collections::HashSet::new();
    if paths.is_empty() {
        return Ok(ignored);
    }
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    let out = run_stdin(root, &["check-ignore", "-z", "--stdin"], &input)?;
    // 0 = at least one ignored, 1 = none ignored, 128 = error
    if out.status == 128 {
        return Err(git_err("git check-ignore failed").with_detail(out.stderr.clone()));
    }
    for p in out.stdout_str().split('\0') {
        if !p.is_empty() {
            ignored.insert(p.to_string());
        }
    }
    Ok(ignored)
}

// ---------------------------------------------------------------------------
// Three-way text merge (specification section 78)
// ---------------------------------------------------------------------------

pub enum MergeOutcome {
    Clean(Vec<u8>),
    Conflict,
}

/// `git merge-file` incorporates the changes of two descendants of a common
/// base into one result. Conflicted output is discarded by the caller: the
/// canonical working tree never receives generated conflict markers
/// (specification sections 7.4 and 80).
pub fn merge_file(
    root: &Path,
    scratch: &Path,
    current: &[u8],
    base: &[u8],
    incoming: &[u8],
) -> Result<MergeOutcome> {
    std::fs::create_dir_all(scratch)?;
    let tag = crate::util::random_hex(8);
    let cur_p = scratch.join(format!("merge-{tag}.current"));
    let base_p = scratch.join(format!("merge-{tag}.base"));
    let inc_p = scratch.join(format!("merge-{tag}.incoming"));
    std::fs::write(&cur_p, current)?;
    std::fs::write(&base_p, base)?;
    std::fs::write(&inc_p, incoming)?;

    let result = run(
        root,
        &[
            "merge-file",
            "-p",
            "--quiet",
            "-L",
            "canonical",
            "-L",
            "base",
            "-L",
            "incoming",
            cur_p.to_str().unwrap_or_default(),
            base_p.to_str().unwrap_or_default(),
            inc_p.to_str().unwrap_or_default(),
        ],
    );

    let _ = std::fs::remove_file(&cur_p);
    let _ = std::fs::remove_file(&base_p);
    let _ = std::fs::remove_file(&inc_p);

    let out = result?;
    if out.status == 0 {
        Ok(MergeOutcome::Clean(out.stdout))
    } else if out.status > 0 && out.status < 128 {
        Ok(MergeOutcome::Conflict)
    } else {
        Err(git_err("git merge-file failed").with_detail(out.stderr))
    }
}

// ---------------------------------------------------------------------------
// Object construction (specification sections 125-128)
// ---------------------------------------------------------------------------

/// Create a Git blob from a stored Weave blob, applying the host repository's
/// path-specific Git semantics (specification section 126).
///
/// `source` is the blob store's own file, so Git reads it directly instead of
/// having the content piped through this process. `--path` still names the
/// repository path, which is what selects the attributes Git applies.
pub fn hash_object(root: &Path, path: &RepoPath, source: &Path) -> Result<String> {
    let source = source.to_string_lossy().to_string();
    let out = run(
        root,
        &[
            "hash-object",
            "-w",
            "-t",
            "blob",
            "--path",
            path.as_str(),
            "--",
            source.as_str(),
        ],
    )?;
    if !out.ok() {
        return Err(
            git_err(format!("Could not create a Git blob for {path}")).with_detail(out.stderr)
        );
    }
    Ok(out.trimmed())
}

/// Build a Git tree from a full manifest using a temporary index, so the live
/// working tree and the real index are untouched.
pub fn write_tree(
    root: &Path,
    scratch: &Path,
    entries: &BTreeMap<RepoPath, (GitMode, String)>,
) -> Result<String> {
    std::fs::create_dir_all(scratch)?;
    let index_path = scratch.join(format!("index-{}", crate::util::random_hex(8)));
    let _ = std::fs::remove_file(&index_path);
    let index_str = index_path.to_string_lossy().to_string();

    let mut payload = Vec::new();
    for (path, (mode, oid)) in entries {
        payload.extend_from_slice(format!("{} {}\t{}\n", mode.as_str(), oid, path).as_bytes());
    }

    let res = (|| -> Result<String> {
        let out = run_stdin_env(
            root,
            &["update-index", "--index-info"],
            &payload,
            &[("GIT_INDEX_FILE", index_str.as_str())],
        )?;
        if !out.ok() {
            return Err(
                git_err("Could not build the Git index for publication").with_detail(out.stderr)
            );
        }
        let out = run_env(
            root,
            &["write-tree"],
            &[("GIT_INDEX_FILE", index_str.as_str())],
        )?;
        if !out.ok() {
            return Err(git_err("Could not write the Git tree").with_detail(out.stderr));
        }
        Ok(out.trimmed())
    })();

    let _ = std::fs::remove_file(&index_path);
    res
}

#[allow(clippy::too_many_arguments)]
pub fn commit_tree(
    root: &Path,
    tree: &str,
    parent: Option<&str>,
    message: &str,
    author_name: &str,
    author_email: &str,
    committer_name: &str,
    committer_email: &str,
    timestamp: i64,
    timezone: &str,
) -> Result<String> {
    let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
    if let Some(p) = parent {
        args.push("-p".into());
        args.push(p.into());
    }
    let date = format!("{timestamp} {timezone}");
    let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = run_stdin_env(
        root,
        &argrefs,
        message.as_bytes(),
        &[
            ("GIT_AUTHOR_NAME", author_name),
            ("GIT_AUTHOR_EMAIL", author_email),
            ("GIT_AUTHOR_DATE", date.as_str()),
            ("GIT_COMMITTER_NAME", committer_name),
            ("GIT_COMMITTER_EMAIL", committer_email),
            ("GIT_COMMITTER_DATE", date.as_str()),
        ],
    )?;
    if !out.ok() {
        return Err(git_err("Could not create the Git commit object").with_detail(out.stderr));
    }
    Ok(out.trimmed())
}

/// Compare-and-swap branch update (specification section 133).
pub fn update_ref_cas(
    root: &Path,
    refname: &str,
    new_oid: &str,
    expected_old: Option<&str>,
) -> Result<()> {
    let zero = "0000000000000000000000000000000000000000";
    let old = expected_old.unwrap_or(zero);
    let out = run(
        root,
        &[
            "update-ref",
            "-m",
            "weave publication",
            refname,
            new_oid,
            old,
        ],
    )?;
    if !out.ok() {
        return Err(crate::error::integrity(format!(
            "GitIntegrityError: {refname} did not match the expected previous commit"
        ))
        .with_detail(format!(
            "Expected {old}\n\n{}\n\nWeave paused this replica rather than rewriting Git state.",
            out.stderr
        )));
    }
    Ok(())
}

/// Update the index to a tree without touching working-tree files
/// (specification section 134).
pub fn read_tree_into_index(root: &Path, tree: &str) -> Result<()> {
    let out = run(root, &["read-tree", tree])?;
    if !out.ok() {
        return Err(
            git_err("Could not update the Git index to the published tree").with_detail(out.stderr),
        );
    }
    Ok(())
}

/// Pack the objects reachable from `commit` but not from `parent`.
/// Write the pack introduced by `commit` straight to `dest`.
///
/// The pack goes to a file rather than to a `Vec`, because a pack containing a
/// large blob has exactly the problem the blob plane exists to remove, and this
/// runs on the critical path of `weave commit create`.
pub fn pack_objects_to(root: &Path, commit: &str, parent: Option<&str>, dest: &Path) -> Result<()> {
    let mut revs = String::new();
    revs.push_str(commit);
    revs.push('\n');
    if let Some(p) = parent {
        revs.push('^');
        revs.push_str(p);
        revs.push('\n');
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(dest)?;

    let mut cmd = base_command(root);
    cmd.args([
        "pack-objects",
        "--revs",
        "--stdout",
        "--delta-base-offset",
        "-q",
    ]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::from(file));
    cmd.stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| git_err(format!("Could not run git: {e}")))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| git_err("Could not write to git stdin"))?;
        stdin.write_all(revs.as_bytes())?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| git_err(format!("git failed: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(dest);
        return Err(git_err("Could not pack Git objects for distribution")
            .with_detail(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(())
}

/// Install exact host-produced Git objects (specification sections 131, 192).
///
/// Reads the pack from disk: it arrived over the blob plane as a file and there
/// is no reason to lift it into memory to hand it back to a child process.
pub fn unpack_objects(root: &Path, pack: &Path) -> Result<()> {
    let file = std::fs::File::open(pack)?;
    let mut cmd = base_command(root);
    cmd.args(["unpack-objects", "-q"]);
    cmd.stdin(Stdio::from(file));
    let out = cmd
        .output()
        .map_err(|e| git_err(format!("Could not run git: {e}")))?;
    if !out.status.success() {
        return Err(
            git_err("Could not install the Git objects sent by the host")
                .with_detail(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Remote
// ---------------------------------------------------------------------------

pub struct PushResult {
    pub ok: bool,
    pub diverged: bool,
    pub message: String,
}

pub fn push(
    root: &Path,
    remote: &str,
    branch: &str,
    expected_remote_oid: Option<&str>,
) -> Result<PushResult> {
    // Never force. If the remote moved under us, report it (section 140).
    let refspec = match expected_remote_oid {
        Some(old) => format!("--force-with-lease=refs/heads/{branch}:{old}"),
        None => "--no-force".to_string(),
    };
    let _ = refspec; // force-with-lease is deliberately unused: V1 never forces.
    let spec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let out = run(root, &["push", remote, &spec])?;
    if out.ok() {
        return Ok(PushResult {
            ok: true,
            diverged: false,
            message: out.stderr,
        });
    }
    let lower = out.stderr.to_ascii_lowercase();
    let diverged = lower.contains("non-fast-forward")
        || lower.contains("fetch first")
        || lower.contains("rejected")
        || lower.contains("behind");
    Ok(PushResult {
        ok: false,
        diverged,
        message: out.stderr,
    })
}

pub fn remote_branch_oid(root: &Path, remote: &str, branch: &str) -> Result<Option<String>> {
    let out = run(
        root,
        &[
            "ls-remote",
            "--heads",
            remote,
            &format!("refs/heads/{branch}"),
        ],
    )?;
    if !out.ok() {
        return Ok(None);
    }
    let text = out.trimmed();
    Ok(text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .map(|s| s.to_string()))
}

// ---------------------------------------------------------------------------
// Entry helpers
// ---------------------------------------------------------------------------

/// The Git mode a file on disk should be recorded with.
///
/// On Unix the executable bit is honoured; on Windows Git cannot represent it
/// per-file, so an existing canonical mode is preserved instead of being reset.
pub fn mode_for_disk_file(path: &Path, previous: Option<&FileEntry>) -> GitMode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.permissions().mode() & 0o111 != 0 {
                return GitMode::Executable;
            }
            return GitMode::Regular;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    previous.map(|e| e.git_mode).unwrap_or(GitMode::Regular)
}
