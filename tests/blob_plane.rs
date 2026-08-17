// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for the blob plane (`docs/BLOB-PLANE.md`, section 7).
//!
//! Everything here is about content that is far too large to have travelled in
//! a JSON control message: file bytes now stream beside the control plane,
//! hash-addressed, and the control plane only ever names them. The properties
//! under test are the ones that make that safe — every participant converges on
//! the same repository state, a transfer that is interrupted installs nothing,
//! control traffic keeps flowing while bulk transfers run, and conflicts and
//! Git publications go the same way.

mod common;

use common::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Comfortably past the 10 MiB ceiling the old control plane imposed.
const LARGE: usize = 12 * 1024 * 1024;

/// Past the old 32 MiB outbound queue bound as well, so this size could not
/// have crossed the wire under the previous design at any message limit.
const HUGE: usize = 40 * 1024 * 1024;

struct Session {
    _sandbox: Sandbox,
    host: Participant,
    guests: Vec<Participant>,
}

impl Session {
    fn everyone(&self) -> impl Iterator<Item = &Participant> {
        std::iter::once(&self.host).chain(self.guests.iter())
    }

    fn stop(&mut self) {
        for guest in &mut self.guests {
            guest.stop_daemon();
        }
        self.host.stop_daemon();
    }
}

/// A host plus `guests` participants, all online and holding canonical state.
fn start_session(label: &str, guests: usize) -> Session {
    let sandbox = Sandbox::new(label);
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");

    let names = ["beta", "gamma", "delta"];
    let people = [
        ("Alice", "alice@example.com"),
        ("Bob", "bob@example.com"),
        ("Carol", "carol@example.com"),
    ];
    let mut clones = Vec::new();
    for index in 0..guests {
        let guest = Participant::new(&sandbox, names[index]);
        let out = std::process::Command::new("git")
            .args(["clone", "-q", "-c", "core.autocrlf=false"])
            .arg(&host.repo)
            .arg(&guest.repo)
            .output()
            .expect("git clone");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        git(&guest.repo, &["config", "user.name", people[index].0]);
        git(&guest.repo, &["config", "user.email", people[index].1]);
        git(&guest.repo, &["config", "commit.gpgsign", "false"]);
        clones.push(guest);
    }

    host.start_daemon(&["host", "--lan"]);
    host.wait_online(LONG);

    let invite = host.json(&["invite"]);
    let invite_path = host.repo.parent().unwrap().join("invite.txt");
    std::fs::write(&invite_path, invite["invite"].as_str().unwrap()).unwrap();

    for guest in &mut clones {
        guest.start_daemon(&["join", "--invite-file", invite_path.to_str().unwrap()]);
        guest.wait_online(LONG);
        guest.wait_for_status("the guest to receive canonical state", LONG, |v| {
            v["file_count"].as_u64().unwrap_or(0) >= 3
        });
    }

    Session {
        _sandbox: sandbox,
        host,
        guests: clones,
    }
}

/// Wait until every participant reports the same revision and state hash.
///
/// This is the invariant the whole redesign exists to protect: one logical
/// repository state, for everybody, whatever the file sizes involved.
fn wait_for_agreement(session: &Session, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let states: Vec<serde_json::Value> = session
            .everyone()
            .map(|p| p.json_allow_failure(&["status"]).unwrap_or_default())
            .collect();
        let settled = states.iter().all(|v| {
            v["outbox_pending"] == 0
                && v["state"].as_str().is_some()
                && v["state"] == states[0]["state"]
                && v["live_revision"] == states[0]["live_revision"]
        });
        if settled {
            return states[0]["state"].as_str().unwrap().to_string();
        }
        if Instant::now() >= deadline {
            let dump: Vec<String> = states
                .iter()
                .map(|v| format!("{} @ r{}", v["state"], v["live_revision"]))
                .collect();
            for participant in session.everyone() {
                participant.assert_daemon_healthy();
            }
            panic!(
                "participants never agreed on one state: {}\nhost log:\n{}",
                dump.join(" | "),
                session.host.daemon_output()
            );
        }
        for participant in session.everyone() {
            participant.assert_daemon_healthy();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Every partially received blob a participant is currently holding.
///
/// These are hash-named, so a transfer that is interrupted leaves behind
/// something the next attempt can recognize and continue from.
fn partials(participant: &Participant) -> Vec<(PathBuf, u64)> {
    let dir = participant
        .repo
        .join(".git")
        .join("weave")
        .join("blobs")
        .join(".partial");
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            // Not `entry.metadata()`: on Windows that reports the directory
            // entry as it stood when the directory was enumerated, and the size
            // recorded there does not move while the file is open. Asking for
            // the path opens the file and reports what is really in it.
            let len = std::fs::metadata(entry.path())
                .map(|m| m.len())
                .unwrap_or(0);
            found.push((entry.path(), len));
        }
    }
    found.sort();
    found
}

