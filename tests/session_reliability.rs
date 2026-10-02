// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
mod common;
use common::*;
use std::time::Duration;
const WAIT: Duration = Duration::from_secs(25);

#[test]
fn unresponsive_local_daemon_reports_a_timeout_not_a_filesystem_error() {
    use std::io::{BufRead, BufReader};
    use weave::session::{DaemonLock, Paths, Runtime};

    let sandbox = Sandbox::new("ipc-timeout");
    let host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    let paths = Paths::discover(&host.repo).unwrap();
    paths.ensure().unwrap();
    let _lock = DaemonLock::acquire(&paths).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    weave::session::write_runtime(
        &paths,
        &Runtime {
            pid: std::process::id(),
            port: listener.local_addr().unwrap().port(),
            token: "test-token".into(),
            role: "host".into(),
            session_id: uuid::Uuid::new_v4(),
            started_at_ms: 0,
        },
    )
    .unwrap();
    // The socket accepts the request, but the engine never replies. Keep the
    // listener alive until the CLI exits so this exercises the read deadline.
    let output = weave_command(&host.repo, &host.home)
        .args(["status", "--json"])
        .output()
        .unwrap();
    let (stream, _) = listener.accept().unwrap();
    let mut request = String::new();
    BufReader::new(stream).read_line(&mut request).unwrap();
    assert!(request.contains("status"));
    assert!(!output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["daemon_state"], "unknown");
    assert!(status.get("active").is_none());
    let detail = status["detail"].as_str().unwrap();
    assert!(detail.contains("did not respond within"), "{detail}");
    assert!(detail.contains("busy or blocked"), "{detail}");
    assert!(!detail.contains("Filesystem error"), "{detail}");
}

#[test]
fn npm_watcher_burst_does_not_block_participant_sync() {
    let sandbox = Sandbox::new("ignored-watcher-burst");
    let mut host = Participant::new(&sandbox, "host");
    let mut guest = Participant::new(&sandbox, "guest");
    init_repo(&host.repo, "Host", "host@example.com");
    git(
        &host.repo,
        &[
            "clone",
            "-q",
            host.repo.to_str().unwrap(),
            guest.repo.to_str().unwrap(),
        ],
    );
    git(&guest.repo, &["config", "user.name", "Guest"]);
    git(&guest.repo, &["config", "user.email", "guest@example.com"]);
    let dependencies = guest.repo.join("node_modules/dependency/dist");
    std::fs::create_dir_all(&dependencies).unwrap();
    let files: Vec<_> = (0..4_000)
        .map(|i| dependencies.join(format!("generated-{}-{i:04}.js", "x".repeat(80))))
        .collect();
    for file in &files {
        std::fs::write(file, "before npm\n").unwrap();
    }
    host.start_daemon(&["host", "--lan"]);
    host.wait_online(WAIT);
    let invitation = sandbox.root.join("invite.txt");
    std::fs::write(
        &invitation,
        host.json(&["invite"])["invite"].as_str().unwrap(),
    )
    .unwrap();
    guest.start_daemon(&["join", "--invite-file", invitation.to_str().unwrap()]);
    guest.wait_for_status("initial sync", WAIT, |v| v["synchronized"] == true);
    for file in &files {
        std::fs::write(file, "after npm\n").unwrap();
    }
    // Let the watcher deliver the npm batch before the real collaborative edits.
    std::thread::sleep(Duration::from_secs(1));
    write_file(&host.repo, "from-host.md", "host edit\n");
    write_file(&guest.repo, "from-guest.md", "guest edit\n");
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let h = host.json_allow_failure(&["status"]).unwrap_or_default();
        let g = guest.json_allow_failure(&["status"]).unwrap_or_default();
        if h["synchronized"] == true
            && g["synchronized"] == true
            && h["state"] == g["state"]
            && read_file(&host.repo, "from-host.md") == "host edit\n"
            && std::fs::read_to_string(host.repo.join("from-guest.md"))
                .ok()
                .as_deref()
                == Some("guest edit\n")
            && std::fs::read_to_string(guest.repo.join("from-host.md"))
                .ok()
                .as_deref()
                == Some("host edit\n")
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // A deadlocked engine cannot process stop, so terminate only this
            // disposable test daemon before failing (never stall the suite).
            guest.kill_daemon();
            host.kill_daemon();
            panic!("sync stalled after ignored watcher events: host={h}, guest={g}");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    assert!(!host.repo.join("node_modules").exists());
    for file in &files {
        assert_eq!(std::fs::read_to_string(file).unwrap(), "after npm\n");
    }
    guest.stop_daemon();
    host.stop_daemon();
}

