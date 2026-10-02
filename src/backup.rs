// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. https://mozilla.org/MPL/2.0/.

//! Independent recovery archives. Blobs are copied into the archive so normal
//! garbage collection cannot remove the last copy of detached work.
use crate::{error::Result, session::Paths};
use std::path::Path;

pub fn archive_session(paths: &Paths, reason: &str) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = paths.weave_dir.join("backups").join(&id);
    std::fs::create_dir_all(&dir)?;
    crate::backup::restrict_directory(&dir)?;
    for name in [
        "session.json",
        "host.sqlite",
        "state.sqlite",
        "blobs",
        "conflicts",
    ] {
        let source = paths.weave_dir.join(name);
        if !source.exists() {
            continue;
        }
        let target = dir.join(name);
        if name.ends_with(".sqlite") {
            // VACUUM INTO includes committed WAL pages, unlike a filesystem copy.
            let conn = crate::db::open(&source)?;
            conn.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
        } else {
            copy_tree(&source, &target)?;
        }
    }
    crate::util::write_atomic(
        &dir.join("backup.json"),
        &serde_json::to_vec_pretty(
            &serde_json::json!({"id": id, "reason": reason, "created_at_ms": crate::util::now_ms()}),
        )?,
    )?;
    sync_directory(&dir)?;
    sync_directory(dir.parent().unwrap())?;
    Ok(id)
}

pub fn list(paths: &Paths) -> Result<Vec<serde_json::Value>> {
    let root = paths.weave_dir.join("backups");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let manifest = entry?.path().join("backup.json");
        if manifest.exists() {
            out.push(serde_json::from_slice(&std::fs::read(manifest)?)?);
        }
    }
    Ok(out)
}

pub fn export(paths: &Paths, id: &str, destination: &Path) -> Result<()> {
    let id =
        uuid::Uuid::parse_str(id).map_err(|_| crate::error::usage("Invalid backup identifier."))?;
    if destination.exists() {
        return Err(crate::error::usage(
            "The backup export destination must not already exist.",
        ));
    }
    copy_tree(
        &paths.weave_dir.join("backups").join(id.to_string()),
        destination,
    )
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.is_dir() {
        std::fs::create_dir_all(target)?;
        restrict_directory(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        sync_directory(target)?;
    } else if meta.is_file() {
        let mut input = std::fs::File::open(source)?;
        let mut output = std::fs::File::create(target)?;
        std::io::copy(&mut input, &mut output)?;
        std::fs::set_permissions(target, meta.permissions())?;
        // Flush through the writable handle (required by Windows).
        output.sync_all()?;
    } else if meta.file_type().is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::fs::read_link(source)?, target)?;
        #[cfg(windows)]
        {
            if source.is_dir() {
                std::os::windows::fs::symlink_dir(std::fs::read_link(source)?, target)?;
            } else {
                std::os::windows::fs::symlink_file(std::fs::read_link(source)?, target)?;
            }
        }
    } else {
        return Err(crate::error::unsupported(
            "Unexpected symbolic link in Weave recovery storage.",
        ));
    }
    Ok(())
}

/// Preserve the original index and every Git-visible local file separately from
/// canonical data. Ignored obstructions are captured on demand before writing.
pub fn capture_worktree(paths: &Paths, id: &str) -> Result<()> {
    let dir = paths.weave_dir.join("backups").join(id);
    let index = paths.git_dir.join("index");
    if index.exists() {
        copy_tree(&index, &dir.join("index"))?;
        crate::gitx::archive_index_objects(&paths.repo_root, &dir.join("index-objects"))?;
    }
    let mut manifest = Vec::new();
    for name in crate::gitx::list_repository_paths(&paths.repo_root)? {
        let path = crate::path::RepoPath::new(&name)?;
        let source = path.to_fs_path(&paths.repo_root);
        if source.is_file() || source.is_symlink() {
            let target = path.to_fs_path(&dir.join("working-tree"));
            std::fs::create_dir_all(target.parent().unwrap())?;
            copy_tree(&source, &target)?;
        }
        manifest.push(name);
    }
    crate::util::write_atomic(
        &dir.join("working-tree.json"),
        &serde_json::to_vec(&manifest)?,
    )?;
    // Persist nested directory entries as well as the copied file contents.
    if dir.join("working-tree").exists() {
        sync_tree_directories(&dir.join("working-tree"))?;
    }
    sync_directory(&dir)?;
    Ok(())
}

fn sync_tree_directories(path: &Path) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree_directories(&entry.path())?;
        }
    }
    sync_directory(path)
}

pub fn capture_file(paths: &Paths, id: &str, path: &crate::path::RepoPath) -> Result<()> {
    let target = path.to_fs_path(
        &paths
            .weave_dir
            .join("backups")
            .join(id)
            .join("working-tree"),
    );
    std::fs::create_dir_all(target.parent().unwrap())?;
    copy_tree(&path.to_fs_path(&paths.repo_root), &target)?;
    sync_tree_directories(&paths.weave_dir.join("backups").join(id))
}

pub fn restrict_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Relocate an obstructing directory or link without following it or deleting
/// its content. This runs only after an explicit join local-work policy.
pub fn move_obstruction(paths: &Paths, id: &str, path: &crate::path::RepoPath) -> Result<()> {
    let source = path.to_fs_path(&paths.repo_root);
    let target = path.to_fs_path(&paths.weave_dir.join("backups").join(id).join("obstacles"));
    std::fs::create_dir_all(target.parent().unwrap())?;
    std::fs::rename(&source, &target)?;
    sync_directory(source.parent().unwrap())?;
    sync_directory(target.parent().unwrap())?;
    sync_tree_directories(&paths.weave_dir.join("backups").join(id))?;
    Ok(())
}