/// Block until a transfer into `participant` is genuinely in flight.
///
/// Sleeping a fixed time instead would be a race against the sender: a large
/// file has to be observed as stable, hashed and submitted before a single byte
/// moves, so a fixed delay tends to interrupt the receiver before the transfer
/// it was meant to interrupt has even started.
fn wait_for_transfer(participant: &Participant, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if partials(participant).iter().any(|(_, len)| *len > 0) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no transfer ever started on this participant"
        );
        participant.assert_daemon_healthy();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The offset a resumed download restarted from, as the receiver logged it.
///
/// The log is the only place this is observable from outside: the offset is
/// negotiated inside the encrypted data plane and never reaches the working
/// tree. `start_daemon` truncates the log, so a line found after a restart
/// belongs to the run that restarted.
fn resumed_offset(participant: &Participant) -> Option<u64> {
    participant.daemon_output().lines().find_map(|line| {
        let rest = line.split("resuming blob ").nth(1)?;
        let rest = rest.split("at offset ").nth(1)?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

// ---------------------------------------------------------------------------
// 1. Create, modify, delete a large file with three participants
// ---------------------------------------------------------------------------

#[test]
fn a_large_binary_file_is_created_modified_and_deleted_for_everyone() {
    let mut session = start_session("blob-lifecycle", 2);

    // Created by the host.
    let first = blob_bytes(1, LARGE);
    write_bytes(&session.host.repo, "assets/deck.pdf", &first);
    for guest in &session.guests {
        guest.wait_for_bytes("assets/deck.pdf", &first, LONG);
    }
    wait_for_agreement(&session, LONG);

    // Modified by a participant: the new content travels the other way, and
    // being a different hash it cannot be satisfied by the copy already held.
    let second = blob_bytes(2, LARGE + 4096);
    write_bytes(&session.guests[0].repo, "assets/deck.pdf", &second);
    session
        .host
        .wait_for_bytes("assets/deck.pdf", &second, LONG);
    session.guests[1].wait_for_bytes("assets/deck.pdf", &second, LONG);
    wait_for_agreement(&session, LONG);

    // Deleted by the other participant.
    std::fs::remove_file(session.guests[1].repo.join("assets/deck.pdf")).unwrap();
    session.host.wait_for_missing("assets/deck.pdf", LONG);
    session.guests[0].wait_for_missing("assets/deck.pdf", LONG);
    wait_for_agreement(&session, LONG);

    session.stop();
}

// ---------------------------------------------------------------------------
// 2. Sizes the old design could not carry at all
// ---------------------------------------------------------------------------

#[test]
fn a_file_far_above_the_old_limits_travels_in_both_directions() {
    let mut session = start_session("blob-huge", 1);

    let down = blob_bytes(11, HUGE);
    write_bytes(&session.host.repo, "video/keynote.mov", &down);
    session.guests[0].wait_for_bytes("video/keynote.mov", &down, LONG);

    let up = blob_bytes(12, HUGE);
    write_bytes(&session.guests[0].repo, "video/reply.mov", &up);
    session.host.wait_for_bytes("video/reply.mov", &up, LONG);

    wait_for_agreement(&session, LONG);
    session.stop();
}

// ---------------------------------------------------------------------------
// 3. Control priority: the session stays live during a bulk transfer
// ---------------------------------------------------------------------------

#[test]
fn control_traffic_keeps_flowing_during_a_large_transfer() {
    let mut session = start_session("blob-priority", 1);

    // Start a transfer that will still be running for the next few seconds.
    let bulk = blob_bytes(21, HUGE);
    write_bytes(&session.host.repo, "video/bulk.mov", &bulk);

    // While it runs, ordinary control traffic must keep its latency. Each of
    // these is a full round trip: the guest captures, submits, the host accepts
    // and broadcasts, and the guest sees the host's copy appear.
    for round in 0..3 {
        let text = format!("note {round}\n");
        let name = format!("notes/{round}.md");
        let started = Instant::now();
        write_bytes(&session.guests[0].repo, &name, text.as_bytes());
        session.host.wait_for_file(&name, &text, SHORT);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(20),
            "a small edit took {elapsed:?} while a bulk transfer was running"
        );

        // And the daemon answers its control socket throughout.
        let status = session.guests[0].status();
        assert_eq!(
            status["connection"].as_str(),
            Some("online"),
            "the guest dropped offline during a transfer: {status}"
        );
    }

    // The bulk transfer still completes correctly afterwards.
    session.guests[0].wait_for_bytes("video/bulk.mov", &bulk, LONG);
    wait_for_agreement(&session, LONG);
    session.stop();
}

// ---------------------------------------------------------------------------
// 4. Interruption installs nothing, and the transfer completes after restart
// ---------------------------------------------------------------------------

#[test]
fn a_killed_daemon_never_leaves_a_partial_file_and_finishes_after_restart() {
    let mut session = start_session("blob-interrupt", 1);

    let bytes = blob_bytes(31, HUGE);
    write_bytes(&session.host.repo, "video/interrupted.mov", &bytes);

    // Kill the guest while the transfer is in flight. No graceful stop: this is
    // a crash, so any partial write is abandoned exactly where it stood.
    std::thread::sleep(Duration::from_millis(400));
    session.guests[0].kill_daemon();

    // Nothing partial may be visible in the working tree. Materialization
    // happens only after a blob is installed, and a blob is installed only
    // after its SHA-256 has been verified whole.
    let path = session.guests[0].repo.join("video/interrupted.mov");
    if path.exists() {
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "a partially transferred file appeared in the working tree"
        );
    }

    session.guests[0].start_daemon(&["resume"]);
    session.guests[0].wait_online(LONG);
    session.guests[0].wait_for_bytes("video/interrupted.mov", &bytes, LONG);
    wait_for_agreement(&session, LONG);

    session.stop();
}

// ---------------------------------------------------------------------------
// 5. Many transfers at once, in both directions, stay separate
// ---------------------------------------------------------------------------

#[test]
fn many_concurrent_transfers_stay_isolated() {
    let mut session = start_session("blob-concurrent", 2);

    // Six blobs of the same size and different content, produced at once from
    // three different machines. If transfer bookkeeping ever crossed streams,
    // the result would be a file holding somebody else's bytes, and the hash
    // check would turn that into a stalled transfer rather than a wrong file —
    // either way this test fails.
    let size = 3 * 1024 * 1024;
    let host_blobs: Vec<Vec<u8>> = (0..2).map(|i| blob_bytes(100 + i, size)).collect();
    let alice_blobs: Vec<Vec<u8>> = (0..2).map(|i| blob_bytes(200 + i, size)).collect();
    let bob_blobs: Vec<Vec<u8>> = (0..2).map(|i| blob_bytes(300 + i, size)).collect();

    for (i, bytes) in host_blobs.iter().enumerate() {
        write_bytes(&session.host.repo, &format!("bulk/host-{i}.bin"), bytes);
    }
    for (i, bytes) in alice_blobs.iter().enumerate() {
        write_bytes(
            &session.guests[0].repo,
            &format!("bulk/alice-{i}.bin"),
            bytes,
        );
    }
    for (i, bytes) in bob_blobs.iter().enumerate() {
        write_bytes(&session.guests[1].repo, &format!("bulk/bob-{i}.bin"), bytes);
    }

    for participant in session.everyone() {
        for (i, bytes) in host_blobs.iter().enumerate() {
            participant.wait_for_bytes(&format!("bulk/host-{i}.bin"), bytes, LONG);
        }
        for (i, bytes) in alice_blobs.iter().enumerate() {
            participant.wait_for_bytes(&format!("bulk/alice-{i}.bin"), bytes, LONG);
        }
        for (i, bytes) in bob_blobs.iter().enumerate() {
            participant.wait_for_bytes(&format!("bulk/bob-{i}.bin"), bytes, LONG);
        }
    }
    wait_for_agreement(&session, LONG);

    session.stop();
}

// ---------------------------------------------------------------------------
// 6. A conflict over a large binary keeps both candidates
// ---------------------------------------------------------------------------

#[test]
fn a_large_binary_conflict_keeps_both_candidates() {
    let mut session = start_session("blob-conflict", 1);

    let base = blob_bytes(41, LARGE);
    write_bytes(&session.host.repo, "assets/poster.psd", &base);
    session.guests[0].wait_for_bytes("assets/poster.psd", &base, LONG);
    wait_for_agreement(&session, LONG);

    // Concurrent edits: binary content cannot be merged, so this is a conflict
    // by construction, and both candidates are large.
    session.guests[0].stop_daemon();
    let guest_version = blob_bytes(42, LARGE);
    write_bytes(&session.guests[0].repo, "assets/poster.psd", &guest_version);

    // Wait for the host's own edit to become canonical, and specifically for a
    // *new* revision: a large file is captured only once it has held still, so
    // "the outbox is empty" is also true in the moment before it is captured.
    let before = session.host.status()["live_revision"].as_u64().unwrap_or(0);
    let host_version = blob_bytes(43, LARGE);
    write_bytes(&session.host.repo, "assets/poster.psd", &host_version);
    session.host.wait_for_status("the host edit", LONG, |v| {
        v["outbox_pending"] == 0 && v["live_revision"].as_u64().unwrap_or(0) > before
    });

    session.guests[0].start_daemon(&["resume"]);
    session.guests[0].wait_online(LONG);

    let conflict = session.guests[0].wait_for_conflict(LONG);
    assert_eq!(conflict["conflict"]["path"], "assets/poster.psd");

    // The rejected candidate is preserved on disk in full — that is what makes
    // "no work is discarded" true for content that never fit in a message.
    let incoming = conflict["candidate_files"]["incoming"].as_str().unwrap();
    assert_eq!(
        std::fs::read(incoming).unwrap(),
        guest_version,
        "the guest's large candidate must be preserved byte for byte"
    );

    // Canonical content is restored in the working tree, whole.
    session.guests[0].wait_for_bytes("assets/poster.psd", &host_version, LONG);

    // Resolving with the local candidate sends those bytes back up the blob
    // plane and makes them canonical for everybody.
    let id = conflict["conflict"]["id"].as_str().unwrap().to_string();
    session.guests[0].expect(&["conflict", "resolve", &id, "--use", "local"]);
    session
        .host
        .wait_for_bytes("assets/poster.psd", &guest_version, LONG);
    session.guests[0].wait_for_bytes("assets/poster.psd", &guest_version, LONG);
    assert_eq!(session.host.json(&["conflict", "list"])["open_count"], 0);

    wait_for_agreement(&session, LONG);
    session.stop();
}

// ---------------------------------------------------------------------------
// 7. Publication: the Git pack travels on the blob plane too
// ---------------------------------------------------------------------------

#[test]
fn a_publication_containing_a_large_file_reaches_participants() {
    let mut session = start_session("blob-publish", 1);

    let bytes = blob_bytes(51, LARGE);
    write_bytes(&session.guests[0].repo, "assets/handout.pdf", &bytes);
    session
        .host
        .wait_for_bytes("assets/handout.pdf", &bytes, LONG);
    wait_for_agreement(&session, LONG);

    let prepare = session.guests[0].json(&["commit", "prepare"]);
    let prepare_id = prepare["prepare_id"].as_str().unwrap().to_string();
    let publication = session.guests[0].json(&[
        "commit",
        "create",
        &prepare_id,
        "--message",
        "assets: add the handout",
    ]);
    let commit_oid = publication["descriptor"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(git(&session.host.repo, &["rev-parse", "HEAD"]), commit_oid);

    // The participant installs the exact objects the host produced, delivered
    // as a pack on the blob plane rather than inlined in the publication.
    session.guests[0].wait_for_git(&["rev-parse", "HEAD"], &commit_oid, LONG);
    let listed = git(
        &session.guests[0].repo,
        &["cat-file", "-s", &format!("{commit_oid}^{{tree}}")],
    );
    assert!(!listed.is_empty(), "the tree object must be installed");
    assert_eq!(
        std::fs::read(session.guests[0].repo.join("assets/handout.pdf")).unwrap(),
        bytes
    );

    session.stop();
}

// ---------------------------------------------------------------------------
// 8. Reconnecting resumes a transfer instead of restarting it
// ---------------------------------------------------------------------------

#[test]
fn an_interrupted_transfer_resumes_from_a_non_zero_offset() {
    let mut session = start_session("blob-resume", 1);

    let bytes = blob_bytes(61, HUGE);
    write_bytes(&session.host.repo, "video/resumed.mov", &bytes);

    // Crash the guest while the transfer is in flight. No graceful stop: the
    // partial is abandoned exactly where it stood, with no chance to record
    // anything about it.
    wait_for_transfer(&session.guests[0], LONG);
    session.guests[0].kill_daemon();

    let held = partials(&session.guests[0]);
    assert_eq!(
        held.len(),
        1,
        "expected exactly one partially received blob, found {held:?}"
    );
    let held = held[0].1;
    assert!(
        held > 0 && held < bytes.len() as u64,
        "the interruption landed outside the transfer: {held} of {} bytes",
        bytes.len()
    );

    // Nothing partial is visible anywhere: installation is the only path into
    // the store, and materialization only follows installation.
    assert!(
        !session.guests[0].repo.join("video/resumed.mov").exists(),
        "an unfinished transfer appeared in the working tree"
    );

    session.guests[0].start_daemon(&["resume"]);
    session.guests[0].wait_online(LONG);
    session.guests[0].wait_for_bytes("video/resumed.mov", &bytes, LONG);

    // The property under test: the second attempt continued from the bytes
    // already on disk rather than paying for them again.
    let offset = resumed_offset(&session.guests[0]).unwrap_or_else(|| {
        panic!(
            "the guest never resumed a transfer:\n{}",
            session.guests[0].daemon_output()
        )
    });
    assert_eq!(
        offset, held,
        "the transfer resumed from the wrong offset (holding {held} bytes)"
    );

    // And a completed transfer leaves nothing behind to recover.
    assert!(
        partials(&session.guests[0]).is_empty(),
        "a partial survived a completed transfer: {:?}",
        partials(&session.guests[0])
    );

    wait_for_agreement(&session, LONG);
    session.stop();
}

// ---------------------------------------------------------------------------
// 9. A partial that is no longer a prefix of the content is thrown away
// ---------------------------------------------------------------------------

#[test]
fn a_damaged_partial_is_discarded_rather_than_installed() {
    let mut session = start_session("blob-damaged", 1);

    let bytes = blob_bytes(62, HUGE);
    write_bytes(&session.host.repo, "video/damaged.mov", &bytes);

    wait_for_transfer(&session.guests[0], LONG);
    session.guests[0].kill_daemon();

    // Damage what was received without changing its length, which is the one
    // case resumption could get wrong: the offset still looks right, the
    // remaining bytes are the right ones, and only the hash of the whole
    // content can tell that the result is not the file that was announced.
    let mut held = partials(&session.guests[0]);
    let (path, len) = held.pop().expect("a partially received blob");
    assert!(len > 0 && len < bytes.len() as u64);
    let mut damaged = std::fs::read(&path).unwrap();
    for byte in damaged.iter_mut().take(64 * 1024) {
        *byte ^= 0xff;
    }
    std::fs::write(&path, &damaged).unwrap();

    session.guests[0].start_daemon(&["resume"]);
    session.guests[0].wait_online(LONG);

    // This is the one case that deliberately pays for the content twice: the
    // resumed tail is transferred, rejected on the whole-content hash, and the
    // file is then fetched again from zero. With the whole suite running in
    // parallel that does not fit in the budget a single transfer gets.
    let patient = LONG * 3;

    // The mismatch is caught when the transfer completes, the partial is
    // destroyed rather than installed, and the content is fetched again from
    // zero. Converging on the correct bytes is what proves all three.
    session.guests[0].wait_for_bytes("video/damaged.mov", &bytes, patient);
    assert!(
        partials(&session.guests[0]).is_empty(),
        "a rejected partial was left behind: {:?}",
        partials(&session.guests[0])
    );

    wait_for_agreement(&session, patient);
    session.stop();
}

// ---------------------------------------------------------------------------
// 10. Publication waits for a participant that cannot yet reproduce the state
// ---------------------------------------------------------------------------

#[test]
fn a_publication_waits_for_a_participant_still_receiving_content() {
    let mut session = start_session("blob-barrier", 1);

    // Large enough that the transfer is still running when the barrier opens,
    // and small enough to finish well inside the host's 20 second barrier
    // timeout, which is the backstop this test must not be measuring.
    let bytes = blob_bytes(63, LARGE);
    write_bytes(&session.host.repo, "assets/master.psd", &bytes);

    // Prepare while the guest demonstrably cannot yet reproduce the state it
    // has been told about: its copy is a partial file in the blob store and
    // nothing is in its working tree.
    wait_for_transfer(&session.guests[0], LONG);
    let prepare = session.host.json(&["commit", "prepare"]);

    let disconnected = prepare["disconnected_participants"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        disconnected.is_empty(),
        "the guest was written off as disconnected instead of waited for: {prepare}"
    );

    // A barrier that answered "ready" as soon as it was asked would let this
    // return with the guest still half way through the transfer. It cannot
    // answer until materialization is unblocked, which means the bytes are on
    // disk before `prepare` comes back.
    assert_eq!(
        std::fs::read(session.guests[0].repo.join("assets/master.psd")).unwrap(),
        bytes,
        "the publication barrier did not wait for the guest to finish receiving"
    );

    // And the publication itself still completes for everybody.
    let id = prepare["prepare_id"].as_str().unwrap().to_string();
    let publication = session.host.json(&[
        "commit",
        "create",
        &id,
        "--message",
        "assets: add the master",
    ]);
    let commit_oid = publication["descriptor"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();
    session.guests[0].wait_for_git(&["rev-parse", "HEAD"], &commit_oid, LONG);

    wait_for_agreement(&session, LONG);
    session.stop();
}

// ---------------------------------------------------------------------------
// 11. Collection removes only what nothing refers to any more
// ---------------------------------------------------------------------------

#[test]
fn collecting_garbage_never_removes_content_the_session_still_needs() {
    let mut session = start_session("blob-gc", 1);

    // One file that stays as it is, and one that is superseded. The superseded
    // content is what makes this test non-vacuous: on a replica nothing refers
    // to it any more, so a sweep that collects nothing at all would prove
    // nothing about a sweep that collects too much.
    let keep = blob_bytes(71, LARGE);
    write_bytes(&session.host.repo, "assets/keep.bin", &keep);
    session.guests[0].wait_for_bytes("assets/keep.bin", &keep, LONG);

    let first = blob_bytes(72, LARGE);
    write_bytes(&session.host.repo, "assets/moving.bin", &first);
    session.guests[0].wait_for_bytes("assets/moving.bin", &first, LONG);
    let second = blob_bytes(73, LARGE);
    write_bytes(&session.host.repo, "assets/moving.bin", &second);
    session.guests[0].wait_for_bytes("assets/moving.bin", &second, LONG);
    wait_for_agreement(&session, LONG);
    session.stop();

    // Sweep each participant with the live set the daemon itself derives, and
    // with no age guard at all — far more aggressive than the daemon ever is.
    let sweep = |participant: &Participant| -> weave::blobs::GcReport {
        let weave_dir = participant.repo.join(".git").join("weave");
        let blobs = weave::blobs::BlobStore::open(weave_dir.join("blobs")).unwrap();
        let store =
            weave::store_client::ClientStore::open(&weave_dir.join("state.sqlite")).unwrap();
        let mut live = store.referenced_blobs().unwrap();
        live.extend(
            weave::store_host::referenced_blobs_at(&weave_dir.join("host.sqlite")).unwrap(),
        );
        let report = blobs.collect_garbage(&live, 0).unwrap();

        // Everything this replica has been told to hold is still here.
        for (path, entry) in store.replica_manifest().unwrap() {
            assert!(
                blobs.has(&entry.blob_hash),
                "collection removed live content for {}",
                path.as_str()
            );
        }
        report
    };

    let guest_report = sweep(&session.guests[0]);
    assert!(
        guest_report.blobs > 0,
        "nothing was collectable on the guest, so this proves nothing"
    );
    sweep(&session.host);

    // The host keeps more than its current manifest: every revision it can
    // still describe names content that has to remain readable.
    let weave_dir = session.host.repo.join(".git").join("weave");
    let blobs = weave::blobs::BlobStore::open(weave_dir.join("blobs")).unwrap();
    let host_store = weave::store_host::HostStore::open(&weave_dir.join("host.sqlite")).unwrap();
    let missing = host_store.verify_blob_references(&blobs).unwrap();
    assert!(
        missing.is_empty(),
        "collection broke the host's revision history: {missing:?}"
    );

    // Both copies of the working tree still reproduce from what survived.
    for participant in session.everyone() {
        assert_eq!(
            std::fs::read(participant.repo.join("assets/keep.bin")).unwrap(),
            keep
        );
        assert_eq!(
            std::fs::read(participant.repo.join("assets/moving.bin")).unwrap(),
            second
        );
    }
}
