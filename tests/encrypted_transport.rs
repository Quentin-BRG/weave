// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Security properties of the encrypted Weave transport, tested end to end.
//!
//! These tests put a recording TCP proxy between a real host daemon and a real
//! participant daemon, so what is inspected is the actual bytes Weave puts on a
//! socket — not a reconstruction. The participant is given an invite pointing
//! at the proxy, which is its only route to the host, so nothing can bypass the
//! capture.
//!
//! The session runs in LAN mode (`ws://`), which is deliberate: it proves the
//! application payload is encrypted even when the transport underneath is not.

mod common;

use common::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use weave::session::{decode_invite, encode_invite, InvitePayload};

// ---------------------------------------------------------------------------
// A recording, optionally sabotaging, TCP proxy
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Capture {
    /// Every byte seen, per direction, including the HTTP upgrade.
    to_host: Vec<u8>,
    to_client: Vec<u8>,
    /// Payloads of WebSocket binary frames, which is where Weave lives.
    host_bound_frames: Vec<Vec<u8>>,
    client_bound_frames: Vec<Vec<u8>>,
    /// The first host-bound binary frame of each TCP connection: the Noise
    /// handshake message, and therefore the initiator's ephemeral public key.
    first_frames: Vec<Vec<u8>>,
    text_frames: usize,
    connections: usize,
}

impl Capture {
    fn all_bytes(&self) -> Vec<u8> {
        let mut all = self.to_host.clone();
        all.extend_from_slice(&self.to_client);
        all
    }
}

/// Flip one bit in the payload of the n-th host-bound binary frame, once.
#[derive(Clone, Copy)]
enum Sabotage {
    None,
    FlipBitInHostBoundFrame(usize),
}

struct WireTap {
    port: u16,
    capture: Arc<Mutex<Capture>>,
    stop: Arc<AtomicBool>,
}

