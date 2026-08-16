// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for a one-machine session: capture, Tasks, Git
//! publication, and the separation between live state and published state.

mod common;

use common::*;
use std::time::Duration;

#[test]
fn host_captures_edits_and_publishes_a_historical_revision() {
    let sandbox = Sandbox::new("single");
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");

    host.start_daemon(&["host", "--local"]);
    host.wait_online(LONG);

    let status = host.status();
    assert_eq!(status["role"], "host");
    assert_eq!(status["branch"], "main");
    assert_eq!(status["live_revision"], 0);

    // ---- a new file is captured without any Weave-specific action ----
    write_file(&host.repo, "slides/02-market.md", "Market\n");
    host.wait_for_status("the new file to be captured", SHORT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) >= 1 && v["outbox_pending"] == 0
    });

    // ---- ignored content never enters canonical state ----
    write_file(&host.repo, "node_modules/pkg/index.js", "ignored\n");
    std::thread::sleep(Duration::from_millis(800));
    let status = host.status();
    let files_before = status["file_count"].as_u64().unwrap();

    // ---- Tasks ----
    host.expect(&[
        "task",
        "start",
        "--description",
        "Rewrite the market slide",
        "--file",
        "slides/02-market.md:1-20",
    ]);
    let tasks = host.json(&["task", "list"]);
    let active: Vec<&serde_json::Value> = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["status"] == "active")
        .collect();
    assert_eq!(active.len(), 1);
    let task_id = active[0]["id"].as_str().unwrap().to_string();

    write_file(&host.repo, "slides/02-market.md", "Market\nGrowing fast\n");
    host.wait_for_status("the task edit to be accepted", SHORT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) >= 2 && v["outbox_pending"] == 0
    });

    // An active Task that contributed revisions blocks publication by default.
    let blocked = host.json_allow_failure(&["commit", "prepare"]);
    assert!(blocked.is_err(), "an active Task must block preparation");
    let message = blocked.unwrap_err();
    assert!(
        message.contains("Cannot prepare Git publication"),
        "unexpected rejection: {message}"
    );

    host.expect(&["task", "complete", &task_id]);

    // ---- commit preparation binds an immutable target revision ----
    let prepare = host.json(&["commit", "prepare"]);
    let prepare_id = prepare["prepare_id"].as_str().unwrap().to_string();
    let target_revision = prepare["target_revision"].as_u64().unwrap();
    assert!(target_revision >= 2);
    assert_eq!(prepare["previous_published_revision"], 0);
    let added: Vec<String> = prepare["diff_summary"]["added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(added.contains(&"slides/02-market.md".to_string()));
    assert!(
        !prepare["included_task_ids"].as_array().unwrap().is_empty(),
        "the completed Task should be reported as included"
    );

    // ---- live state continues past the prepared revision ----
    write_file(&host.repo, "slides/03-later.md", "After the barrier\n");
    host.wait_for_status("post-preparation work to be accepted", SHORT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) > target_revision && v["outbox_pending"] == 0
    });
    let live_revision = host.status()["live_revision"].as_u64().unwrap();

    // ---- create the Git publication ----
    let publication = host.json(&[
        "commit",
        "create",
        &prepare_id,
        "--message",
        "docs: add the market slide",
    ]);
    let commit_oid = publication["descriptor"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        publication["descriptor"]["target_revision"]
            .as_u64()
            .unwrap(),
        target_revision
    );

    // The Git commit represents the prepared revision, not the live tree.
    let head = git(&host.repo, &["rev-parse", "HEAD"]);
    assert_eq!(head, commit_oid);
    let listed = git(&host.repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(listed.contains("slides/02-market.md"));
    assert!(
        !listed.contains("slides/03-later.md"),
        "the published tree must represent r{target_revision}, not the live tree:\n{listed}"
    );

    // Working tree keeps the newer live state, and Git reports it as pending.
    assert_eq!(
        read_file(&host.repo, "slides/03-later.md"),
        "After the barrier\n"
    );
    let pending = git(&host.repo, &["status", "--porcelain"]);
    assert!(
        pending.contains("slides/03-later.md"),
        "changes after the published revision must remain visible as uncommitted work:\n{pending}"
    );
    assert!(
        !pending.contains("slides/02-market.md"),
        "published content must not still look uncommitted:\n{pending}"
    );

    // The published revision is behind the live revision, as designed.
    let status = host.status();
    assert_eq!(status["published_revision"], target_revision);
    assert_eq!(status["live_revision"], live_revision);
    assert!(status["revisions_ahead"].as_u64().unwrap() >= 1);
    assert_eq!(status["conflicts_open"], 0);
    assert!(status["file_count"].as_u64().unwrap() >= files_before);

    // Ignored files never became canonical.
    let listed = git(&host.repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!listed.contains("node_modules"));

    // ---- deletions propagate ----
    std::fs::remove_file(host.repo.join("slides/03-later.md")).unwrap();
    host.wait_for_status("the deletion to be captured", SHORT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) > live_revision && v["outbox_pending"] == 0
    });

    host.stop_daemon();

    // ---- the repository is an ordinary Git repository without Weave ----
    let log = git(&host.repo, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 2, "{log}");
    std::fs::remove_dir_all(host.repo.join(".git").join("weave")).unwrap();
    let (ok, output) = git_allow_failure(&host.repo, &["status"]);
    assert!(
        ok,
        "Git must remain usable after Weave is removed: {output}"
    );
    let (ok, output) = git_allow_failure(&host.repo, &["fsck", "--no-progress"]);
    assert!(ok, "Git object database must stay intact: {output}");
}