#[test]
fn leave_without_daemon_is_durable_idempotent_and_recoverable() {
    let sandbox = Sandbox::new("offline-leave");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    host.start_daemon(&["host", "--local"]);
    host.wait_online(WAIT);
    write_file(&host.repo, "work.md", "preserve me\n");
    host.wait_for_status("captured work", WAIT, |v| {
        v["outbox_pending"] == 0 && v["live_revision"].as_u64().unwrap_or(0) > 0
    });
    host.stop_daemon();
    assert_eq!(host.status()["session_saved"], true);
    assert_eq!(host.json(&["leave"])["left"], true);
    assert_eq!(host.status()["session_saved"], false);
    assert_eq!(host.json(&["leave"])["left"], true);
    assert_eq!(read_file(&host.repo, "work.md"), "preserve me\n");
    let backups = host.json(&["recover", "--list"]);
    assert!(backups
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["reason"] == "leave"));
}

#[test]
fn excluded_offline_outbox_converges_with_scoped_recovery_archives() {
    let sandbox = Sandbox::new("excluded-offline-outbox");
    let mut host = Participant::new(&sandbox, "host");
    let mut guest = Participant::new(&sandbox, "guest");
    init_repo(&host.repo, "Host", "host@example.com");
    // A session blob unrelated to the excluded files: the old implementation
    // copied it into every archive and could generate gigabytes during resume.
    let unrelated = blob_bytes(51, 2 * 1024 * 1024);
    write_bytes(&host.repo, "asset.bin", &unrelated);
    git(&host.repo, &["add", "asset.bin"]);
    git(&host.repo, &["commit", "-qm", "Asset"]);
    git(
        &host.repo,
        &[
            "clone",
            "-q",
            host.repo.to_str().unwrap(),
            guest.repo.to_str().unwrap(),
        ],
    );
    git(&guest.repo, &["config", "user.name", "Guest"]);
    git(&guest.repo, &["config", "user.email", "guest@example.com"]);
    host.start_daemon(&["host", "--lan"]);
    host.wait_online(WAIT);
    let invitation = sandbox.root.join("invite.txt");
    std::fs::write(
        &invitation,
        host.json(&["invite"])["invite"].as_str().unwrap(),
    )
    .unwrap();
    guest.start_daemon(&["join", "--invite-file", invitation.to_str().unwrap()]);
    guest.wait_for_status("initial sync", WAIT, |v| v["synchronized"] == true);
    host.stop_daemon();
    const COUNT: u64 = 24;
    for i in 0..COUNT {
        write_file(
            &guest.repo,
            &format!("generated/{i}.txt"),
            &format!("offline candidate {i}\n"),
        );
    }
    guest.wait_for_status("offline files captured", WAIT, |v| {
        v["outbox_pending"] == COUNT
    });
    guest.stop_daemon();
    write_file(&host.repo, ".gitignore", "node_modules/\ngenerated/\n");
    host.start_daemon(&["resume"]);
    host.wait_for_status("new shared ignore rule", WAIT, |v| {
        v["synchronized"] == true
    });
    // macOS/Windows can keep the old port reserved after shutdown. The host
    // then advertises a new port, so reconnect using its current invitation
    // while retaining this participant's saved outbox and session identity.
    std::fs::write(
        &invitation,
        host.json(&["invite"])["invite"].as_str().unwrap(),
    )
    .unwrap();
    guest.start_daemon(&["join", "--invite-file", invitation.to_str().unwrap()]);
    guest.wait_for_status("excluded outbox drained", WAIT, |v| {
        v["synchronized"] == true && v["outbox_pending"] == 0
    });
    write_file(
        &guest.repo,
        "after-resume.md",
        "collaboration still works\n",
    );
    host.wait_for_file("after-resume.md", "collaboration still works\n", WAIT);
    guest.stop_daemon();
    host.stop_daemon();

    let backups = guest.json(&["recover", "--list"]);
    let mut archived_paths = std::collections::BTreeSet::new();
    let mut bytes = 0;
    for backup in backups
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["reason"] == "excluded-candidates")
    {
        let root = guest
            .repo
            .join(".git/weave/backups")
            .join(backup["id"].as_str().unwrap());
        let saved = weave::store_client::ClientStore::open(&root.join("state.sqlite")).unwrap();
        let blobs = weave::blobs::BlobStore::open(root.join("blobs")).unwrap();
        bytes += blobs.stats().unwrap().1;
        for (path, state) in saved.all_states().unwrap() {
            let entry = state.in_flight.unwrap().desired.unwrap();
            assert_eq!(
                std::fs::read(blobs.path_of(&entry.blob_hash).unwrap()).unwrap(),
                std::fs::read(path.to_fs_path(&guest.repo)).unwrap()
            );
            archived_paths.insert(path.to_string());
        }
    }
    assert_eq!(archived_paths.len() as u64, COUNT);
    assert!(
        bytes < unrelated.len() as u64,
        "excluded recovery copied {bytes} bytes of unrelated content"
    );
    assert!(!host.repo.join("generated").exists());
    for i in 0..COUNT {
        assert_eq!(
            read_file(&guest.repo, &format!("generated/{i}.txt")),
            format!("offline candidate {i}\n")
        );
    }
}