impl WireTap {
    fn start(upstream: u16, sabotage: Sabotage) -> WireTap {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind wiretap");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let capture = Arc::new(Mutex::new(Capture::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let frame_counter = Arc::new(AtomicUsize::new(0));

        let tap = WireTap {
            port,
            capture: capture.clone(),
            stop: stop.clone(),
        };

        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((downstream, _)) => {
                        let Ok(upstream) = TcpStream::connect(("127.0.0.1", upstream)) else {
                            continue;
                        };
                        // Accepted sockets inherit the listener's non-blocking
                        // mode on Windows; the pumps below need blocking reads.
                        downstream.set_nonblocking(false).ok();
                        upstream.set_nonblocking(false).ok();
                        downstream.set_nodelay(true).ok();
                        upstream.set_nodelay(true).ok();
                        capture.lock().unwrap().connections += 1;

                        let to_host = (
                            downstream.try_clone().unwrap(),
                            upstream.try_clone().unwrap(),
                        );
                        let to_client = (upstream, downstream);
                        let (cap, counter) = (capture.clone(), frame_counter.clone());
                        std::thread::spawn(move || {
                            pump(
                                to_host.0,
                                to_host.1,
                                cap,
                                Direction::ToHost,
                                counter,
                                sabotage,
                            )
                        });
                        let cap = capture.clone();
                        std::thread::spawn(move || {
                            pump(
                                to_client.0,
                                to_client.1,
                                cap,
                                Direction::ToClient,
                                Arc::new(AtomicUsize::new(0)),
                                Sabotage::None,
                            )
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        tap
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}{}", self.port, weave::transport::WS_PATH)
    }
}

impl Drop for WireTap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Direction {
    ToHost,
    ToClient,
}

fn pump(
    mut from: TcpStream,
    mut to: TcpStream,
    capture: Arc<Mutex<Capture>>,
    direction: Direction,
    counter: Arc<AtomicUsize>,
    sabotage: Sabotage,
) {
    // The HTTP upgrade first, verbatim.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match from.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => head.push(byte[0]),
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 16 * 1024 {
            return;
        }
    }
    record(&capture, direction, &head);
    if to.write_all(&head).is_err() {
        return;
    }

    let mut first_seen = false;
    while let Ok(Some(frame)) = read_ws_frame(&mut from) {
        let mut raw = frame.raw;
        if frame.opcode == 0x2 {
            let payload = raw[frame.payload_offset..].to_vec();
            {
                let mut cap = capture.lock().unwrap();
                match direction {
                    Direction::ToHost => {
                        if !first_seen {
                            cap.first_frames.push(payload.clone());
                            first_seen = true;
                        }
                        cap.host_bound_frames.push(payload)
                    }
                    Direction::ToClient => cap.client_bound_frames.push(payload),
                }
            }
            if let Sabotage::FlipBitInHostBoundFrame(target) = sabotage {
                let index = counter.fetch_add(1, Ordering::Relaxed);
                if index == target && raw.len() > frame.payload_offset {
                    // XOR passes straight through WebSocket masking, so this
                    // flips exactly one plaintext-position bit of the Noise
                    // ciphertext without having to unmask it.
                    raw[frame.payload_offset] ^= 0x01;
                }
            }
        } else if frame.opcode == 0x1 {
            capture.lock().unwrap().text_frames += 1;
        }
        record(&capture, direction, &raw);
        if to.write_all(&raw).is_err() {
            return;
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Both);
}

fn record(capture: &Arc<Mutex<Capture>>, direction: Direction, bytes: &[u8]) {
    let mut cap = capture.lock().unwrap();
    match direction {
        Direction::ToHost => cap.to_host.extend_from_slice(bytes),
        Direction::ToClient => cap.to_client.extend_from_slice(bytes),
    }
}

struct Frame {
    raw: Vec<u8>,
    opcode: u8,
    payload_offset: usize,
}

/// Minimal RFC 6455 frame reader: enough to find payload boundaries.
fn read_ws_frame(stream: &mut TcpStream) -> std::io::Result<Option<Frame>> {
    let mut head = [0u8; 2];
    if let Err(e) = stream.read_exact(&mut head) {
        return if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Ok(None)
        } else {
            Err(e)
        };
    }
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut raw = head.to_vec();

    let length = match head[1] & 0x7f {
        126 => {
            let mut b = [0u8; 2];
            stream.read_exact(&mut b)?;
            raw.extend_from_slice(&b);
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            stream.read_exact(&mut b)?;
            raw.extend_from_slice(&b);
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    if masked {
        let mut key = [0u8; 4];
        stream.read_exact(&mut key)?;
        raw.extend_from_slice(&key);
    }
    let payload_offset = raw.len();
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload)?;
    raw.extend_from_slice(&payload);
    Ok(Some(Frame {
        raw,
        opcode,
        payload_offset,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rewrite an invite so it points at the proxy, keeping everything else.
fn invite_via(original: &str, url: String, secret: Option<String>) -> String {
    let payload = decode_invite(original).expect("decode invite");
    encode_invite(&InvitePayload {
        url,
        secret: secret.unwrap_or(payload.secret),
        ..payload
    })
    .expect("encode invite")
}

fn upstream_port(endpoint: &str) -> u16 {
    endpoint
        .rsplit_once(':')
        .and_then(|(_, rest)| rest.split('/').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("could not read a port from {endpoint}"))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn write_invite(sandbox: &Sandbox, name: &str, invite: &str) -> String {
    let path = sandbox.root.join(name);
    std::fs::write(&path, invite).unwrap();
    path.to_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// 1. Nothing sensitive reaches the wire
// ---------------------------------------------------------------------------

const SECRET_CONTENT: &str = "SUPER_SECRET_WEAVE_FILE_CONTENT_c4f19a2b\n";
const SECRET_PATH: &str = "private/sentinel-file-name-9d13ea.txt";
const SECRET_TASK: &str = "SECRET_TASK_DESCRIPTION_7b2c55d1";
const SECRET_MESSAGE: &str = "SECRET_COMMIT_MESSAGE_e08fa413";

#[test]
fn no_repository_content_appears_in_the_websocket_frames() {
    let sandbox = Sandbox::new("wire");
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");
    let mut guest = clone_participant(&sandbox, &host, "beta", "Alice", "alice@example.com");

    host.start_daemon(&["host", "--lan"]);
    host.wait_online(SHORT);
    let invite = host.wait_for_invite(SHORT);
    let endpoint = invite["endpoint"].as_str().unwrap().to_string();

    let tap = WireTap::start(upstream_port(&endpoint), Sabotage::None);
    let rewritten = invite_via(invite["invite"].as_str().unwrap(), tap.url(), None);
    let path = write_invite(&sandbox, "invite.txt", &rewritten);
    guest.start_daemon(&["join", "--invite-file", &path]);
    guest.wait_online(SHORT);

    // Content, a path, a Task description and a commit message all cross the
    // proxy in both directions.
    write_file(&host.repo, SECRET_PATH, SECRET_CONTENT);
    guest.wait_for_file(SECRET_PATH, SECRET_CONTENT, SHORT);

    write_file(&guest.repo, "slides/02-guest.md", SECRET_CONTENT);
    host.wait_for_file("slides/02-guest.md", SECRET_CONTENT, SHORT);

    guest.expect(&[
        "task",
        "start",
        "--description",
        SECRET_TASK,
        "--file",
        "slides/02-guest.md",
    ]);
    let task_id = guest.wait_for_active_task(SHORT);
    guest.expect(&["task", "complete", &task_id]);

    let prepare = guest.json(&["commit", "prepare"]);
    let prepare_id = prepare["prepare_id"].as_str().unwrap().to_string();
    guest.expect(&["commit", "create", &prepare_id, "--message", SECRET_MESSAGE]);
    guest.wait_for_status("the session to settle", SHORT, |v| v["outbox_pending"] == 0);

    let capture = tap.capture.lock().unwrap();
    let wire = capture.all_bytes();

    // The sentinels demonstrably crossed this proxy: the waits above only
    // returned because each side received the other's content, and the proxy is
    // the participant's only route to the host. The size and frame counts guard
    // against a capture that silently stopped recording partway through.
    assert!(
        wire.len() > 8_000,
        "the proxy captured only {} bytes; the session did not really run through it",
        wire.len()
    );
    assert!(
        capture.host_bound_frames.len() > 5 && capture.client_bound_frames.len() > 5,
        "expected real traffic in both directions, saw {} up and {} down",
        capture.host_bound_frames.len(),
        capture.client_bound_frames.len()
    );
    assert_eq!(
        capture.text_frames, 0,
        "Weave must never send a WebSocket text frame; the protocol is binary and encrypted"
    );

    for (label, sentinel) in [
        ("file content", SECRET_CONTENT),
        ("file path", SECRET_PATH),
        ("Task description", SECRET_TASK),
        ("commit message", SECRET_MESSAGE),
    ] {
        assert!(
            !contains(&wire, sentinel.as_bytes()),
            "the {label} sentinel appeared in plaintext on the wire"
        );
    }

    // The session secret itself never crosses the network in any form.
    let secret = decode_invite(invite["invite"].as_str().unwrap())
        .unwrap()
        .secret;
    assert!(
        !contains(&wire, secret.as_bytes()),
        "the session secret appeared on the wire"
    );
    let psk = weave::secure::derive_psk(
        &secret,
        decode_invite(invite["invite"].as_str().unwrap())
            .unwrap()
            .session_id,
    );
    assert!(
        !contains(&wire, psk.as_ref()),
        "the derived Noise pre-shared key appeared on the wire"
    );

    drop(capture);
    guest.stop_daemon();
    host.stop_daemon();
}

/// The scan above is only meaningful if it would catch plaintext. This feeds it
/// exactly what a build without the encryption layer would have written to the
/// socket: the same `ClientEnvelope`, serialized by the same code, as a
/// WebSocket text frame.
#[test]
fn the_wire_scan_detects_plaintext_when_it_is_there() {
    use base64::Engine as _;
    use weave::model::{FileEntry, FileOperation, GitMode};
    use weave::path::RepoPath;
    use weave::proto::{ClientEnvelope, ClientMessage};

    let content = base64::engine::general_purpose::STANDARD.encode(SECRET_CONTENT);
    let envelope = ClientEnvelope::wrap(ClientMessage::SubmitOperation {
        operation: Box::new(FileOperation {
            operation_id: uuid::Uuid::new_v4(),
            actor_id: uuid::Uuid::new_v4(),
            task_id: None,
            local_seq: 1,
            base_revision: 1,
            base_entry: None,
            path: RepoPath::new(SECRET_PATH).unwrap(),
            desired_entry: Some(FileEntry::from_bytes(
                SECRET_CONTENT.as_bytes(),
                GitMode::Regular,
            )),
            content_b64: Some(content.clone()),
        }),
    });
    let plaintext = serde_json::to_vec(&envelope).unwrap();

    // This is byte-for-byte what the socket carried before the encryption layer
    // existed, and the scan catches it immediately.
    assert!(
        contains(&plaintext, SECRET_PATH.as_bytes()),
        "the scan must find a path that really is present"
    );
    assert!(
        contains(&plaintext, content.as_bytes()),
        "the scan must find the encoded content that really is present"
    );
    // And it does not raise a false alarm on an unrelated value.
    assert!(!contains(&plaintext, b"NOT_IN_THIS_MESSAGE_c0ffee"));
}

// ---------------------------------------------------------------------------
// 2. A wrong secret learns nothing and changes nothing
// ---------------------------------------------------------------------------

#[test]
fn a_participant_with_the_wrong_secret_gets_no_state_and_cannot_mutate() {
    let sandbox = Sandbox::new("wrongkey");
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");
    let mut impostor = clone_participant(&sandbox, &host, "beta", "Mallory", "mallory@example.com");

    host.start_daemon(&["host", "--lan"]);
    host.wait_online(SHORT);
    let invite = host.wait_for_invite(SHORT);
    let endpoint = invite["endpoint"].as_str().unwrap().to_string();

    // Host-only content the impostor must never see.
    write_file(&host.repo, SECRET_PATH, SECRET_CONTENT);
    host.wait_for_status("the host to capture its edit", SHORT, |v| {
        v["outbox_pending"] == 0 && v["live_revision"].as_u64().unwrap_or(0) > 0
    });

    let wrong = "0".repeat(64);
    let tap = WireTap::start(upstream_port(&endpoint), Sabotage::None);
    let rewritten = invite_via(
        invite["invite"].as_str().unwrap(),
        tap.url(),
        Some(wrong.clone()),
    );
    let path = write_invite(&sandbox, "bad-invite.txt", &rewritten);
    impostor.start_daemon(&["join", "--invite-file", &path]);

    // Give it a generous window to fail repeatedly rather than once.
    std::thread::sleep(Duration::from_secs(8));

    let status = impostor.status();
    assert_ne!(
        status["connection"].as_str(),
        Some("online"),
        "a wrong secret must never reach an online session: {status}"
    );
    assert_eq!(
        status["live_revision"].as_u64().unwrap_or(0),
        0,
        "no canonical revision may be disclosed: {status}"
    );
    assert!(
        !file_exists(&impostor.repo, SECRET_PATH),
        "host-only content must not reach an unauthenticated peer"
    );

    // Nothing it writes locally can reach the host either.
    write_file(&impostor.repo, "injected.md", "MALLORY_WAS_HERE\n");
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !file_exists(&host.repo, "injected.md"),
        "an unauthenticated peer must not be able to mutate the session"
    );

    // The host still sees only itself.
    let peers = host.json(&["peers"]);
    let online = peers["peers"]
        .as_array()
        .map(|p| p.iter().filter(|p| p["online"] == true).count())
        .unwrap_or(0);
    assert_eq!(online, 1, "only the host should be present: {peers}");

    // The handshake really was attempted and really was refused.
    let capture = tap.capture.lock().unwrap();
    assert!(
        capture.connections > 0,
        "the impostor never even reached the proxy"
    );
    assert!(
        !capture.first_frames.is_empty(),
        "the impostor never sent a handshake message"
    );
    assert!(
        capture.client_bound_frames.is_empty(),
        "the host answered an unauthenticated peer with {} application frames",
        capture.client_bound_frames.len()
    );
    drop(capture);

    assert!(
        host.daemon_output().contains("failed the Weave handshake"),
        "the host should log a refused handshake:\n{}",
        host.daemon_output()
    );
    host.assert_daemon_healthy();

    impostor.stop_daemon();
    host.stop_daemon();
}

// ---------------------------------------------------------------------------
// 3. Tampering, and recovery afterwards
// ---------------------------------------------------------------------------

#[test]
fn a_tampered_frame_is_rejected_and_the_session_recovers() {
    let sandbox = Sandbox::new("tamper");
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");
    let mut guest = clone_participant(&sandbox, &host, "beta", "Alice", "alice@example.com");

    host.start_daemon(&["host", "--lan"]);
    host.wait_online(SHORT);
    let invite = host.wait_for_invite(SHORT);
    let endpoint = invite["endpoint"].as_str().unwrap().to_string();

    // Frame 0 is the handshake; frame 2 is well inside the transport phase.
    let tap = WireTap::start(
        upstream_port(&endpoint),
        Sabotage::FlipBitInHostBoundFrame(2),
    );
    let rewritten = invite_via(invite["invite"].as_str().unwrap(), tap.url(), None);
    let path = write_invite(&sandbox, "invite.txt", &rewritten);
    guest.start_daemon(&["join", "--invite-file", &path]);
    guest.wait_online(SHORT);

    // Keep writing until the sabotaged frame has certainly gone past.
    for i in 0..6 {
        write_file(&guest.repo, "slides/02-guest.md", &format!("edit {i}\n"));
        std::thread::sleep(Duration::from_millis(400));
    }
    let settled = "edit 5\n";
    write_file(&guest.repo, "slides/02-guest.md", settled);

    // The durable outbox and idempotent operations carry the work across the
    // dropped connection and the fresh handshake that follows it.
    host.wait_for_file("slides/02-guest.md", settled, LONG);
    guest.wait_for_file("slides/02-guest.md", settled, LONG);

    let host_log = host.daemon_output();
    assert!(
        host_log.contains("failed authentication") || host_log.contains("dropping a Weave"),
        "the host should have rejected the altered frame:\n{host_log}"
    );
    host.assert_daemon_healthy();
    guest.assert_daemon_healthy();

    // More than one connection means the tamper really did break the first one.
    let capture = tap.capture.lock().unwrap();
    assert!(
        capture.connections >= 2,
        "the altered frame should have cost the connection, saw {}",
        capture.connections
    );
    drop(capture);

    guest.stop_daemon();
    host.stop_daemon();
}

// ---------------------------------------------------------------------------
// 4. Every connection gets fresh keys
// ---------------------------------------------------------------------------

#[test]
fn every_reconnect_performs_a_fresh_handshake_with_new_ephemeral_keys() {
    let sandbox = Sandbox::new("rehandshake");
    let mut host = Participant::new(&sandbox, "alpha");
    init_repo(&host.repo, "Quentin", "quentin@example.com");
    let mut guest = clone_participant(&sandbox, &host, "beta", "Alice", "alice@example.com");

    host.start_daemon(&["host", "--lan"]);
    host.wait_online(SHORT);
    let invite = host.wait_for_invite(SHORT);
    let endpoint = invite["endpoint"].as_str().unwrap().to_string();

    let tap = WireTap::start(upstream_port(&endpoint), Sabotage::None);
    let rewritten = invite_via(invite["invite"].as_str().unwrap(), tap.url(), None);
    let path = write_invite(&sandbox, "invite.txt", &rewritten);

    guest.start_daemon(&["join", "--invite-file", &path]);
    guest.wait_online(SHORT);
    write_file(&guest.repo, "slides/02-guest.md", "first connection\n");
    host.wait_for_file("slides/02-guest.md", "first connection\n", SHORT);

    // Offline work, then a reconnect that must renegotiate from scratch.
    guest.stop_daemon();
    write_file(&guest.repo, "slides/02-guest.md", "written while offline\n");
    guest.start_daemon(&["resume"]);
    guest.wait_online(SHORT);
    host.wait_for_file("slides/02-guest.md", "written while offline\n", LONG);

    let capture = tap.capture.lock().unwrap();
    assert!(
        capture.connections >= 2,
        "expected a second connection, saw {}",
        capture.connections
    );
    assert!(
        capture.first_frames.len() >= 2,
        "expected a handshake per connection, saw {}",
        capture.first_frames.len()
    );
    // Identical first frames would mean a replayed ephemeral key.
    for i in 1..capture.first_frames.len() {
        assert_ne!(
            capture.first_frames[0], capture.first_frames[i],
            "reconnect {i} reused the initial handshake message"
        );
    }
    // No captured application frame is byte-identical to another, which is what
    // fresh per-connection transport keys guarantee.
    let mut sorted = capture.host_bound_frames.clone();
    sorted.sort();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "a ciphertext frame repeated verbatim");
    drop(capture);

    guest.stop_daemon();
    host.stop_daemon();
}

// ---------------------------------------------------------------------------
// 5. What the encryption layer costs
// ---------------------------------------------------------------------------

/// Opt-in measurement, not an assertion about the machine it runs on.
///
/// ```text
/// cargo test --release --test encrypted_transport -- --ignored measure --nocapture
/// ```
#[test]
#[ignore = "measurement, not a pass/fail property; run with --ignored"]
fn measure_the_cost_of_the_encrypted_transport() {
    use std::time::Instant;
    use weave::secure::{derive_psk, Initiator, Responder};

    let session = uuid::Uuid::new_v4();
    let secret = "b".repeat(64);

    // --- handshake ---
    let psk = derive_psk(&secret, session);
    let rounds = 200;
    let start = Instant::now();
    for _ in 0..rounds {
        let mut initiator = Initiator::new(&psk, session).unwrap();
        let first = initiator.first_message().unwrap();
        let (reply, _host) = Responder::new(&psk, session)
            .unwrap()
            .respond(&first)
            .unwrap();
        let _client = initiator.finish(&reply).unwrap();
    }
    let per_handshake = start.elapsed() / rounds;
    println!("handshake (both sides, no network): {per_handshake:?}");

    let start = Instant::now();
    for _ in 0..rounds {
        let _ = derive_psk(&secret, session);
    }
    println!("psk derivation: {:?}", start.elapsed() / rounds);

    // --- small messages, the common case ---
    let (client, host) = {
        let mut initiator = Initiator::new(&psk, session).unwrap();
        let first = initiator.first_message().unwrap();
        let (reply, host) = Responder::new(&psk, session)
            .unwrap()
            .respond(&first)
            .unwrap();
        (initiator.finish(&reply).unwrap(), host)
    };
    let small = vec![b'x'; 512];
    let rounds = 20_000;
    let start = Instant::now();
    for _ in 0..rounds {
        let frames = client.encrypt(&small).unwrap();
        host.decrypt(&frames[0]).unwrap().unwrap();
    }
    println!(
        "512-byte message, encrypt + decrypt: {:?}",
        start.elapsed() / rounds
    );

    // --- a maximal V1 payload ---
    let big = vec![7u8; 10 * 1024 * 1024];
    let start = Instant::now();
    let frames = client.encrypt(&big).unwrap();
    let encrypt = start.elapsed();
    let start = Instant::now();
    for frame in &frames[..frames.len() - 1] {
        assert!(host.decrypt(frame).unwrap().is_none());
    }
    host.decrypt(frames.last().unwrap()).unwrap().unwrap();
    let decrypt = start.elapsed();
    let mib = big.len() as f64 / (1024.0 * 1024.0);
    println!(
        "10 MiB payload: encrypt {encrypt:?} ({:.0} MiB/s), decrypt {decrypt:?} ({:.0} MiB/s), \
         {} frames",
        mib / encrypt.as_secs_f64(),
        mib / decrypt.as_secs_f64(),
        frames.len()
    );
    let overhead = frames.iter().map(|f| f.len()).sum::<usize>() - big.len();
    println!(
        "ciphertext expansion: {overhead} bytes over {} ({:.4}%)",
        big.len(),
        overhead as f64 * 100.0 / big.len() as f64
    );

    // --- end to end, through two real daemons ---
    let sandbox = Sandbox::new("perf");
    let mut host_p = Participant::new(&sandbox, "alpha");
    init_repo(&host_p.repo, "Quentin", "quentin@example.com");
    let mut guest = clone_participant(&sandbox, &host_p, "beta", "Alice", "alice@example.com");

    host_p.start_daemon(&["host", "--lan"]);
    host_p.wait_online(SHORT);
    let invite = host_p.wait_for_invite(SHORT);
    let path = write_invite(&sandbox, "invite.txt", invite["invite"].as_str().unwrap());

    let joined = Instant::now();
    guest.start_daemon(&["join", "--invite-file", &path]);
    guest.wait_online(SHORT);
    println!(
        "join to online (process start included): {:?}",
        joined.elapsed()
    );

    let mut samples = Vec::new();
    for i in 0..10 {
        let text = format!("small edit {i}\n");
        let start = Instant::now();
        write_file(&guest.repo, "slides/01-intro.md", &text);
        host_p.wait_for_file("slides/01-intro.md", &text, SHORT);
        samples.push(start.elapsed());
    }
    samples.sort();
    println!(
        "small text edit, guest to host: median {:?}, min {:?}, max {:?}",
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1]
    );

    let blob: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let start = Instant::now();
    std::fs::write(guest.repo.join("large.bin"), &blob).unwrap();
    host_p.wait_for_bytes("large.bin", &blob, LONG);
    println!("8 MiB file, guest to host: {:?}", start.elapsed());

    guest.stop_daemon();
    host_p.stop_daemon();
}

// ---------------------------------------------------------------------------
// Shared setup
// ---------------------------------------------------------------------------

/// Clone the host repository into a second participant, as a real user would.
fn clone_participant(
    sandbox: &Sandbox,
    host: &Participant,
    name: &str,
    user: &str,
    email: &str,
) -> Participant {
    let guest = Participant::new(sandbox, name);
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
    git(&guest.repo, &["config", "user.name", user]);
    git(&guest.repo, &["config", "user.email", email]);
    git(&guest.repo, &["config", "commit.gpgsign", "false"]);
    guest
}