#[test]
fn resume_recovers_local_work_captured_while_the_daemon_was_down() {
    let sandbox = Sandbox::new("resume");
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");

    host.start_daemon(&["host", "--local"]);
    host.wait_online(LONG);
    write_file(&host.repo, "slides/01-intro.md", "L1\nL2 edited\nL3\n");
    host.wait_for_status("the first edit", SHORT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) >= 1 && v["outbox_pending"] == 0
    });
    let revision = host.status()["live_revision"].as_u64().unwrap();
    host.stop_daemon();

    // Edited with no Weave process running at all.
    write_file(&host.repo, "slides/01-intro.md", "L1\nL2 offline\nL3\n");
    write_file(&host.repo, "offline-note.md", "written while stopped\n");

    host.start_daemon(&["resume"]);
    host.wait_online(LONG);
    host.wait_for_status("offline work to be captured on resume", SHORT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) >= revision + 2 && v["outbox_pending"] == 0
    });

    assert_eq!(
        read_file(&host.repo, "slides/01-intro.md"),
        "L1\nL2 offline\nL3\n"
    );
    assert!(file_exists(&host.repo, "offline-note.md"));

    let recovered = host.json(&["recover"]);
    assert_eq!(recovered["healthy"], true, "{recovered:#?}");

    host.stop_daemon();
}

#[test]
fn leaving_and_hosting_again_starts_a_fresh_session_from_r0() {
    let sandbox = Sandbox::new("relive");
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");

    host.start_daemon(&["host", "--local"]);
    host.wait_online(LONG);
    let first_session = host.status()["session_id"].as_str().unwrap().to_string();

    write_file(&host.repo, "slides/02-market.md", "Market\n");
    host.wait_for_status("the edit", SHORT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) >= 1 && v["outbox_pending"] == 0
    });

    // Publish so the working tree is clean again, then end the session.
    let prepare = host.json(&["commit", "prepare"]);
    let prepare_id = prepare["prepare_id"].as_str().unwrap().to_string();
    host.expect(&["commit", "create", &prepare_id, "--message", "docs: market"]);
    host.leave_session();

    // A brand new session must not inherit the previous revision numbering.
    host.start_daemon(&["host", "--local"]);
    host.wait_online(LONG);
    let status = host.status();
    assert_ne!(status["session_id"].as_str().unwrap(), first_session);
    assert_eq!(status["live_revision"], 0);
    assert_eq!(status["published_revision"], 0);
    assert_eq!(status["outbox_pending"], 0);
    assert!(file_exists(&host.repo, "slides/02-market.md"));

    // And it still tracks live edits normally.
    write_file(&host.repo, "slides/02-market.md", "Market\nStill working\n");
    host.wait_for_status("an edit in the new session", SHORT, |v| {
        v["live_revision"].as_u64().unwrap_or(0) >= 1 && v["outbox_pending"] == 0
    });

    host.stop_daemon();
}

#[test]
fn doctor_and_bootstrap_work_without_a_session() {
    let sandbox = Sandbox::new("doctor");
    let participant = Participant::new(&sandbox, "alpha");
    init_repo(&participant.repo, "Quentin", "quentin@example.com");

    let report = participant.json(&["doctor"]);
    assert_eq!(report["ready"], true, "{report:#?}");

    let status = participant.json(&["status"]);
    assert_eq!(status["active"], false);

    let bootstrap = participant.json(&["agent", "bootstrap"]);
    assert_eq!(bootstrap["created"], true);
    let agents = read_file(&participant.repo, "AGENTS.md");
    assert!(agents.contains("<!-- weave:begin -->"));
    assert!(agents.contains("weave status --json"));

    // Re-running is idempotent and leaves unrelated text alone.
    write_file(
        &participant.repo,
        "AGENTS.md",
        &format!("# House rules\n\n{agents}"),
    );
    let bootstrap = participant.json(&["agent", "bootstrap"]);
    assert_eq!(bootstrap["updated"], false);
    let agents = read_file(&participant.repo, "AGENTS.md");
    assert!(agents.starts_with("# House rules"));
    assert_eq!(agents.matches("<!-- weave:begin -->").count(), 1);
}

#[test]
fn a_dirty_repository_cannot_start_a_session() {
    let sandbox = Sandbox::new("dirty");
    let participant = Participant::new(&sandbox, "alpha");
    init_repo(&participant.repo, "Quentin", "quentin@example.com");
    write_file(&participant.repo, "README.md", "# Deck\n\nuncommitted\n");

    let result = participant.run(&["host", "--local"]);
    let message = result.expect_err("a dirty working tree must be refused");
    assert!(
        message.contains("working tree is not clean"),
        "unexpected error: {message}"
    );
}
