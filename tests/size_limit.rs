// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The canonical session file size limit and its state machine.
//!
//! `docs/BLOB-PLANE.md` section 4. The limit is session state, not a local
//! preference, and the promise around it is narrow and absolute: a file above
//! it is preserved exactly as its author wrote it, is never partially
//! synchronized, is visible to everyone, and blocks Git publication until it is
//! resolved. Every test here drives the real binary, so what is exercised is
//! the behaviour a user gets.
//!
//! The sessions run with a deliberately small limit. Nothing about the state
//! machine depends on the size, and 6 MiB proves it as well as 200 MiB would in
//! a fraction of the time.

mod common;

use common::*;
use std::time::{Duration, Instant};

/// The limit these sessions run under.
const LIMIT: &str = "4MiB";

/// Comfortably above the limit, and below the 8 MiB stability threshold, so
/// these tests measure the limit rather than the settling rules.
const OVERSIZE: usize = 6 * 1024 * 1024;

/// Ordinary content: under the limit, over the text-merge limit.
const ORDINARY: usize = 2 * 1024 * 1024;

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

/// Build a repository and its clones, without starting anything.
fn prepare(label: &str, guests: usize) -> (Sandbox, Participant, Vec<Participant>) {
    let sandbox = Sandbox::new(label);
    let host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");

    let names = ["beta", "gamma"];
    let people = [("Alice", "alice@example.com"), ("Bob", "bob@example.com")];
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
    (sandbox, host, clones)
}