#[test]
fn invalid_runtime_never_reports_inactive() {
    let sandbox = Sandbox::new("bad-runtime");
    let host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    std::fs::create_dir_all(host.repo.join(".git/weave")).unwrap();
    std::fs::write(host.repo.join(".git/weave/runtime.json"), "truncated").unwrap();
    assert!(host.json_allow_failure(&["status"]).is_err());
    // A confirmed free lock still lets the user detach safely, even though a
    // malformed runtime cannot serve as evidence of a stopped daemon.
    assert_eq!(host.json(&["leave"])["left"], true);
    assert_eq!(host.status()["daemon_state"], "stopped");
}

#[test]
fn leave_preserves_staged_bytes_after_git_garbage_collection() {
    let sandbox = Sandbox::new("staged-backup");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    host.start_daemon(&["host", "--local"]);
    host.wait_online(WAIT);
    host.stop_daemon();
    write_file(&host.repo, "README.md", "staged version\n");
    git(&host.repo, &["add", "README.md"]);
    let staged = git(&host.repo, &["rev-parse", ":README.md"]);
    write_file(&host.repo, "README.md", "unstaged version\n");
    let departure = host.json(&["leave"]);
    let id = departure["backup"].as_str().unwrap();
    git(&host.repo, &["read-tree", "HEAD"]);
    git(&host.repo, &["prune", "--expire", "now"]);
    assert!(!git_allow_failure(&host.repo, &["cat-file", "-e", &staged]).0);
    let exported = sandbox.root.join("exported");
    host.expect(&[
        "recover",
        "--backup",
        id,
        "--export",
        exported.to_str().unwrap(),
    ]);
    assert_eq!(
        read_file(&exported, &format!("index-objects/{staged}")),
        "staged version\n"
    );
    assert_eq!(
        read_file(&exported, "working-tree/README.md"),
        "unstaged version\n"
    );
    assert_eq!(read_file(&host.repo, "README.md"), "unstaged version\n");
}

