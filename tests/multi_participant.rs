// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for a host plus one remote participant.
//!
//! These cover the correctness requirements of specification sections 177-196:
//! independent concurrent edits merge, overlapping edits become explicit
//! conflicts with both candidates preserved, queued work survives a
//! disconnect, and Git publication distributes exact host-built objects.

mod common;

use common::*;
use std::time::Duration;

struct Session {
    _sandbox: Sandbox,
    host: Participant,
    guest: Participant,
}

fn start_session(label: &str) -> Session {
    let sandbox = Sandbox::new(label);
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");

    // A participant must already possess a checkout; Weave never clones.
    let guest = Participant::new(&sandbox, "beta");
    let out = std::process::Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg("-c")
        .arg("core.autocrlf=false")
        .arg(&host.repo)
        .arg(&guest.repo)
        .output()
        .expect("git clone");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(&guest.repo, &["config", "user.name", "Alice"]);
    git(&guest.repo, &["config", "user.email", "alice@example.com"]);
    git(&guest.repo, &["config", "commit.gpgsign", "false"]);

    host.start_daemon(&["host", "--lan"]);
    host.wait_online(LONG);

    let invite = host.json(&["invite"]);
    let invite_text = invite["invite"].as_str().unwrap().to_string();
    let invite_path = host.repo.parent().unwrap().join("invite.txt");
    std::fs::write(&invite_path, &invite_text).unwrap();

    let mut guest = guest;
    guest.start_daemon(&["join", "--invite-file", invite_path.to_str().unwrap()]);
    guest.wait_online(LONG);
    guest.wait_for_status("the guest to receive canonical state", LONG, |v| {
        v["file_count"].as_u64().unwrap_or(0) >= 3
    });

    Session {
        _sandbox: sandbox,
        host,
        guest,
    }
}

#[test]
fn edits_propagate_in_both_directions() {
    let mut session = start_session("bidir");

    write_file(&session.host.repo, "README.md", "# Deck\n\nHost edit\n");
    session
        .guest
        .wait_for_file("README.md", "# Deck\n\nHost edit\n", LONG);

    write_file(&session.guest.repo, "slides/02-guest.md", "From Alice\n");
    session
        .host
        .wait_for_file("slides/02-guest.md", "From Alice\n", LONG);

    // Deletions are ordinary mutations too (rename = delete + create).
    std::fs::remove_file(session.guest.repo.join("slides/02-guest.md")).unwrap();
    session.host.wait_for_missing("slides/02-guest.md", LONG);

    // Both replicas agree on the deterministic state hash at the same revision.
    let host_status = session
        .host
        .wait_for_status("host to settle", LONG, |v| v["outbox_pending"] == 0);
    let revision = host_status["live_revision"].as_u64().unwrap();
    let guest_status = session
        .guest
        .wait_for_status("guest to catch up", LONG, |v| {
            v["live_revision"].as_u64() == Some(revision) && v["outbox_pending"] == 0
        });
    assert_eq!(host_status["state"], guest_status["state"]);

    session.guest.stop_daemon();
    session.host.stop_daemon();
}

#[test]
fn independent_concurrent_edits_merge_without_a_conflict() {
    let mut session = start_session("merge");

    session
        .guest
        .wait_for_file("slides/01-intro.md", "L1\nL2\nL3\n", LONG);

    // Take the guest offline so the two edits are genuinely concurrent.
    session.guest.stop_daemon();
    write_file(&session.guest.repo, "slides/01-intro.md", "L1\nL2\nGUEST\n");

    write_file(&session.host.repo, "slides/01-intro.md", "HOST\nL2\nL3\n");
    session.host.wait_for_status("the host edit", SHORT, |v| {
        v["outbox_pending"] == 0 && v["live_revision"].as_u64().unwrap_or(0) >= 1
    });

    session.guest.start_daemon(&["resume"]);
    session.guest.wait_online(LONG);

    // Specification section 178: both independent changes survive.
    let merged = "HOST\nL2\nGUEST\n";
    session
        .guest
        .wait_for_file("slides/01-intro.md", merged, LONG);
    session
        .host
        .wait_for_file("slides/01-intro.md", merged, LONG);

    assert_eq!(
        session.host.json(&["conflict", "list"])["open_count"],
        0,
        "a clean three-way merge must not create a conflict"
    );

    session.guest.stop_daemon();
    session.host.stop_daemon();
}

