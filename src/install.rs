// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a packaged Weave installation keeps its own files.
//!
//! Weave ships `cloudflared` inside its own installation so a normal user
//! installs one package and nothing else. Discovery is therefore anchored on
//! the running executable rather than on `PATH`: whichever `weave` you launched
//! uses *its* `cloudflared`, never some other copy that happens to be earlier in
//! the search path.
//!
//! The packaged layouts are:
//!
//! ```text
//! Windows   %LOCALAPPDATA%\Programs\Weave\weave.exe
//!           %LOCALAPPDATA%\Programs\Weave\cloudflared.exe
//!           %LOCALAPPDATA%\Programs\Weave\licenses\cloudflared\{LICENSE,NOTICE}
//!
//! macOS     /usr/local/bin/weave
//!           /usr/local/libexec/weave/cloudflared-aarch64
//!           /usr/local/libexec/weave/cloudflared-x86_64
//!           /usr/local/libexec/weave/licenses/cloudflared/{LICENSE,NOTICE}
//!
//! Linux     /usr/bin/weave
//!           /usr/lib/weave/cloudflared
//!           /usr/share/doc/weave/third-party/cloudflared/{LICENSE,NOTICE}
//! ```
//!
//! and the portable tarball mirrors the Linux one relative to its own root
//! (`bin/weave`, `lib/weave/cloudflared`, `share/doc/weave/...`).
//!
//! `WEAVE_CLOUDFLARED` overrides the search with an explicit path. A system
//! `cloudflared` on `PATH` is still accepted, but only as a last resort and it
//! is reported as such: a packaged installation must never depend on it.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The `cloudflared` release Weave bundles.
///
/// Kept byte-identical to `CLOUDFLARED_VERSION` in
/// `packaging/cloudflared/pinned.env`, which is what the release build
/// downloads and checksum-verifies. `packaging_pin_matches_crate` in
/// `tests/packaging.rs` fails the build if the two drift apart.
pub const CLOUDFLARED_VERSION: &str = "2026.8.2";

/// The file every Weave package drops beside its bundled `cloudflared`.
///
/// Its presence is what tells Weave it is running from a package rather than
/// from `cargo build`, which is the difference between "the bundle is missing,
/// this package is broken" and "no bundle here, you built from source".
pub const BUNDLE_MANIFEST: &str = "weave-bundle.json";

/// What a package recorded about itself when it was built.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    /// The Weave version this package was built from.
    pub weave_version: String,
    /// The `cloudflared` release this package carries.
    pub cloudflared_version: String,
    /// Which package produced this installation, e.g. `linux-x64-deb`.
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at: Option<String>,
    /// Where the manifest was read from. Not part of the on-disk file.
    #[serde(skip)]
    pub path: PathBuf,
}

/// Read this installation's bundle manifest, if it has one.
pub fn bundle() -> Option<Bundle> {
    for dir in support_dirs() {
        let candidate = dir.join(BUNDLE_MANIFEST);
        if !candidate.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&candidate).ok()?;
        let mut bundle: Bundle = serde_json::from_str(&text).ok()?;
        bundle.path = candidate;
        return Some(bundle);
    }
    None
}

/// How the `cloudflared` Weave is about to run was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CloudflaredSource {
    /// Selected explicitly with `WEAVE_CLOUDFLARED`.
    Override,
    /// Shipped inside this Weave installation.
    Bundled,
    /// A system installation found on `PATH`; useful when developing, never
    /// what a packaged installation relies on.
    Path,
}