#[test]
fn interrupted_adoption_replays_only_uncommitted_revisions() {
    for interrupt in ["revision", "state"] {
        let sandbox = Sandbox::new("adoption-interrupted");
        let mut host = Participant::new(&sandbox, "host");
        init_repo(&host.repo, "Host", "host@example.com");
        host.start_daemon(&["host", "--local"]);
        host.wait_online(WAIT);
        let session = host.status()["session_id"].clone();
        host.stop_daemon();
        write_file(&host.repo, "a-external.md", "first\n");
        write_file(&host.repo, "z-external.md", "second\n");
        git(&host.repo, &["add", "a-external.md", "z-external.md"]);
        git(&host.repo, &["commit", "-qm", "External two files"]);
        let target = git(&host.repo, &["rev-parse", "HEAD"]);
        let database = host.repo.join(".git/weave/host.sqlite");
        let conn = rusqlite::Connection::open(&database).unwrap();
        // Fail at a durable boundary, without giving the daemon an opportunity
        // to finish the adoption. The next process must use its saved journal.
        let trigger = if interrupt == "revision" {
            "CREATE TRIGGER interrupt BEFORE INSERT ON revisions WHEN NEW.path='z-external.md' BEGIN SELECT RAISE(FAIL, 'injected interruption'); END;"
        } else {
            "CREATE TRIGGER interrupt BEFORE INSERT ON meta_v4 WHEN NEW.key='git_state' BEGIN SELECT RAISE(FAIL, 'injected interruption'); END;"
        };
        conn.execute_batch(trigger).unwrap();
        drop(conn);
        let (ok, output) = host.run_until_exit(&["resume"], WAIT);
        assert!(!ok, "{output}");
        let conn = rusqlite::Connection::open(&database).unwrap();
        let journal: String = conn
            .query_row(
                "SELECT value FROM meta_v4 WHERE key='pending_adoption'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(journal.contains(&target));
        let accepted: String = conn
            .query_row(
                "SELECT operation_id FROM revisions WHERE path='a-external.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute_batch("DROP TRIGGER interrupt").unwrap();
        drop(conn);
        host.start_daemon(&["resume"]);
        host.wait_online(WAIT);
        assert_eq!(host.status()["session_id"], session);
        assert_eq!(host.status()["git_state"]["commit"], target);
        assert_eq!(host.status()["unpublished_changes"], false);
        host.stop_daemon();
        let conn = rusqlite::Connection::open(&database).unwrap();
        let revisions: i64 = conn
            .query_row("SELECT count(*) FROM revisions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(revisions, 2);
        let persisted: String = conn
            .query_row(
                "SELECT operation_id FROM revisions WHERE path='a-external.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(persisted, accepted);
        assert_eq!(read_file(&host.repo, "a-external.md"), "first\n");
        assert_eq!(read_file(&host.repo, "z-external.md"), "second\n");
    }
}

#[test]
fn external_commit_and_rewind_resume_the_same_session_with_live_work() {
    let sandbox = Sandbox::new("external-adoption");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    let original = git(&host.repo, &["rev-parse", "HEAD"]);
    host.start_daemon(&["host", "--local"]);
    host.wait_online(WAIT);
    let session = host.status()["session_id"].clone();
    write_file(&host.repo, "live.md", "unpublished\n");
    host.wait_for_status("live work", WAIT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) > 0 && v["outbox_pending"] == 0
    });
    let revision = host.status()["live_revision"].as_u64().unwrap();
    host.stop_daemon();
    write_file(&host.repo, "external.md", "external\n");
    git(&host.repo, &["add", "external.md"]);
    git(&host.repo, &["commit", "-qm", "External Git commit"]);
    let external = git(&host.repo, &["rev-parse", "HEAD"]);
    host.start_daemon(&["resume"]);
    host.wait_online(WAIT);
    assert_eq!(host.status()["session_id"], session);
    assert_eq!(host.status()["git_state"]["commit"], external);
    assert_eq!(host.status()["unpublished_changes"], true);
    assert!(host.status()["live_revision"].as_u64().unwrap() > revision);
    assert_eq!(read_file(&host.repo, "live.md"), "unpublished\n");
    host.stop_daemon();
    git(&host.repo, &["reset", "--hard", &original]);
    host.start_daemon(&["resume"]);
    host.wait_online(WAIT);
    assert_eq!(host.status()["session_id"], session);
    assert_eq!(host.status()["git_state"]["commit"], original);
    assert_eq!(read_file(&host.repo, "live.md"), "unpublished\n");
    let prep = host.json(&["commit", "prepare"]);
    assert_eq!(prep["parent_commit_oid"], original);
    host.stop_daemon();
}

#[test]
fn join_aligns_missing_git_objects_and_backs_up_local_work() {
    let sandbox = Sandbox::new("negotiated-join");
    let mut host = Participant::new(&sandbox, "host");
    let mut guest = Participant::new(&sandbox, "guest");
    init_repo(&host.repo, "Host", "host@example.com");
    git(
        &host.repo,
        &[
            "clone",
            "-q",
            host.repo.to_str().unwrap(),
            guest.repo.to_str().unwrap(),
        ],
    );
    git(&guest.repo, &["config", "user.name", "Guest"]);
    git(&guest.repo, &["config", "user.email", "guest@example.com"]);
    // The observed client HEAD is not a parent of the announced host commit.
    // It must remain recoverable even though the working tree could be clean.
    write_file(&guest.repo, "private.md", "local commit only\n");
    git(&guest.repo, &["add", "private.md"]);
    git(&guest.repo, &["commit", "-qm", "Private client commit"]);
    let private_commit = git(&guest.repo, &["rev-parse", "HEAD"]);
    write_file(&guest.repo, "README.md", "local candidate\n");
    write_file(&host.repo, "new.md", "host commit\n");
    git(&host.repo, &["add", "new.md"]);
    git(&host.repo, &["commit", "-qm", "New host commit"]);
    let target = git(&host.repo, &["rev-parse", "HEAD"]);
    host.start_daemon(&["host", "--lan"]);
    host.wait_online(WAIT);
    let invite = host.json(&["invite"]);
    let path = sandbox.root.join("invite.txt");
    std::fs::write(&path, invite["invite"].as_str().unwrap()).unwrap();
    guest.start_daemon(&[
        "join",
        "--invite-file",
        path.to_str().unwrap(),
        "--local-changes=backup",
    ]);
    guest.wait_online(WAIT);
    guest.wait_for_git(&["rev-parse", "HEAD"], &target, WAIT);
    assert!(git(
        &guest.repo,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/weave/recovery/"
        ]
    )
    .contains(&private_commit));
    guest.wait_for_file("new.md", "host commit\n", WAIT);
    guest.wait_for_file("README.md", "# Deck\n\nIntro line\n", WAIT);
    let backups = guest.json(&["recover", "--list"]);
    let id = backups
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["reason"] == "first-join-local-work")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    assert_eq!(
        read_file(
            &guest.repo.join(".git/weave/backups").join(id),
            "working-tree/README.md"
        ),
        "local candidate\n"
    );
    assert_eq!(read_file(&host.repo, "README.md"), "# Deck\n\nIntro line\n");
    let updated = host.json(&["invite", "refresh"]);
    assert_eq!(updated["session_id"], invite["session_id"]);
    std::fs::write(&path, updated["invite"].as_str().unwrap()).unwrap();
    guest.expect(&["join", "--invite-file", path.to_str().unwrap()]);
    guest.wait_online(WAIT);
    guest.stop_daemon();
    host.stop_daemon();
}

