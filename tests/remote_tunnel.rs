// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opt-in end-to-end test of the real remote path: a Cloudflare Quick Tunnel.
//!
//! This is the only test that leaves the machine. It is `#[ignore]`d so an
//! ordinary `cargo test` never depends on `cloudflared`, on Cloudflare's
//! availability, or on outbound network access.
//!
//! Run it explicitly:
//!
//! ```text
//! cargo test --test remote_tunnel -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Requirements: `cloudflared` on PATH and outbound HTTPS. The participant
//! connects only through the generated `wss://<name>.trycloudflare.com` URL —
//! the test asserts that, so a regression that quietly falls back to loopback
//! or LAN fails here rather than passing silently.

mod common;

use common::*;
use std::time::Duration;

/// Quick Tunnel startup, DNS propagation and the first WebSocket upgrade are
/// all slower than anything on loopback.
const TUNNEL: Duration = Duration::from_secs(180);

fn require_cloudflared() {
    let ok = std::process::Command::new("cloudflared")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        ok,
        "this test needs `cloudflared` on PATH; install it or skip the remote test"
    );
}

fn assert_public_endpoint(endpoint: &str) {
    assert!(
        endpoint.starts_with("wss://"),
        "the remote endpoint must be TLS: {endpoint}"
    );
    assert!(
        endpoint.contains("trycloudflare.com"),
        "the participant must connect through the Quick Tunnel, not loopback or LAN: {endpoint}"
    );
    assert!(
        !endpoint.contains("127.0.0.1") && !endpoint.contains("localhost"),
        "the remote endpoint must not be local: {endpoint}"
    );
}