impl CloudflaredSource {
    pub fn describe(self) -> &'static str {
        match self {
            CloudflaredSource::Override => "WEAVE_CLOUDFLARED",
            CloudflaredSource::Bundled => "bundled with Weave",
            CloudflaredSource::Path => "found on PATH",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Cloudflared {
    pub path: PathBuf,
    pub source: CloudflaredSource,
}

/// The running `weave` executable, as the operating system reports it.
pub fn exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// True when the running executable looks like an installed `weave`, rather
/// than a test harness or an example binary that merely links the library.
pub fn running_as_weave() -> bool {
    exe()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .map(|stem| stem == "weave")
        .unwrap_or(false)
}

/// Directories that may contain the installed `weave` executable: the plain one
/// and, when it differs, the fully resolved one. Resolving matters on macOS and
/// for the portable tarball, where `weave` may be reached through a symlink.
fn exe_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(exe) = exe() {
        if let Some(dir) = exe.parent() {
            push_unique(&mut dirs, dir.to_path_buf());
        }
        if let Ok(real) = std::fs::canonicalize(&exe) {
            if let Some(dir) = real.parent() {
                push_unique(&mut dirs, dir.to_path_buf());
            }
        }
    }
    dirs
}

/// The support directories implied by one executable directory.
///
/// Pure, so the documented layouts can be asserted without an installation:
/// `C:\...\Weave` yields itself, `/usr/bin` yields `/usr/lib/weave`,
/// `/usr/local/bin` yields `/usr/local/libexec/weave`.
pub fn support_dirs_for(exe_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Windows: everything sits beside weave.exe.
    push_unique(&mut dirs, exe_dir.to_path_buf());
    if let Some(prefix) = exe_dir.parent() {
        // Linux package and portable tarball.
        push_unique(&mut dirs, prefix.join("lib").join("weave"));
        // macOS package.
        push_unique(&mut dirs, prefix.join("libexec").join("weave"));
    }
    dirs
}

/// Every directory that may hold this installation's private runtime files, in
/// the order they should be trusted.
pub fn support_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for exe_dir in exe_dirs() {
        for dir in support_dirs_for(&exe_dir) {
            push_unique(&mut dirs, dir);
        }
    }
    // Absolute fallbacks, for the case where `weave` was copied elsewhere but
    // the package is still installed.
    if cfg!(target_os = "macos") {
        push_unique(&mut dirs, PathBuf::from("/usr/local/libexec/weave"));
    }
    if cfg!(target_os = "linux") {
        push_unique(&mut dirs, PathBuf::from("/usr/lib/weave"));
        push_unique(&mut dirs, PathBuf::from("/usr/local/lib/weave"));
    }
    dirs
}

/// Every directory that may hold third-party licence texts.
pub fn licenses_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for support in support_dirs() {
        push_unique(&mut dirs, support.join("licenses"));
    }
    for exe_dir in exe_dirs() {
        if let Some(prefix) = exe_dir.parent() {
            push_unique(
                &mut dirs,
                prefix
                    .join("share")
                    .join("doc")
                    .join("weave")
                    .join("third-party"),
            );
        }
    }
    if cfg!(target_os = "linux") {
        push_unique(&mut dirs, PathBuf::from("/usr/share/doc/weave/third-party"));
    }
    dirs
}