#[test]
fn newly_ignored_canonical_file_stays_local_and_is_not_published() {
    let sandbox = Sandbox::new("shared-exclusion");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    host.start_daemon(&["host", "--local"]);
    host.wait_online(WAIT);
    write_file(&host.repo, "generated.tmp", "keep locally\n");
    host.wait_for_status("temporary file admitted", WAIT, |v| {
        v["file_count"] == 4 && v["outbox_pending"] == 0
    });
    write_file(&host.repo, ".gitignore", "node_modules/\n*.tmp\n");
    host.wait_for_status("temporary file excluded", WAIT, |v| {
        v["file_count"] == 3 && v["outbox_pending"] == 0
    });
    assert_eq!(read_file(&host.repo, "generated.tmp"), "keep locally\n");
    let prep = host.json(&["commit", "prepare"]);
    host.json(&[
        "commit",
        "create",
        prep["prepare_id"].as_str().unwrap(),
        "--message",
        "Ignore generated files",
    ]);
    assert!(!git(&host.repo, &["ls-tree", "--name-only", "HEAD"]).contains("generated.tmp"));
    host.stop_daemon();
}

#[test]
fn offline_participant_reconciles_after_multiple_external_adoptions() {
    let sandbox = Sandbox::new("offline-adoption");
    let mut host = Participant::new(&sandbox, "host");
    let mut guest = Participant::new(&sandbox, "guest");
    init_repo(&host.repo, "Host", "host@example.com");
    git(
        &host.repo,
        &[
            "clone",
            "-q",
            host.repo.to_str().unwrap(),
            guest.repo.to_str().unwrap(),
        ],
    );
    host.start_daemon(&["host", "--lan"]);
    host.wait_online(WAIT);
    let invite = host.json(&["invite"]);
    let path = sandbox.root.join("invite.txt");
    std::fs::write(&path, invite["invite"].as_str().unwrap()).unwrap();
    guest.start_daemon(&["join", "--invite-file", path.to_str().unwrap()]);
    guest.wait_online(WAIT);
    guest.stop_daemon();
    host.stop_daemon();
    write_file(&guest.repo, "slides/01-intro.md", "L1\nL2\nGuest L3\n");
    write_file(&host.repo, "slides/01-intro.md", "Host L1\nL2\nL3\n");
    git(&host.repo, &["add", "slides/01-intro.md"]);
    git(&host.repo, &["commit", "-qm", "External one"]);
    host.start_daemon(&["resume"]);
    host.wait_online(WAIT);
    host.stop_daemon();
    write_file(&host.repo, "second.md", "another commit\n");
    git(&host.repo, &["add", "second.md"]);
    git(&host.repo, &["commit", "-qm", "External two"]);
    let target = git(&host.repo, &["rev-parse", "HEAD"]);
    host.start_daemon(&["resume"]);
    host.wait_online(WAIT);
    guest.start_daemon(&["resume"]);
    guest.wait_online(WAIT);
    host.wait_for_file("slides/01-intro.md", "Host L1\nL2\nGuest L3\n", WAIT);
    guest.wait_for_file("slides/01-intro.md", "Host L1\nL2\nGuest L3\n", WAIT);
    guest.wait_for_git(&["rev-parse", "HEAD"], &target, WAIT);
    guest.stop_daemon();
    host.stop_daemon();
}