#[test]
fn overlapping_edits_become_an_explicit_conflict_that_can_be_resolved() {
    let mut session = start_session("conflict");
    session
        .guest
        .wait_for_file("slides/01-intro.md", "L1\nL2\nL3\n", LONG);

    session.guest.stop_daemon();
    write_file(
        &session.guest.repo,
        "slides/01-intro.md",
        "L1\nGUEST LINE\nL3\n",
    );

    write_file(
        &session.host.repo,
        "slides/01-intro.md",
        "L1\nHOST LINE\nL3\n",
    );
    session.host.wait_for_status("the host edit", SHORT, |v| {
        v["outbox_pending"] == 0 && v["live_revision"].as_u64().unwrap_or(0) >= 1
    });

    session.guest.start_daemon(&["resume"]);
    session.guest.wait_online(LONG);

    let conflict = session.guest.wait_for_conflict(LONG);
    assert_eq!(conflict["conflict"]["kind"], "text_concurrent_edit");
    assert_eq!(conflict["conflict"]["path"], "slides/01-intro.md");

    // Specification sections 7.4 and 80: canonical state stays clean and never
    // receives generated conflict markers.
    let canonical = read_file(&session.host.repo, "slides/01-intro.md");
    assert_eq!(canonical, "L1\nHOST LINE\nL3\n");
    assert!(!canonical.contains("<<<<<<<"));

    // Specification section 179: both candidates are preserved.
    let candidates = &conflict["candidates"];
    assert_eq!(
        candidates["canonical"].as_str().unwrap(),
        "L1\nHOST LINE\nL3\n"
    );
    assert_eq!(
        candidates["incoming"].as_str().unwrap(),
        "L1\nGUEST LINE\nL3\n"
    );
    assert_eq!(candidates["base"].as_str().unwrap(), "L1\nL2\nL3\n");

    // The rejected candidate is also on disk for a human or agent to read.
    let incoming_file = conflict["candidate_files"]["incoming"].as_str().unwrap();
    assert_eq!(
        std::fs::read_to_string(incoming_file).unwrap(),
        "L1\nGUEST LINE\nL3\n"
    );

    // The guest's working file was restored to canonical only after its own
    // candidate became durable, and it holds no conflict markers.
    session
        .guest
        .wait_for_file("slides/01-intro.md", "L1\nHOST LINE\nL3\n", LONG);

    // While the path is in conflict draft mode, edits are captured but not
    // auto-submitted, so the watcher cannot race the resolution.
    write_file(
        &session.guest.repo,
        "slides/01-intro.md",
        "L1\nHOST LINE and GUEST LINE\nL3\n",
    );
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        read_file(&session.host.repo, "slides/01-intro.md"),
        "L1\nHOST LINE\nL3\n",
        "a conflict draft must not reach canonical state on its own"
    );

    let conflict_id = conflict["conflict"]["id"].as_str().unwrap().to_string();
    session.guest.expect(&["conflict", "resolve", &conflict_id]);

    let resolved = "L1\nHOST LINE and GUEST LINE\nL3\n";
    session
        .host
        .wait_for_file("slides/01-intro.md", resolved, LONG);
    session
        .guest
        .wait_for_file("slides/01-intro.md", resolved, LONG);

    let list = session.host.json(&["conflict", "list"]);
    assert_eq!(list["open_count"], 0);

    session.guest.stop_daemon();
    session.host.stop_daemon();
}

