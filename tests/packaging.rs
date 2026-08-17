// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the native packages promise, asserted without building one.
//!
//! The release workflow installs the real `.exe`, `.pkg` and `.deb` on real
//! runners; those jobs are the end of the chain. These tests cover the part
//! that would otherwise only fail *after* a release: the pinned cloudflared
//! version drifting away from the packaging scripts, and the installed layout
//! no longer being the layout `weave doctor --install` accepts.
//!
//! The stand-in for cloudflared is a copy of the `weave` binary. That is
//! deliberate: it is a real executable on every platform, it prints a version
//! string, and the bundle manifest written next to it claims that same version
//! — so the discovery, execution and version-cross-check paths all run for
//! real without downloading 40 MB.

mod common;

use common::{weave_bin, Sandbox};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// The pin
// ---------------------------------------------------------------------------

fn pinned_version() -> String {
    let text = read(&repo_root().join("packaging/cloudflared/pinned.env"));
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .find_map(|line| {
            line.trim()
                .strip_prefix("CLOUDFLARED_VERSION=")
                .map(str::to_string)
        })
        .expect("CLOUDFLARED_VERSION in packaging/cloudflared/pinned.env")
}

#[test]
fn the_packaged_cloudflared_version_matches_the_crate() {
    assert_eq!(
        pinned_version(),
        weave::install::CLOUDFLARED_VERSION,
        "packaging/cloudflared/pinned.env and install::CLOUDFLARED_VERSION must agree; \
         weave doctor --install compares the two at run time"
    );
}

#[test]
fn every_platform_asset_is_checksum_pinned() {
    let sums = read(&repo_root().join("packaging/cloudflared/SHA256SUMS"));
    // One per release runner: Windows x64, both macOS architectures, Linux x64.
    for asset in [
        "cloudflared-windows-amd64.exe",
        "cloudflared-darwin-amd64.tgz",
        "cloudflared-darwin-arm64.tgz",
        "cloudflared-linux-amd64",
    ] {
        let line = sums
            .lines()
            .find(|line| line.trim_end().ends_with(asset))
            .unwrap_or_else(|| panic!("{asset} is not pinned in packaging/cloudflared/SHA256SUMS"));
        let digest = line.split_whitespace().next().unwrap();
        assert_eq!(
            digest.len(),
            64,
            "{asset}: {digest} is not a SHA-256 digest"
        );
        assert!(
            digest.chars().all(|c| c.is_ascii_hexdigit()),
            "{asset}: {digest} is not hexadecimal"
        );
    }
}

#[test]
fn the_notice_names_the_version_that_is_actually_bundled() {
    let notice = read(&repo_root().join("packaging/cloudflared/licenses/cloudflared/NOTICE"));
    assert!(
        notice.contains(weave::install::CLOUDFLARED_VERSION),
        "the third-party notice must name the bundled cloudflared release"
    );
    assert!(notice.contains("Apache License"));
    let license = read(&repo_root().join("packaging/cloudflared/licenses/cloudflared/LICENSE"));
    assert!(license.contains("Apache License"));
    assert!(license.contains("Version 2.0"));
}

#[test]
fn every_packaging_entry_point_exists() {
    for script in [
        "packaging/cloudflared/fetch.sh",
        "packaging/bundle-manifest.sh",
        "packaging/windows/build.ps1",
        "packaging/windows/weave.iss",
        "packaging/macos/build-pkg.sh",
        "packaging/macos/distribution.xml",
        "packaging/macos/scripts/postinstall",
        "packaging/linux/build-deb.sh",
        "packaging/linux/build-tarball.sh",
    ] {
        let path = repo_root().join(script);
        assert!(path.is_file(), "{script} is missing");
    }
}

/// The primary asset names are a public contract: the README's download buttons
/// link to `releases/latest/download/<name>` and must not go stale.
#[test]
fn the_release_asset_names_are_the_ones_the_readme_links_to() {
    let readme = read(&repo_root().join("README.md"));
    let workflow = read(&repo_root().join(".github/workflows/release.yml"));
    for asset in [
        "WeaveSetup-x64.exe",
        "Weave-macos-universal.pkg",
        "weave-linux-x64.deb",
    ] {
        assert!(
            readme.contains(&format!("releases/latest/download/{asset}")),
            "the README no longer links to {asset}"
        );
        assert!(
            workflow.contains(asset),
            "the release workflow no longer produces {asset}"
        );
    }
}