#[test]
fn identity_changes_take_effect_without_restarting() {
    let sandbox = Sandbox::new("identity-refresh");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "old@example.com");
    host.start_daemon(&["host", "--local"]);
    host.wait_online(WAIT);
    write_file(&host.repo, "work.md", "new work\n");
    host.wait_for_status("captured work", WAIT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) > 0 && v["outbox_pending"] == 0
    });
    git(&host.repo, &["config", "user.email", "updated@example.com"]);
    let prep = host.json(&["commit", "prepare"]);
    host.json(&[
        "commit",
        "create",
        prep["prepare_id"].as_str().unwrap(),
        "--message",
        "New identity",
    ]);
    assert_eq!(
        git(&host.repo, &["log", "-1", "--format=%ae %ce"]),
        "updated@example.com updated@example.com"
    );
    host.stop_daemon();
}

#[test]
fn schema_one_is_backed_up_and_old_metadata_access_is_refused() {
    let sandbox = Sandbox::new("schema-upgrade");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    host.start_daemon(&["host", "--local"]);
    host.wait_online(WAIT);
    let session = host.status()["session_id"].clone();
    host.stop_daemon();
    for name in ["host.sqlite", "state.sqlite"] {
        let path = host.repo.join(".git/weave").join(name);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("DROP VIEW meta; ALTER TABLE meta_v4 RENAME TO meta; UPDATE meta SET value='1' WHERE key='schema_version';").unwrap();
    }
    host.start_daemon(&["resume"]);
    host.wait_online(WAIT);
    assert_eq!(host.status()["session_id"], session);
    host.stop_daemon();
    let conn = rusqlite::Connection::open(host.repo.join(".git/weave/host.sqlite")).unwrap();
    assert!(conn
        .prepare("SELECT value FROM meta WHERE key='schema_version'")
        .is_err());
    assert!(host
        .json(&["recover", "--list"])
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["reason"] == "schema-v1-migration"));
}