/// A live session running under `LIMIT`, everybody online and caught up.
fn start_session(label: &str, guests: usize) -> Session {
    let (sandbox, mut host, mut clones) = prepare(label, guests);

    host.start_daemon(&["host", "--lan", "--max-file-size", LIMIT]);
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

/// Wait until `participant` sees `path` among the session's oversize paths,
/// and return the row it saw.
fn wait_for_oversize(
    participant: &Participant,
    path: &str,
    timeout: Duration,
) -> serde_json::Value {
    let status = participant.wait_for_status(
        &format!("{path} to be reported as above the session limit"),
        timeout,
        |v| {
            v["oversize"]
                .as_array()
                .map(|list| list.iter().any(|item| item["path"] == path))
                .unwrap_or(false)
        },
    );
    status["oversize"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["path"] == path)
        .cloned()
        .unwrap()
}

fn wait_for_no_oversize(participant: &Participant, timeout: Duration) {
    participant.wait_for_status(
        "the session limit to stop blocking anything",
        timeout,
        |v| {
            v["oversize"]
                .as_array()
                .map(|list| list.is_empty())
                .unwrap_or(false)
        },
    );
}

/// Wait until every participant reports the same revision and state hash.
fn wait_for_agreement(session: &Session, timeout: Duration) {
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
            return;
        }
        if Instant::now() >= deadline {
            let dump: Vec<String> = states
                .iter()
                .map(|v| format!("{} @ r{}", v["state"], v["live_revision"]))
                .collect();
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

/// `weave commit prepare`, expecting it to be refused, returning what was said.
fn refused_prepare(participant: &Participant) -> String {
    match participant.run(&["commit", "prepare", "--json"]) {
        Ok(text) => panic!("the publication was prepared even though it should not be:\n{text}"),
        Err(message) => message,
    }
}

// ---------------------------------------------------------------------------
// 1. A session is never started on state it cannot represent
// ---------------------------------------------------------------------------

#[test]
fn a_host_refuses_to_start_on_a_repository_it_could_not_represent() {
    let (_sandbox, mut host, _) = prepare("limit-startup", 0);
    write_bytes(&host.repo, "media/reel.mov", &blob_bytes(1, OVERSIZE));
    git(&host.repo, &["add", "-A"]);
    git(&host.repo, &["commit", "-q", "-m", "Add the reel"]);

    let (ok, output) = host.run_until_exit(&["host", "--lan", "--max-file-size", LIMIT], SHORT);
    assert!(!ok, "the session started anyway:\n{output}");
    assert!(
        output.contains("media/reel.mov"),
        "the refusal did not name the file:\n{output}"
    );
    assert!(
        output.contains("--max-file-size"),
        "the refusal did not offer raising the limit:\n{output}"
    );
    assert!(
        output.contains(".gitignore") || output.contains("remove these"),
        "the refusal did not offer removing the file:\n{output}"
    );

    // The same repository, with a limit that can hold it, starts normally.
    host.start_daemon(&["host", "--lan", "--max-file-size", "16MiB"]);
    host.wait_online(LONG);
    host.wait_for_status("the reel to become canonical", LONG, |v| {
        v["file_count"].as_u64().unwrap_or(0) >= 4
    });
    host.stop_daemon();
}

// ---------------------------------------------------------------------------
// 2. A file created above the limit, and what everybody sees
// ---------------------------------------------------------------------------

#[test]
fn an_oversize_file_is_preserved_reported_everywhere_and_blocks_publication() {
    let mut session = start_session("limit-created", 1);
    let bytes = blob_bytes(2, OVERSIZE);
    write_bytes(&session.host.repo, "media/master.mov", &bytes);

    // Whoever owns it sees it, with the size and the fact that it is theirs.
    let mine = wait_for_oversize(&session.host, "media/master.mov", LONG);
    assert_eq!(mine["size"].as_u64(), Some(OVERSIZE as u64));
    assert_eq!(mine["mine"].as_bool(), Some(true));
    assert_eq!(mine["canonical"].as_bool(), Some(false));

    // And so does everybody else, told whose machine it is on: the file blocks
    // the session, so the session gets to see it.
    let theirs = wait_for_oversize(&session.guests[0], "media/master.mov", LONG);
    assert_eq!(theirs["mine"].as_bool(), Some(false));
    assert_eq!(theirs["display_name"].as_str(), Some("Quentin"));
    assert_eq!(theirs["size"].as_u64(), Some(OVERSIZE as u64));

    // The local bytes are exactly what was written: not read into a blob, not
    // rewritten, not truncated to fit.
    assert_eq!(
        std::fs::read(session.host.repo.join("media/master.mov")).unwrap(),
        bytes,
        "the local file was modified"
    );
    // And it was never partially synchronized: nothing of it reached anybody.
    assert!(
        !file_exists(&session.guests[0].repo, "media/master.mov"),
        "an oversize file appeared on another machine"
    );
    assert_eq!(
        session.host.status()["file_count"].as_u64(),
        Some(3),
        "the oversize file entered canonical state"
    );

    // Publication is refused, from either end, naming the file and the way out.
    for participant in [&session.host, &session.guests[0]] {
        let refusal = refused_prepare(participant);
        assert!(
            refusal.contains("media/master.mov"),
            "the refusal did not name the file:\n{refusal}"
        );
        assert!(
            refusal.contains("weave limit set"),
            "the refusal did not offer raising the limit:\n{refusal}"
        );
    }

    // Everything else in the repository carries on as normal.
    write_file(&session.host.repo, "slides/01-intro.md", "L1\nL2\nL3\nL4\n");
    session.guests[0].wait_for_file("slides/01-intro.md", "L1\nL2\nL3\nL4\n", LONG);

    session.stop();
}

// ---------------------------------------------------------------------------
// 3. Shrinking, then deleting, clears the condition on its own
// ---------------------------------------------------------------------------

#[test]
fn shrinking_or_deleting_the_file_resolves_it_with_no_special_step() {
    let mut session = start_session("limit-shrink", 1);
    let small = blob_bytes(3, ORDINARY);
    write_bytes(
        &session.host.repo,
        "media/draft.mov",
        &blob_bytes(3, OVERSIZE),
    );
    wait_for_oversize(&session.host, "media/draft.mov", LONG);

    // Below the limit it is an ordinary create, with nothing to undo: it was
    // never canonical, so no revision has to be taken back.
    write_bytes(&session.host.repo, "media/draft.mov", &small);
    session.guests[0].wait_for_bytes("media/draft.mov", &small, LONG);
    wait_for_no_oversize(&session.host, LONG);
    wait_for_no_oversize(&session.guests[0], LONG);
    wait_for_agreement(&session, LONG);

    // With nothing held back, publication works again.
    let prepared = session.host.json(&["commit", "prepare"]);
    let prepare_id = prepared["prepare_id"].as_str().unwrap().to_string();
    session
        .host
        .expect(&["commit", "create", &prepare_id, "-m", "Add the draft"]);

    // The other way out: deletion. Also ordinary.
    std::fs::remove_file(session.host.repo.join("media/draft.mov")).unwrap();
    session.guests[0].wait_for_missing("media/draft.mov", LONG);
    wait_for_agreement(&session, LONG);

    session.stop();
}

// ---------------------------------------------------------------------------
// 4. Raising the limit captures what was held back
// ---------------------------------------------------------------------------

#[test]
fn raising_the_limit_captures_the_file_that_was_too_large() {
    let mut session = start_session("limit-raise", 1);
    let bytes = blob_bytes(4, OVERSIZE);
    write_bytes(&session.host.repo, "media/keynote.mov", &bytes);
    wait_for_oversize(&session.guests[0], "media/keynote.mov", LONG);

    let before = session.guests[0].json(&["limit", "show"]);
    assert_eq!(before["max_file_size"].as_u64(), Some(4 * 1024 * 1024));

    // Raised from a participant, not the host: it is a session decision, and
    // every participant observes it at the same control version.
    let after = session.guests[0].json(&["limit", "set", "16MiB"]);
    assert_eq!(after["max_file_size"].as_u64(), Some(16 * 1024 * 1024));
    assert!(
        after["control_version"].as_u64().unwrap_or(0)
            > before["control_version"].as_u64().unwrap_or(0),
        "the new limit did not arrive as a new control version"
    );

    // The file that was held back is now ordinary work, and travels.
    session.guests[0].wait_for_bytes("media/keynote.mov", &bytes, LONG);
    wait_for_no_oversize(&session.host, LONG);
    wait_for_no_oversize(&session.guests[0], LONG);
    wait_for_agreement(&session, LONG);

    // And it really is canonical: publication proceeds and carries it.
    let prepared = session.host.json(&["commit", "prepare"]);
    let prepare_id = prepared["prepare_id"].as_str().unwrap().to_string();
    session
        .host
        .expect(&["commit", "create", &prepare_id, "-m", "Add the keynote"]);
    session.guests[0].wait_for_git(
        &["ls-tree", "--name-only", "HEAD", "media/"],
        "media/keynote.mov",
        LONG,
    );

    session.stop();
}

// ---------------------------------------------------------------------------
// 5. Lowering below what the session already holds is refused
// ---------------------------------------------------------------------------

#[test]
fn the_limit_cannot_drop_below_content_the_session_already_holds() {
    let mut session = start_session("limit-lower", 1);
    let bytes = blob_bytes(5, ORDINARY);
    write_bytes(&session.host.repo, "media/short.mov", &bytes);
    session.guests[0].wait_for_bytes("media/short.mov", &bytes, LONG);
    wait_for_agreement(&session, LONG);

    let refusal = session
        .host
        .run(&["limit", "set", "1MiB", "--json"])
        .expect_err("lowering under canonical content must be refused");
    assert!(
        refusal.contains("media/short.mov"),
        "the refusal did not name the file that prevents it:\n{refusal}"
    );

    // Refused, not partially applied: the session still holds the old value.
    assert_eq!(
        session.host.json(&["limit", "show"])["max_file_size"].as_u64(),
        Some(4 * 1024 * 1024)
    );
    assert_eq!(
        session.guests[0].json(&["limit", "show"])["max_file_size"].as_u64(),
        Some(4 * 1024 * 1024)
    );

    // Nonsense values are refused before they reach the session at all.
    assert!(session.host.run(&["limit", "set", "12KiB"]).is_err());
    assert!(session.host.run(&["limit", "set", "enormous"]).is_err());

    // Lowering to something the content fits inside is fine.
    session.host.expect(&["limit", "set", "3MiB"]);
    assert_eq!(
        session.guests[0].wait_for_status("the lowered limit to arrive", LONG, |v| {
            v["max_file_size"].as_u64() == Some(3 * 1024 * 1024)
        })["max_file_size"]
            .as_u64(),
        Some(3 * 1024 * 1024)
    );

    session.stop();
}

// ---------------------------------------------------------------------------
// 6. The condition is durable, and survives a restart
// ---------------------------------------------------------------------------

#[test]
fn an_oversize_condition_survives_a_restart() {
    use weave::store_client::ClientStore;

    let mut session = start_session("limit-restart", 1);
    let bytes = blob_bytes(6, OVERSIZE);
    write_bytes(&session.guests[0].repo, "media/guest.mov", &bytes);
    wait_for_oversize(&session.guests[0], "media/guest.mov", LONG);
    wait_for_oversize(&session.host, "media/guest.mov", LONG);

    session.guests[0].stop_daemon();

    // Durable, not remembered by a running process: the record is in the
    // participant's own store, where a restart will find it.
    let db = session.guests[0]
        .repo
        .join(".git")
        .join("weave")
        .join("state.sqlite");
    assert!(db.exists(), "no participant store at {}", db.display());
    let store = ClientStore::open(&db).expect("open the participant store");
    let held = store.oversize().expect("read the oversize set");
    assert_eq!(held.len(), 1, "the condition was not recorded durably");
    assert_eq!(held[0].path.as_str(), "media/guest.mov");
    assert_eq!(held[0].size, OVERSIZE as u64);
    drop(store);

    session.guests[0].start_daemon(&["resume"]);
    session.guests[0].wait_online(LONG);

    // Reported again without anybody touching the file, and still blocking.
    wait_for_oversize(&session.guests[0], "media/guest.mov", LONG);
    wait_for_oversize(&session.host, "media/guest.mov", LONG);
    assert_eq!(
        std::fs::read(session.guests[0].repo.join("media/guest.mov")).unwrap(),
        bytes
    );
    let refusal = refused_prepare(&session.host);
    assert!(
        refusal.contains("media/guest.mov"),
        "publication was not blocked after the restart:\n{refusal}"
    );

    session.stop();
}

// ---------------------------------------------------------------------------
// 7. A canonical file that grows past the limit is never overwritten
// ---------------------------------------------------------------------------

#[test]
fn a_canonical_file_that_grows_oversize_is_never_overwritten() {
    let mut session = start_session("limit-grew", 1);
    let original = blob_bytes(7, ORDINARY);
    write_bytes(&session.host.repo, "media/cut.mov", &original);
    session.guests[0].wait_for_bytes("media/cut.mov", &original, LONG);
    wait_for_agreement(&session, LONG);

    // The guest grows its copy past the limit. Nothing of it is captured.
    let grown = blob_bytes(8, OVERSIZE);
    write_bytes(&session.guests[0].repo, "media/cut.mov", &grown);
    let reported = wait_for_oversize(&session.guests[0], "media/cut.mov", LONG);
    assert_eq!(
        reported["canonical"].as_bool(),
        Some(true),
        "the session was not told it already holds content for this path"
    );

    // Meanwhile the host moves the same path on. This is the moment the
    // guarantee is worth something: canonical content the guest is entitled to
    // must not be written over bytes only the guest holds.
    let replacement = blob_bytes(9, ORDINARY);
    write_bytes(&session.host.repo, "media/cut.mov", &replacement);
    session
        .host
        .wait_for_status("the host's edit to be accepted", LONG, |v| {
            v["outbox_pending"] == 0 && v["live_revision"].as_u64().unwrap_or(0) >= 2
        });

    // Given plenty of time to get it wrong.
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(
        std::fs::read(session.guests[0].repo.join("media/cut.mov")).unwrap(),
        grown,
        "canonical content was written over a local file Weave never captured"
    );

    // The divergence is explicit and blocks publication rather than being
    // committed.
    let refusal = refused_prepare(&session.host);
    assert!(
        refusal.contains("media/cut.mov"),
        "the divergence did not block publication:\n{refusal}"
    );

    // Resolved the ordinary way, and the canonical content the guest was owed
    // arrives as soon as it stops being held back.
    write_bytes(
        &session.guests[0].repo,
        "media/cut.mov",
        &blob_bytes(10, ORDINARY),
    );
    wait_for_no_oversize(&session.guests[0], LONG);
    wait_for_agreement(&session, LONG);
    session.host.json(&["commit", "prepare"]);

    session.stop();
}