/// Every packaging format has its own idea of a legal version string, and a
/// prerelease is where they disagree. Debian wants `0.1.0~rc.1`; the Win32
/// VERSIONINFO resource Inno writes is four numbers and rejects `0.1.0-rc.1`
/// outright, aborting the compile. Both build scripts derive what their format
/// needs, and a release candidate is exactly when a regression here would be
/// discovered by a failed tag rather than by a test.
#[test]
fn every_packaging_format_can_express_a_prerelease_version() {
    let iss = read(&repo_root().join("packaging/windows/weave.iss"));
    assert!(
        iss.contains("VersionInfoVersion={#AppFileVersion}"),
        "weave.iss feeds a raw semver string to Inno's numeric version resource"
    );

    let ps1 = read(&repo_root().join("packaging/windows/build.ps1"));
    assert!(
        ps1.contains("/DAppFileVersion=$fileVersion"),
        "build.ps1 no longer passes a numeric file version to ISCC"
    );

    let deb = read(&repo_root().join("packaging/linux/build-deb.sh"));
    assert!(
        deb.contains(r#"deb_version="${version//-/\~}""#),
        "build-deb.sh no longer converts a prerelease to Debian's `~` form"
    );
}

// ---------------------------------------------------------------------------
// The installed layout
// ---------------------------------------------------------------------------

/// Build a directory tree shaped like a real installation and return the
/// `weave` inside it.
///
/// `layout` picks which of the three documented shapes to build, so the test
/// exercises the same resolution the packages rely on.
fn fake_installation(root: &Path, layout: &str, with_cloudflared: bool) -> PathBuf {
    let exe = format!("weave{}", std::env::consts::EXE_SUFFIX);
    // The macOS package ships one cloudflared per architecture, because
    // Cloudflare publishes no universal build. Using the arch-qualified name in
    // the `libexec` layout is what keeps that selection tested everywhere —
    // there is no macOS runner in `cargo test`.
    let cloudflared = if layout == "libexec" {
        format!(
            "cloudflared-{}{}",
            std::env::consts::ARCH,
            std::env::consts::EXE_SUFFIX
        )
    } else {
        format!("cloudflared{}", std::env::consts::EXE_SUFFIX)
    };

    let (bin_dir, support_dir) = match layout {
        // Windows: everything beside weave.exe.
        "flat" => (root.to_path_buf(), root.to_path_buf()),
        // Linux .deb and the portable tarball.
        "lib" => (root.join("bin"), root.join("lib").join("weave")),
        // macOS .pkg.
        "libexec" => (root.join("bin"), root.join("libexec").join("weave")),
        other => panic!("unknown layout {other}"),
    };
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(support_dir.join("licenses").join("cloudflared")).unwrap();

    let installed = bin_dir.join(&exe);
    std::fs::copy(weave_bin(), &installed).unwrap();

    if with_cloudflared {
        let target = support_dir.join(&cloudflared);
        std::fs::copy(weave_bin(), &target).unwrap();
        make_executable(&target);
    }

    // The stand-in reports the Weave version, so the manifest claims that as
    // the bundled cloudflared version: the point is to exercise the check, and
    // a mismatch here must fail (see `a_mixed_installation_is_reported`).
    std::fs::write(
        support_dir.join("weave-bundle.json"),
        format!(
            r#"{{"weave_version":"{}","cloudflared_version":"{}","package":"test-fixture"}}"#,
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_VERSION")
        ),
    )
    .unwrap();
    std::fs::write(
        support_dir
            .join("licenses")
            .join("cloudflared")
            .join("NOTICE"),
        "cloudflared, Apache-2.0, bundled with Weave.\n",
    )
    .unwrap();
    std::fs::write(
        support_dir
            .join("licenses")
            .join("cloudflared")
            .join("LICENSE"),
        "Apache License, Version 2.0\n",
    )
    .unwrap();

    make_executable(&installed);
    installed
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// Run `weave doctor --install --json` with a clean environment.
///
/// `WEAVE_CLOUDFLARED` is explicitly cleared: a developer machine that sets it
/// would otherwise mask exactly the bug this test exists to catch.
fn install_check(weave: &Path, dir: &Path) -> (bool, Value, String) {
    install_check_with_path(weave, dir, None)
}

/// A PATH with every directory holding a `cloudflared` removed.
///
/// The doctor deliberately accepts a `cloudflared` found on PATH when the
/// bundled one is missing, so a machine with Weave (or cloudflared) actually
/// installed on it would otherwise turn "this package is broken" into "this
/// package is fine, using the one next door" — a true answer to a different
/// question than the test is asking.
fn path_without_cloudflared() -> std::ffi::OsString {
    let name = if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    };
    let current = std::env::var_os("PATH").unwrap_or_default();
    let kept: Vec<PathBuf> = std::env::split_paths(&current)
        .filter(|dir| !dir.join(name).exists())
        .collect();
    std::env::join_paths(kept).expect("rebuild PATH")
}

fn install_check_with_path(
    weave: &Path,
    dir: &Path,
    path: Option<std::ffi::OsString>,
) -> (bool, Value, String) {
    let mut command = Command::new(weave);
    command
        .args(["doctor", "--install", "--json"])
        .current_dir(dir)
        .env_remove("WEAVE_CLOUDFLARED");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let out = command.output().expect("run weave doctor --install");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let json: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --install did not print JSON ({e}):\n{stdout}{stderr}"));
    (out.status.success(), json, stderr)
}

fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no `{name}` check in {report:#}"))
}

#[test]
fn an_installed_layout_passes_the_installation_self_check() {
    for layout in ["flat", "lib", "libexec"] {
        let sandbox = Sandbox::new(&format!("install-{layout}"));
        let weave = fake_installation(&sandbox.root, layout, true);

        // Deliberately run from a directory that is not a Git repository: the
        // installation check must never need one, because the native packages
        // run it from an installer.
        let elsewhere = sandbox.root.join("not-a-repository");
        std::fs::create_dir_all(&elsewhere).unwrap();

        let (ok, report, stderr) = install_check(&weave, &elsewhere);
        assert!(ok, "{layout}: {report:#}\n{stderr}");
        assert_eq!(report["ready"], true, "{layout}: {report:#}");
        assert_eq!(report["scope"], "install", "{layout}: {report:#}");

        // No repository check leaked into the installation report.
        for absent in [
            "Repository",
            "Branch",
            "Working tree clean",
            "Portable paths",
        ] {
            assert!(
                report["checks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|c| c["name"] != absent),
                "{layout}: `{absent}` does not belong in --install: {report:#}"
            );
        }

        // The bundled copy was found, not something on PATH.
        let bundled = check(&report, "Bundled cloudflared");
        assert_eq!(bundled["status"], "pass", "{layout}: {bundled:#}");
        assert!(
            bundled["detail"]
                .as_str()
                .unwrap()
                .contains("bundled with Weave"),
            "{layout}: {bundled:#}"
        );
        assert_eq!(check(&report, "cloudflared runs")["status"], "pass");
        assert_eq!(check(&report, "Weave package")["status"], "pass");
        assert_eq!(check(&report, "Third-party licences")["status"], "pass");
        assert_eq!(check(&report, "Weave executable")["status"], "pass");
        assert_eq!(check(&report, "Platform")["status"], "pass");
    }
}

#[test]
fn a_package_without_its_cloudflared_is_reported_as_broken() {
    let sandbox = Sandbox::new("install-incomplete");
    let weave = fake_installation(&sandbox.root, "lib", false);

    let (ok, report, _) =
        install_check_with_path(&weave, &sandbox.root, Some(path_without_cloudflared()));
    assert!(!ok, "an incomplete package must exit non-zero: {report:#}");
    assert_eq!(report["ready"], false, "{report:#}");

    let bundled = check(&report, "Bundled cloudflared");
    assert_eq!(bundled["status"], "fail", "{bundled:#}");
    assert!(
        bundled["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("Reinstall"),
        "a broken package must say what to do: {bundled:#}"
    );
}

#[test]
fn a_mixed_installation_is_reported() {
    let sandbox = Sandbox::new("install-mixed");
    let weave = fake_installation(&sandbox.root, "lib", true);

    // Rewrite the manifest to claim a Weave version this binary is not.
    std::fs::write(
        sandbox
            .root
            .join("lib")
            .join("weave")
            .join("weave-bundle.json"),
        r#"{"weave_version":"0.0.1-not-this-one","cloudflared_version":"1.2.3","package":"test"}"#,
    )
    .unwrap();

    let (ok, report, _) = install_check(&weave, &sandbox.root);
    assert!(!ok, "{report:#}");
    assert_eq!(
        check(&report, "Weave package")["status"],
        "fail",
        "{report:#}"
    );
}

#[test]
fn a_source_build_is_not_mistaken_for_a_broken_package() {
    // No bundle manifest anywhere: `cargo build`, not an installation. Missing
    // bundled runtime dependencies are a warning, not a failure — `weave host
    // --lan` works perfectly well without cloudflared.
    let sandbox = Sandbox::new("install-source");
    let bin = sandbox.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let weave = bin.join(format!("weave{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(weave_bin(), &weave).unwrap();
    make_executable(&weave);

    let (ok, report, _) = install_check(&weave, &sandbox.root);
    assert!(
        ok,
        "a source build is not a broken installation: {report:#}"
    );
    assert_eq!(
        check(&report, "Weave package")["status"],
        "warn",
        "{report:#}"
    );
}

// ---------------------------------------------------------------------------
// Independence from the build toolchain
// ---------------------------------------------------------------------------

/// A packaged Weave must run on a machine with no Rust toolchain. The binary is
/// statically self-contained apart from the platform C runtime, so the test
/// that means anything is: does it still work when cargo and rustc are not
/// reachable at all?
#[test]
fn a_packaged_weave_runs_without_a_rust_toolchain() {
    let sandbox = Sandbox::new("install-no-cargo");
    let weave = fake_installation(&sandbox.root, "lib", true);

    // A PATH containing only the system directories Git needs — no ~/.cargo/bin
    // and no rustup shims.
    let minimal = if cfg!(windows) {
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        // Git for Windows is not under SystemRoot, so keep the entries that
        // contain git.exe and drop everything else.
        let git_dirs: Vec<String> = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|d| d.join("git.exe").is_file())
            .map(|d| d.display().to_string())
            .collect();
        let mut parts = vec![format!(r"{system}\System32"), system.clone()];
        parts.extend(git_dirs);
        parts.join(";")
    } else {
        "/usr/local/bin:/usr/bin:/bin".to_string()
    };

    let out = Command::new(&weave)
        .args(["doctor", "--install", "--json"])
        .current_dir(&sandbox.root)
        .env("PATH", &minimal)
        .env_remove("WEAVE_CLOUDFLARED")
        .env_remove("CARGO")
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .output()
        .expect("run weave");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let report: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("no JSON ({e}): {stdout}"));

    assert_eq!(
        report["ready"], true,
        "packaged Weave must not need cargo: {report:#}\nPATH was {minimal}"
    );
    assert!(out.status.success());

    // And prove the toolchain really was absent from that PATH.
    let cargo_visible = std::env::split_paths(minimal.as_str()).any(|d| {
        d.join(format!("cargo{}", std::env::consts::EXE_SUFFIX))
            .is_file()
    });
    assert!(!cargo_visible, "the minimal PATH still contains cargo");
}

// ---------------------------------------------------------------------------
// `--version` and help, which the packaging tests on every runner assert
// ---------------------------------------------------------------------------

#[test]
fn the_installed_binary_reports_its_version() {
    let out = Command::new(weave_bin())
        .arg("--version")
        .output()
        .expect("weave --version");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "`weave --version` printed {text:?}"
    );
}

#[test]
fn doctor_help_presents_it_as_troubleshooting_rather_than_setup() {
    let out = Command::new(weave_bin())
        .args(["doctor", "--help"])
        .output()
        .expect("weave doctor --help");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(text.contains("--install"), "{text}");
    assert!(text.contains("troubleshoot"), "{text}");
}