#[test]
fn stopped_host_preserves_staged_work_before_normalizing_the_index() {
    let sandbox = Sandbox::new("staged-resume");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    host.start_daemon(&["host", "--local"]);
    host.wait_online(WAIT);
    let session = host.status()["session_id"].clone();
    host.stop_daemon();
    write_file(&host.repo, "README.md", "staged\n");
    git(&host.repo, &["add", "README.md"]);
    let staged = git(&host.repo, &["rev-parse", ":README.md"]);
    write_file(&host.repo, "README.md", "additional unstaged work\n");
    host.start_daemon(&["resume"]);
    host.wait_online(WAIT);
    host.wait_for_status("local work reconciled", WAIT, |v| {
        v["outbox_pending"] == 0 && v["synchronized"] == true
    });
    assert_eq!(host.status()["session_id"], session);
    assert_eq!(
        read_file(&host.repo, "README.md"),
        "additional unstaged work\n"
    );
    assert!(git(&host.repo, &["diff", "--cached", "--name-only"]).is_empty());
    let backups = host.json(&["recover", "--list"]);
    assert!(backups.as_array().unwrap().iter().any(|backup| {
        host.repo
            .join(".git/weave/backups")
            .join(backup["id"].as_str().unwrap())
            .join("index-objects")
            .join(&staged)
            .exists()
    }));
    host.stop_daemon();
}

#[test]
fn a_failed_endpoint_replacement_keeps_the_saved_invitation() {
    let sandbox = Sandbox::new("endpoint-authentication");
    let mut host = Participant::new(&sandbox, "host");
    let mut guest = Participant::new(&sandbox, "guest");
    init_repo(&host.repo, "Host", "host@example.com");
    git(
        &host.repo,
        &[
            "clone",
            "-q",
            host.repo.to_str().unwrap(),
            guest.repo.to_str().unwrap(),
        ],
    );
    host.start_daemon(&["host", "--lan"]);
    host.wait_online(WAIT);
    let invitation = host.json(&["invite"]);
    let invite_file = sandbox.root.join("invite.txt");
    std::fs::write(&invite_file, invitation["invite"].as_str().unwrap()).unwrap();
    guest.start_daemon(&["join", "--invite-file", invite_file.to_str().unwrap()]);
    guest.wait_online(WAIT);
    guest.stop_daemon();
    let saved = std::fs::read(guest.repo.join(".git/weave/session.json")).unwrap();
    let mut payload =
        weave::session::decode_invite(invitation["invite"].as_str().unwrap()).unwrap();
    // A closed, previously allocated local port gives a deterministic failure.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    payload.url = format!("ws://127.0.0.1:{port}/weave/v2");
    std::fs::write(
        &invite_file,
        weave::session::encode_invite(&payload).unwrap(),
    )
    .unwrap();
    let (ok, _) = guest.run_until_exit(
        &["join", "--invite-file", invite_file.to_str().unwrap()],
        WAIT,
    );
    assert!(!ok);
    assert_eq!(
        std::fs::read(guest.repo.join(".git/weave/session.json")).unwrap(),
        saved
    );
    guest.start_daemon(&["resume"]);
    guest.wait_online(WAIT);
    guest.stop_daemon();
    host.stop_daemon();
}

#[test]
fn join_recovers_after_the_reference_moved_but_alignment_was_not_recorded() {
    let sandbox = Sandbox::new("join-interrupted");
    let mut host = Participant::new(&sandbox, "host");
    let mut guest = Participant::new(&sandbox, "guest");
    init_repo(&host.repo, "Host", "host@example.com");
    git(
        &host.repo,
        &[
            "clone",
            "-q",
            host.repo.to_str().unwrap(),
            guest.repo.to_str().unwrap(),
        ],
    );
    let previous = git(&guest.repo, &["rev-parse", "HEAD"]);
    write_file(&host.repo, "README.md", "New Git state\n");
    git(&host.repo, &["commit", "-am", "New state", "-q"]);
    let target = git(&host.repo, &["rev-parse", "HEAD"]);
    host.start_daemon(&["host", "--lan"]);
    host.wait_online(WAIT);
    let invitation = host.json(&["invite"]);
    let invite_file = sandbox.root.join("invite.txt");
    std::fs::write(&invite_file, invitation["invite"].as_str().unwrap()).unwrap();
    let paths = weave::session::Paths::discover(&guest.repo).unwrap();
    let store = weave::store_client::ClientStore::open(&paths.client_db()).unwrap();
    store.conn().execute_batch("CREATE TRIGGER interrupt BEFORE INSERT ON meta_v4 WHEN NEW.key='git_state' BEGIN SELECT RAISE(FAIL, 'injected interruption'); END;").unwrap();
    drop(store);
    let (ok, output) = guest.run_until_exit(
        &["join", "--invite-file", invite_file.to_str().unwrap()],
        WAIT,
    );
    assert!(!ok, "{output}");
    assert_eq!(git(&guest.repo, &["rev-parse", "HEAD"]), target);
    assert!(git(
        &guest.repo,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/weave/recovery/"
        ]
    )
    .contains(&previous));
    let conn = rusqlite::Connection::open(paths.client_db()).unwrap();
    conn.execute_batch("DROP TRIGGER interrupt").unwrap();
    drop(conn);
    write_file(&guest.repo, "README.md", "work after interruption\n");
    git(&guest.repo, &["add", "README.md"]);
    let staged = git(&guest.repo, &["rev-parse", ":README.md"]);
    guest.start_daemon(&[
        "join",
        "--invite-file",
        invite_file.to_str().unwrap(),
        "--local-changes=backup",
    ]);
    guest.wait_online(WAIT);
    guest.wait_for_file("README.md", "New Git state\n", WAIT);
    assert_eq!(read_file(&host.repo, "README.md"), "New Git state\n");
    let backups = guest.json(&["recover", "--list"]);
    assert!(backups.as_array().unwrap().iter().any(|backup| {
        paths
            .weave_dir
            .join("backups")
            .join(backup["id"].as_str().unwrap())
            .join("index-objects")
            .join(&staged)
            .exists()
    }));
    guest.stop_daemon();
    host.stop_daemon();
}