#[test]
#[ignore = "requires cloudflared and outbound network; run with --ignored"]
fn a_full_session_runs_over_a_real_cloudflare_quick_tunnel() {
    require_cloudflared();

    let sandbox = Sandbox::new("tunnel");
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");

    // The participant must already possess a checkout; Weave never clones.
    let mut guest = Participant::new(&sandbox, "beta");
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

    // ---- 1. host with the default transport: a real Quick Tunnel ----
    host.start_daemon(&["host"]);
    host.wait_online(TUNNEL);

    let invite = host.wait_for_invite(TUNNEL);
    let endpoint = invite["endpoint"].as_str().unwrap().to_string();
    assert_public_endpoint(&endpoint);
    eprintln!("quick tunnel endpoint: {endpoint}");

    // ---- 2. join across the public internet ----
    let invite_path = sandbox.root.join("invite-1.txt");
    std::fs::write(&invite_path, invite["invite"].as_str().unwrap()).unwrap();
    guest.start_daemon(&["join", "--invite-file", invite_path.to_str().unwrap()]);
    guest.wait_online(TUNNEL);
    guest.wait_for_status("the guest to receive canonical state", TUNNEL, |v| {
        v["file_count"].as_u64().unwrap_or(0) >= 3
    });

    // ---- 3. synchronization in both directions ----
    write_file(
        &host.repo,
        "README.md",
        "# Deck\n\nHost edit over the tunnel\n",
    );
    guest.wait_for_file("README.md", "# Deck\n\nHost edit over the tunnel\n", TUNNEL);

    write_file(&guest.repo, "slides/02-guest.md", "From Alice, remotely\n");
    host.wait_for_file("slides/02-guest.md", "From Alice, remotely\n", TUNNEL);

    // A binary asset exercises the base64 payload path over the tunnel.
    let blob: Vec<u8> = (0u32..40_000).map(|i| (i % 251) as u8).collect();
    std::fs::write(guest.repo.join("assets.bin"), &blob).unwrap();
    host.wait_for_bytes("assets.bin", &blob, TUNNEL);

    // Both replicas agree on the deterministic state hash at the same revision.
    let host_status =
        host.wait_for_status("the host to settle", TUNNEL, |v| v["outbox_pending"] == 0);
    let revision = host_status["live_revision"].as_u64().unwrap();
    let guest_status = guest.wait_for_status("the guest to catch up", TUNNEL, |v| {
        v["live_revision"].as_u64() == Some(revision) && v["outbox_pending"] == 0
    });
    assert_eq!(host_status["state"], guest_status["state"]);

    // ---- 4. Tasks and overlap reporting across the tunnel ----
    guest.expect(&[
        "task",
        "start",
        "--description",
        "Add the guest slide",
        "--file",
        "slides/02-guest.md",
    ]);
    host.expect(&[
        "task",
        "start",
        "--description",
        "Polish the guest slide",
        "--file",
        "slides/02-guest.md",
    ]);
    let host_task_id = host.wait_for_active_task(TUNNEL);
    let shown = host.json(&["task", "show", &host_task_id]);
    assert!(
        !shown["overlaps"].as_array().unwrap().is_empty(),
        "an overlapping active Task should be reported: {shown:#?}"
    );

    // The guest's Task contributed accepted revisions, so publication is blocked.
    write_file(
        &guest.repo,
        "slides/02-guest.md",
        "From Alice, remotely\nMore\n",
    );
    host.wait_for_file("slides/02-guest.md", "From Alice, remotely\nMore\n", TUNNEL);
    let blocked = guest.json_allow_failure(&["commit", "prepare"]);
    assert!(
        blocked.is_err(),
        "an active contributing Task must block preparation"
    );

    let guest_task_id = guest.wait_for_active_task(TUNNEL);
    guest.expect(&["task", "complete", &guest_task_id]);
    host.expect(&["task", "cancel", &host_task_id]);

    // ---- 5. disconnect, edit offline, reconnect into a conflict ----
    guest.wait_for_file("slides/01-intro.md", "L1\nL2\nL3\n", TUNNEL);
    guest.stop_daemon();
    write_file(&guest.repo, "slides/01-intro.md", "L1\nGUEST LINE\nL3\n");

    write_file(&host.repo, "slides/01-intro.md", "L1\nHOST LINE\nL3\n");
    host.wait_for_status("the host edit", TUNNEL, |v| v["outbox_pending"] == 0);

    guest.start_daemon(&["resume"]);
    guest.wait_online(TUNNEL);

    let conflict = guest.wait_for_conflict(TUNNEL);
    assert_eq!(conflict["conflict"]["kind"], "text_concurrent_edit");
    let canonical = read_file(&host.repo, "slides/01-intro.md");
    assert_eq!(canonical, "L1\nHOST LINE\nL3\n");
    assert!(!canonical.contains("<<<<<<<"));
    assert_eq!(
        conflict["candidates"]["incoming"].as_str().unwrap(),
        "L1\nGUEST LINE\nL3\n",
        "the rejected candidate must survive the round trip"
    );
    guest.wait_for_file("slides/01-intro.md", "L1\nHOST LINE\nL3\n", TUNNEL);

    let conflict_id = conflict["conflict"]["id"].as_str().unwrap().to_string();
    write_file(
        &guest.repo,
        "slides/01-intro.md",
        "L1\nHOST LINE and GUEST LINE\nL3\n",
    );
    guest.expect(&["conflict", "resolve", &conflict_id]);
    let resolved = "L1\nHOST LINE and GUEST LINE\nL3\n";
    host.wait_for_file("slides/01-intro.md", resolved, TUNNEL);
    guest.wait_for_file("slides/01-intro.md", resolved, TUNNEL);
    assert_eq!(host.json(&["conflict", "list"])["open_count"], 0);

    // ---- 6. participant-requested Git publication over the tunnel ----
    let prepare = guest.json(&["commit", "prepare"]);
    let prepare_id = prepare["prepare_id"].as_str().unwrap().to_string();
    let target_revision = prepare["target_revision"].as_u64().unwrap();

    // Live work continues past the prepared revision.
    write_file(&guest.repo, "slides/03-after.md", "after the barrier\n");
    host.wait_for_file("slides/03-after.md", "after the barrier\n", TUNNEL);

    let publication = guest.json(&[
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

    assert_eq!(git(&host.repo, &["rev-parse", "HEAD"]), commit_oid);
    // The Git pack travelled over the tunnel; the participant installed the
    // exact host-built objects.
    guest.wait_for_git(&["rev-parse", "HEAD"], &commit_oid, TUNNEL);
    assert_eq!(git(&guest.repo, &["rev-parse", "HEAD^{tree}"]), tree_oid);
    let listed = git(&guest.repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(listed.contains("slides/02-guest.md"));
    assert!(
        !listed.contains("slides/03-after.md"),
        "the publication must represent r{target_revision}:\n{listed}"
    );
    assert_eq!(
        read_file(&guest.repo, "slides/03-after.md"),
        "after the barrier\n"
    );
    let (ok, output) = git_allow_failure(&guest.repo, &["fsck", "--no-progress"]);
    assert!(
        ok,
        "participant Git object database must stay intact: {output}"
    );

    // ---- 7. tunnel restart: same session, new URL ----
    let restarted = host.json(&["tunnel", "restart"]);
    let new_endpoint = restarted["endpoint"].as_str().unwrap().to_string();
    assert_public_endpoint(&new_endpoint);
    assert_ne!(
        new_endpoint, endpoint,
        "restarting the tunnel must produce a new public URL"
    );
    assert_eq!(
        restarted["session_id"].as_str().unwrap(),
        host.status()["session_id"].as_str().unwrap(),
        "the logical session must survive a tunnel replacement"
    );
    eprintln!("restarted tunnel endpoint: {new_endpoint}");

    // Canonical state, Tasks and conflicts are untouched by the replacement.
    let after_restart = host.status();
    assert!(after_restart["live_revision"].as_u64().unwrap() > 0);
    assert_eq!(after_restart["conflicts_open"], 0);

    // The old URL is gone, so the guest drops offline and queues local work.
    guest.wait_for_status("the guest to notice the dead tunnel", TUNNEL, |v| {
        v["connection"] == "offline"
    });
    write_file(
        &guest.repo,
        "slides/04-offline.md",
        "queued while the tunnel was replaced\n",
    );

    // Re-join with the newly published invite; queued work must not disappear.
    guest.stop_daemon();
    let invite_path = sandbox.root.join("invite-2.txt");
    std::fs::write(&invite_path, restarted["invite"].as_str().unwrap()).unwrap();
    guest.start_daemon(&["join", "--invite-file", invite_path.to_str().unwrap()]);
    guest.wait_online(TUNNEL);
    host.wait_for_file(
        "slides/04-offline.md",
        "queued while the tunnel was replaced\n",
        TUNNEL,
    );

    // ---- 8. converge and shut down cleanly ----
    let host_status =
        host.wait_for_status("the host to settle", TUNNEL, |v| v["outbox_pending"] == 0);
    let revision = host_status["live_revision"].as_u64().unwrap();
    let guest_status = guest.wait_for_status("the guest to converge", TUNNEL, |v| {
        v["live_revision"].as_u64() == Some(revision) && v["outbox_pending"] == 0
    });
    assert_eq!(host_status["state"], guest_status["state"]);

    host.assert_daemon_healthy();
    guest.assert_daemon_healthy();
    assert!(
        !host.daemon_output().contains("ReplicaDivergence"),
        "a first join is not a divergence:\n{}",
        host.daemon_output()
    );

    guest.stop_daemon();
    host.stop_daemon();
}