/// The bundled third-party notice for `cloudflared`, if this installation
/// carries one.
pub fn cloudflared_notice() -> Option<PathBuf> {
    for dir in licenses_dirs() {
        for name in ["NOTICE", "LICENSE"] {
            let candidate = dir.join("cloudflared").join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve the `cloudflared` this Weave should run.
///
/// Bundled copies win over `PATH` so a packaged installation is deterministic
/// even on a machine that also has Cloudflare's own package installed.
pub fn cloudflared() -> Option<Cloudflared> {
    if let Some(raw) = std::env::var_os("WEAVE_CLOUDFLARED") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(Cloudflared {
                path,
                source: CloudflaredSource::Override,
            });
        }
        // An override that points nowhere is a configuration mistake, not an
        // invitation to silently use a different binary.
        return None;
    }
    if let Some(path) = bundled_cloudflared() {
        return Some(Cloudflared {
            path,
            source: CloudflaredSource::Bundled,
        });
    }
    cloudflared_on_path().map(|path| Cloudflared {
        path,
        source: CloudflaredSource::Path,
    })
}

/// The names a bundled `cloudflared` may have, most specific first.
///
/// The architecture-qualified name comes first because the macOS package ships
/// one file per architecture: Cloudflare publishes `cloudflared-darwin-amd64`
/// and `cloudflared-darwin-arm64` and no universal build, so the choice has to
/// happen here rather than in the Mach-O loader.
pub fn bundled_names() -> Vec<String> {
    let suffix = std::env::consts::EXE_SUFFIX;
    vec![
        format!("cloudflared-{}{suffix}", std::env::consts::ARCH),
        format!("cloudflared{suffix}"),
    ]
}

/// The bundled `cloudflared`, ignoring `PATH` and the environment override.
pub fn bundled_cloudflared() -> Option<PathBuf> {
    bundled_cloudflared_in(&support_dirs())
}

/// [`bundled_cloudflared`], searching directories the caller chooses.
pub fn bundled_cloudflared_in(dirs: &[PathBuf]) -> Option<PathBuf> {
    let names = bundled_names();
    for dir in dirs {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// A `which cloudflared`, resolved to a full path so diagnostics can name it.
fn cloudflared_on_path() -> Option<PathBuf> {
    let name = format!("cloudflared{}", std::env::consts::EXE_SUFFIX);
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

fn push_unique(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if !dirs.iter().any(|existing| existing == &dir) {
        dirs.push(dir);
    }
}

/// Whether Weave supports this operating system and processor architecture.
///
/// Weave itself is portable Rust, but the packages, the bundled `cloudflared`
/// and the path-portability rules are only exercised on these combinations.
pub fn supported_platform() -> bool {
    matches!(
        (std::env::consts::OS, std::env::consts::ARCH),
        ("windows", "x86_64")
            | ("macos", "x86_64")
            | ("macos", "aarch64")
            | ("linux", "x86_64")
            | ("linux", "aarch64")
    )
}

pub fn platform_description() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Read a file only far enough to prove it is readable.
pub fn is_readable(path: &Path) -> bool {
    std::fs::File::open(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_dirs_are_anchored_on_the_executable() {
        let dirs = support_dirs();
        assert!(!dirs.is_empty());
        let exe_dir = exe().and_then(|e| e.parent().map(|p| p.to_path_buf()));
        if let Some(exe_dir) = exe_dir {
            assert_eq!(
                dirs[0], exe_dir,
                "the executable's own directory comes first"
            );
        }
    }

    #[test]
    fn support_dirs_contain_no_duplicates() {
        let dirs = support_dirs();
        let mut seen = std::collections::HashSet::new();
        for dir in &dirs {
            assert!(seen.insert(dir.clone()), "{} listed twice", dir.display());
        }
    }

    #[test]
    fn the_documented_package_layouts_resolve() {
        // Windows: cloudflared.exe sits beside weave.exe.
        let windows = support_dirs_for(Path::new(r"C:\Users\a\AppData\Local\Programs\Weave"));
        assert_eq!(
            windows[0],
            PathBuf::from(r"C:\Users\a\AppData\Local\Programs\Weave")
        );

        // Linux .deb: /usr/bin/weave -> /usr/lib/weave/cloudflared.
        let linux = support_dirs_for(Path::new("/usr/bin"));
        assert!(linux.contains(&PathBuf::from("/usr/lib/weave")));

        // macOS .pkg: /usr/local/bin/weave -> /usr/local/libexec/weave/.
        let macos = support_dirs_for(Path::new("/usr/local/bin"));
        assert!(macos.contains(&PathBuf::from("/usr/local/libexec/weave")));

        // Portable tarball, run in place.
        let tarball = support_dirs_for(Path::new("/opt/weave-linux-x64/bin"));
        assert!(tarball.contains(&PathBuf::from("/opt/weave-linux-x64/lib/weave")));
    }

    #[test]
    fn the_architecture_qualified_name_is_preferred() {
        let names = bundled_names();
        assert_eq!(
            names[0],
            format!(
                "cloudflared-{}{}",
                std::env::consts::ARCH,
                std::env::consts::EXE_SUFFIX
            )
        );
        assert_eq!(
            names[1],
            format!("cloudflared{}", std::env::consts::EXE_SUFFIX)
        );
    }

    #[test]
    fn nothing_is_found_in_an_empty_installation() {
        let empty = std::env::temp_dir().join("weave-empty-install-probe");
        assert!(bundled_cloudflared_in(&[empty]).is_none());
    }
}