#[test]
fn an_occupied_lan_port_produces_a_new_invitation_for_the_same_session() {
    let sandbox = Sandbox::new("occupied-port");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    host.start_daemon(&["host", "--lan"]);
    host.wait_online(WAIT);
    let old = host.json(&["invite"]);
    let original = weave::session::decode_invite(old["invite"].as_str().unwrap()).unwrap();
    let port: u16 = original
        .url
        .strip_prefix("ws://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    host.stop_daemon();
    let _occupied = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    host.start_daemon(&["resume"]);
    host.wait_online(WAIT);
    let updated = host.json(&["invite"]);
    let current = weave::session::decode_invite(updated["invite"].as_str().unwrap()).unwrap();
    assert_ne!(current.url, original.url);
    assert_eq!(current.session_id, original.session_id);
    assert_eq!(current.secret, original.secret);
    assert!(current.base_commit.is_empty());
    host.stop_daemon();
}

#[test]
fn external_overlapping_edits_preserve_both_candidates_as_a_conflict() {
    let sandbox = Sandbox::new("adoption-conflict");
    let mut host = Participant::new(&sandbox, "host");
    init_repo(&host.repo, "Host", "host@example.com");
    host.start_daemon(&["host", "--local"]);
    host.wait_online(WAIT);
    write_file(&host.repo, "slides/01-intro.md", "Canonical L1\nL2\nL3\n");
    host.wait_for_status("accepted canonical edit", WAIT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) > 0 && v["outbox_pending"] == 0
    });
    host.stop_daemon();
    write_file(&host.repo, "slides/01-intro.md", "Git L1\nL2\nL3\n");
    git(&host.repo, &["add", "slides/01-intro.md"]);
    git(&host.repo, &["commit", "-qm", "External conflict"]);
    host.start_daemon(&["resume"]);
    host.wait_online(WAIT);
    host.wait_for_status("explicit adoption conflict", WAIT, |v| {
        v["conflicts_open"].as_u64().unwrap_or(0) > 0
    });
    let paths = weave::session::Paths::discover(&host.repo).unwrap();
    let store = weave::store_host::HostStore::open(&paths.host_db()).unwrap();
    let conflict = store.open_conflicts().unwrap().remove(0);
    let blobs = weave::blobs::BlobStore::open(paths.blobs()).unwrap();
    assert_eq!(
        blobs
            .get(&conflict.canonical_entry.unwrap().blob_hash)
            .unwrap(),
        b"Git L1\nL2\nL3\n"
    );
    assert_eq!(
        blobs
            .get(&conflict.incoming_entry.unwrap().blob_hash)
            .unwrap(),
        b"Canonical L1\nL2\nL3\n"
    );
    assert!(!read_file(&host.repo, "slides/01-intro.md").contains("<<<<<<<"));
    host.stop_daemon();
}
