// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
mod common;
use common::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn large_ignore_batches_finish_without_blocking_git_pipes() {
    const CHILD: &str = "WEAVE_TEST_LARGE_IGNORE_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        // Both input and output exceed even a large OS pipe buffer. No files
        // need to exist: these are the paths a watcher can report during npm.
        let mut names: Vec<String> = (0..50_000)
            .map(|i| format!("node_modules/dependency/dist/generated-file-{i:06}.js"))
            .collect();
        names.push("README.md".into());
        let ignored = weave::gitx::filter_ignored(&root, &names).unwrap();
        assert_eq!(ignored.len(), names.len() - 1);
        assert!(!ignored.contains("README.md"));

        let paths = weave::session::Paths::discover(&root).unwrap();
        let blobs = weave::blobs::BlobStore::open(paths.blobs()).unwrap();
        let head = git(&root, &["rev-parse", "HEAD"]);
        let manifest = weave::gitx::committed_manifest(&root, &head, &blobs).unwrap();
        let shared = weave::gitx::shared_ignored(&paths, &manifest, &blobs, &head, &names).unwrap();
        assert_eq!(shared.len(), ignored.len());
        assert!(!shared.contains("README.md"));
        return;
    }

    let sandbox = Sandbox::new("git-pipe-deadlock");
    init_repo(&sandbox.root, "Host", "host@example.com");
    let log_path = sandbox.root.join("child.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "large_ignore_batches_finish_without_blocking_git_pipes",
            "--nocapture",
        ])
        .env(CHILD, &sandbox.root)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    // Isolate the call so a regression fails instead of hanging the test suite.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{}",
                std::fs::read_to_string(&log_path).unwrap()
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("git check-ignore deadlocked while exchanging a large batch of ignored paths");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn early_git_failure_preserves_its_diagnostic_with_large_stdin() {
    let sandbox = Sandbox::new("git-early-exit");
    let out = weave::gitx::run_stdin(
        &sandbox.root,
        &["weave-nonexistent-subcommand"],
        &vec![b'x'; 2 * 1024 * 1024],
    )
    .unwrap();
    assert!(!out.ok());
    assert!(out.stderr.contains("weave-nonexistent-subcommand"));
}