#[test]
fn a_participant_can_request_a_publication_the_host_builds_and_distributes() {
    let mut session = start_session("publish");

    // Tasks describe intent; overlap is reported but never blocks.
    session.guest.expect(&[
        "task",
        "start",
        "--description",
        "Add the guest slide",
        "--file",
        "slides/02-guest.md",
    ]);
    write_file(&session.guest.repo, "slides/02-guest.md", "From Alice\n");
    session
        .host
        .wait_for_file("slides/02-guest.md", "From Alice\n", LONG);

    session.host.expect(&[
        "task",
        "start",
        "--description",
        "Polish the intro",
        "--file",
        "slides/02-guest.md",
    ]);
    let host_tasks = session.host.json(&["task", "list"]);
    let host_task_id = host_tasks["active_task_id"].as_str().unwrap().to_string();
    let shown = session.host.json(&["task", "show", &host_task_id]);
    assert!(
        !shown["overlaps"].as_array().unwrap().is_empty(),
        "an overlapping active Task should be reported: {shown:#?}"
    );

    // An active Task that contributed accepted revisions blocks publication.
    let blocked = session.guest.json_allow_failure(&["commit", "prepare"]);
    assert!(
        blocked.is_err(),
        "active contributing Task must block preparation"
    );

    let guest_tasks = session.guest.json(&["task", "list"]);
    let guest_task_id = guest_tasks["active_task_id"].as_str().unwrap().to_string();
    session.guest.expect(&["task", "complete", &guest_task_id]);
    session.host.expect(&["task", "cancel", &host_task_id]);

    let prepare = session.guest.json(&["commit", "prepare"]);
    let prepare_id = prepare["prepare_id"].as_str().unwrap().to_string();
    let target_revision = prepare["target_revision"].as_u64().unwrap();
    assert!(
        prepare["contributors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["display_name"] == "Alice"),
        "{prepare:#?}"
    );

    // Live work continues past the prepared revision.
    write_file(
        &session.guest.repo,
        "slides/03-after.md",
        "after the barrier\n",
    );
    session
        .host
        .wait_for_file("slides/03-after.md", "after the barrier\n", LONG);

    let publication = session.guest.json(&[
        "commit",
        "create",
        &prepare_id,
        "--message",
        "docs: add the guest slide",
    ]);
    let commit_oid = publication["descriptor"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();
    let tree_oid = publication["descriptor"]["tree_oid"]
        .as_str()
        .unwrap()
        .to_string();

    // The host constructs the commit; the participant installs the exact
    // objects the host produced (specification sections 125, 131, 192).
    assert_eq!(git(&session.host.repo, &["rev-parse", "HEAD"]), commit_oid);
    let deadline = std::time::Instant::now() + LONG;
    loop {
        let head = git(&session.guest.repo, &["rev-parse", "HEAD"]);
        if head == commit_oid {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "guest never installed the publication (head {head}, expected {commit_oid})\n{}",
            session.guest.daemon_output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        git(&session.guest.repo, &["rev-parse", "HEAD^{tree}"]),
        tree_oid
    );
    assert_eq!(
        git(&session.host.repo, &["rev-parse", "HEAD^{tree}"]),
        tree_oid,
        "both machines must hold the identical canonical tree"
    );

    // The published tree is the prepared revision, not the live tree.
    let listed = git(
        &session.guest.repo,
        &["ls-tree", "-r", "--name-only", "HEAD"],
    );
    assert!(listed.contains("slides/02-guest.md"));
    assert!(
        !listed.contains("slides/03-after.md"),
        "publication must represent r{target_revision}:\n{listed}"
    );

    // Post-publication live work is still present and visible to Git.
    assert_eq!(
        read_file(&session.guest.repo, "slides/03-after.md"),
        "after the barrier\n"
    );
    assert!(git(&session.guest.repo, &["status", "--porcelain"]).contains("slides/03-after.md"));

    // The commit records the requesting participant as author.
    let author = git(&session.host.repo, &["log", "-1", "--format=%an <%ae>"]);
    assert_eq!(author, "Alice <alice@example.com>");
    let message = git(&session.host.repo, &["log", "-1", "--format=%B"]);
    assert!(message.contains("docs: add the guest slide"), "{message}");

    let (ok, output) = git_allow_failure(&session.guest.repo, &["fsck", "--no-progress"]);
    assert!(
        ok,
        "participant Git object database must stay intact: {output}"
    );

    session.guest.stop_daemon();
    session.host.stop_daemon();
}

#[test]
fn a_second_daemon_cannot_control_the_same_working_tree() {
    let mut session = start_session("lock");
    let error = session
        .host
        .run(&["host", "--local"])
        .expect_err("a second daemon must be refused");
    assert!(
        error.contains("already running"),
        "unexpected error: {error}"
    );
    session.guest.stop_daemon();
    session.host.stop_daemon();
}
