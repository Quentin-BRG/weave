# Specification coverage

Where each part of the Weave V1 specification lives in this implementation. Section
numbers are the specification's.

| §  | Requirement | Implementation |
| --- | --- | --- |
| 2–4 | Product definition and boundaries | `README.md`, `docs/ARCHITECTURE.md` |
| 5–6 | One host coordinator, no P2P | `src/host.rs`, `src/daemon.rs` (loopback pair for the host's own replica) |
| 7.1–7.2 | One canonical state, one total revision order | `src/store_host.rs::commit_revision` |
| 7.3 | No silent data loss | `src/client.rs::capture_path`, `materialize_if_safe` |
| 7.4 | Canonical state is valid filesystem state | `src/reconcile.rs` (conflicted merge output discarded) |
| 7.5 | Every revision reconstructible | `src/store_host.rs::manifest_at` |
| 7.6 | Git and live state separate | `src/host.rs::create_commit`, `gitx::read_tree_into_index` |
| 8 | Rust stable, one binary, Git delegated | `Cargo.toml`, `src/gitx.rs` |
| 9 | Host and participant roles | `src/model.rs::Role`, `src/daemon.rs` |
| 10 | Host repository prerequisites | `src/daemon.rs::verify_new_host_repository` |
| 11 | Joining prerequisites | `src/daemon.rs::run_join`, `src/host.rs::on_hello` |
| 12 | Unsupported Git features | `src/gitx.rs::detect_unsupported`, `check_attr` |
| 13 | Git authority rule | `docs/`, `AGENTS.md` block, plugin skills |
| 14 | Git mutation detection | `src/host.rs::check_git_state`, `src/client.rs::check_git_state` |
| 15 | Weave metadata under `.git/weave` | `src/session.rs::Paths` |
| 16 | Content-addressed blob store | `src/blobs.rs` |
| 17–19 | File entry, file kind, manifest | `src/model.rs`, `src/store_host.rs` |
| 20–21 | Revision record, no-op operations | `src/model.rs::Revision`, `src/host.rs::try_apply` |
| 22–23 | Full-content file operations | `src/model.rs::FileOperation`, `src/proto.rs` |
| 24 | Operation identity and idempotency | `src/store_host.rs::lookup_operation`, `FileOperation::payload_hash` |
| 25 | `base_revision` validation | `src/store_host.rs::validate_base`, `historical_entry` |
| 26 | Actor identity bound to connection | `src/host.rs::on_submit` |
| 27 | Persistent actor identity, Git identity | `src/session.rs` |
| 28–29 | Local daemon model and IPC | `src/ipc.rs`, `src/daemon.rs::start_ipc` |
| 30 | Single daemon lock | `src/session.rs::DaemonLock` |
| 31–33 | Watcher, rescan, debounce | `src/watch.rs`, `src/client.rs::full_rescan` |
| 34 | Local sequence number | `src/store_client.rs::next_local_seq` |
| 35 | Persistent local outbox | `src/store_client.rs` |
| 36 | Capture-before-overwrite | `src/client.rs::materialize_if_safe` |
| 37 | Per-path logical state | `src/store_client.rs::PathState` |
| 38 | One in-flight operation per path | `src/client.rs::capture_path` |
| 39 | Incoming revisions with local work | `src/client.rs::apply_revision` |
| 40–41 | Continuation rebase | `src/client.rs::continuation_rebase` |
| 42 | Continuation rebase conflict | `src/client.rs::continuation_rebase`, `ReportConflict` |
| 43 | Host rejection with newer local work | `src/client.rs::on_conflicted`, `AttachLocalCandidate` |
| 44 | Remote-write watcher suppression | `src/client.rs::capture_path` (compares against `materialized`) |
| 45 | Safe materialization | `src/util.rs::write_atomic`, `src/scan.rs` |
| 46 | Ignore behaviour via Git | `src/gitx.rs::list_repository_paths`, `filter_ignored` |
| 47–48 | Canonical and portable paths | `src/path.rs` |
| 49 | Symlink / junction safety | `src/path.rs::ensure_no_indirection` |
| 50 | Rename = delete + create | `src/client.rs` (no rename operation exists) |
| 51 | File limits | `src/model.rs`, `src/scan.rs::read_path` |
| 52–54 | Transport, serialization, protocol version | `src/proto.rs`, `src/daemon.rs` |
| 55–57 | Session identity, invites, secure entry | `src/session.rs`, `src/cli.rs::read_invite` |
| 58 | Remote authentication and end-to-end encryption | `src/secure.rs` (Noise handshake, PSK derivation, framing), `src/daemon.rs::host_handshake`, `src/daemon.rs::connect_once` |
| 59–61 | Quick Tunnel, dependency, existing config | `src/tunnel.rs` |
| 62 | Tunnel lifecycle | `src/daemon.rs::restart_tunnel` |
| 63 | LAN mode | `src/daemon.rs::host_async` |
| 64 | Host connection model | `src/daemon.rs` |
| 65 | Backpressure | `src/transport.rs::Outbound` |
| 66 | Maximum message size | `src/model.rs::MAX_PROTOCOL_MESSAGE` |
| 67 | Heartbeats | `src/client.rs::on_tick`, `src/host.rs` |
| 68–70 | Persistence ordering, ACK meaning, broadcast after durability | `src/store_host.rs::commit_revision`, `src/host.rs::try_apply` |
| 71–81 | Reconciliation matrix | `src/reconcile.rs` |
| 82–84 | Conflict object, durability, working-tree behaviour | `src/model.rs::Conflict`, `src/host.rs`, `src/client.rs` |
| 85 | Conflict draft mode | `src/client.rs::capture_path`, `on_conflicted` |
| 86 | Conflict CLI | `src/cli.rs`, `src/client_ipc.rs` |
| 87–88 | Atomic resolution, `ResolutionOutdated` | `src/host.rs::on_resolve_conflict` |
| 89–97 | Tasks, scopes, soft locks, overlap, staleness, touched paths | `src/model.rs::Task`, `src/host.rs`, `src/client_ipc.rs` |
| 98–100 | Control state, control version, snapshots | `src/proto.rs::ControlSnapshot`, `src/host.rs::broadcast_control` |
| 101–102 | Join snapshot and transfer | `src/host.rs::on_hello`, `src/client.rs::apply_manifest` |
| 103–106 | Reconnection and replay | `src/proto.rs::ClientResumeState`, `src/host.rs::on_hello` |
| 105 | Contiguous watermark | `src/client.rs::drain_pending_revisions` |
| 107–108 | Replica state hash and divergence | `src/proto.rs::state_hash`, `src/host.rs` |
| 109–111 | Commit terminology, publication concept, `commit prepare` | `src/cli.rs`, `src/host.rs::on_commit_prepare` |
| 112–115 | Barrier | `src/host.rs::BarrierState`, `src/client.rs::on_barrier_start` |
| 116 | Disconnected participants warning | `src/host.rs::build_preparation` |
| 117–119 | Active Task rule, unassigned edits | `src/host.rs::build_preparation` |
| 120–121 | Prepare object and `--json` | `src/model.rs::CommitPreparation`, `PreparedTask` |
| 122–124 | Semantic message, `commit create`, who may request | `src/cli.rs`; skill in [weave-plugin](https://github.com/Quentin-BRG/weave-plugin) |
| 125–128 | Host-only Git construction, plumbing | `src/host.rs::create_commit`, `src/gitx.rs` |
| 129–130 | Commit descriptor, author and co-authors | `src/model.rs::CommitDescriptor`, `src/host.rs::coauthor_trailers` |
| 131–132 | Object propagation and verification | `src/gitx.rs::pack_objects`/`unpack_objects`, `src/client.rs::apply_publication` |
| 133–134 | Guarded ref update, index update | `src/gitx.rs::update_ref_cas`, `read_tree_into_index` |
| 135 | Ref/index crash journal | `src/store_client.rs::pub_journal`, `src/client.rs::repair_publications` |
| 136–137 | Publication record and broadcast | `src/store_host.rs::publications`, `src/host.rs::send_publication` |
| 138–140 | Push authority, `weave push`, divergence | `src/host.rs::on_push`, `src/gitx.rs::push` |
| 141–143 | Host crash recovery, resume tunnel, crash before broadcast | `src/daemon.rs::run_resume` |
| 144–146 | Disk-full, missing blob, `weave recover` | `src/host.rs`, `src/blobs.rs`, `src/recover.rs` |
| 147–150 | Participant recovery, disconnect, host absence | `src/client.rs`, `src/daemon.rs::client_connection_loop` |
| 151–157 | CLI surface, JSON, `status`, `peers`, `doctor`, errors | `src/cli.rs`, `src/doctor.rs`, `src/error.rs` |
| 158–160 | Plugin and skills, no MCP | [weave-plugin](https://github.com/Quentin-BRG/weave-plugin): portable Agent Plugins v1.0.0 package plus a Codex compatibility layer, each validated independently there |
| 161–163 | `AGENTS.md` bootstrap | `src/bootstrap.rs` |
| 164–169 | Skill contents | [weave-plugin](https://github.com/Quentin-BRG/weave-plugin) `skills/*/SKILL.md`; the raw Git prohibition (§165) and the host-only commit rule (§169) are asserted by its validators |
| 170 | Agent-independent design | CLI is the only integration surface |
| 171–174 | Security model, secrets, transport, logging | `docs/SECURITY.md`, `src/session.rs`, `src/util.rs` |
| 175–176 | Retention and snapshot equivalence | `src/store_host.rs::manifest_at` |
| 198 | Git removability | `tests/single_host.rs` |
| 199 | Performance envelope | full-content transfer, per-batch ignore checks |

## Correctness requirements (§177–197)

| §  | Requirement | Test |
| --- | --- | --- |
| 177 | No lost local edit while an operation is in flight | `src/client.rs` continuation rebase; `tests/multi_participant.rs` |
| 178 | Independent concurrent edits merge | `reconcile::tests::independent_edits_merge_cleanly`, `tests/multi_participant.rs::independent_concurrent_edits_merge_without_a_conflict` |
| 179 | Overlapping edits conflict, both candidates preserved, no markers | `reconcile::tests::overlapping_edits_conflict`, `tests/multi_participant.rs::overlapping_edits_become_an_explicit_conflict_that_can_be_resolved` |
| 180 | Concurrent create converges or conflicts | `reconcile::tests::concurrent_create_*` |
| 181 | Modify/delete conflicts in both directions | `reconcile::tests::modify_delete_conflicts_both_directions` |
| 182–183 | Duplicate packet, duplicate ID mutation | `store_host::lookup_operation` + `payload_hash` comparison in `host::on_submit` |
| 184 | No watcher echo | `client::capture_path` compares against `materialized` |
| 185 | Missed watcher event found by rescan | `tests/single_host.rs::resume_recovers_local_work_captured_while_the_daemon_was_down` |
| 186 | Reconnect merges or conflicts, never discards | `tests/multi_participant.rs` (both offline-edit tests) |
| 187 | Control state after reconnect | full control snapshot on every reconnect |
| 188–189 | Historical Git publication, post-publication working tree | `tests/single_host.rs::host_captures_edits_and_publishes_a_historical_revision`, `tests/multi_participant.rs::a_participant_can_request_a_publication_the_host_builds_and_distributes` |
| 190 | Active Task blocks preparation | both publication tests |
| 191 | Distinct preparation identity | `CommitPreparation::prepare_id`, required by `commit create` |
| 192 | Clients install exact host objects | `tests/multi_participant.rs` (tree OID equality on both machines) |
| 193–194 | Crash before / after ACK | persistence ordering in `store_host::commit_revision` |
| 195 | Crash during publication | `store_client::incomplete_publications`, `client::repair_publications` |
| 196 | Late join converges | `tests/multi_participant.rs::start_session` |
| 197 | Backpressure bounded | `transport::Outbound` |

## Plugin packaging

Sections 158-169 are implemented in a separate repository,
[Quentin-BRG/weave-plugin](https://github.com/Quentin-BRG/weave-plugin), because the
skills version and install on their own schedule: they are instructions about a CLI,
this repository is the CLI.

That repository carries the portable
[Agent Plugins v1.0.0](https://agent-plugins.org/specification) package as its source
of truth, with Codex packaging as a compatibility layer around the same canonical
skills. It validates the two formats independently — a Codex validator passing says
nothing about portable conformance — and additionally asserts provider neutrality,
the raw Git prohibition (§165) and the host-only publication rule (§169).

Nothing plugin-specific remains here. When a CLI command, flag or `--json` field
changes, the skills in that repository may need the same change.

## The encrypted transport

`src/secure.rs` carries unit tests for the handshake and the framing in isolation:
matching secrets round-trip, a wrong secret and a secret for another session both
fail, tampered ciphertext and truncated or malformed frames are rejected, traffic
from one connection is invalid on a fresh one, frames cannot be replayed or
reordered, oversized messages chunk and reassemble while a dropped chunk fails
rather than truncating, and PSK derivation is deterministic and domain-separated.

`tests/encrypted_transport.rs` checks the same claims against the actual bytes on a
socket. It runs a recording proxy between two real daemons and asserts that no
repository sentinel — file content, file path, Task description, commit message —
appears anywhere in the captured frames; that a participant with the wrong secret
receives no state and cannot mutate anything; that a single flipped bit in one frame
is rejected and the session recovers; and that every reconnect performs a fresh
handshake. One test exists to keep the others honest: it serializes the exact
message the socket carried before this change and asserts the scan finds it, so the
no-plaintext assertion cannot pass vacuously.

## The real remote path

`tests/multi_participant.rs` exercises the protocol over a real WebSocket, but on
loopback. `tests/remote_tunnel.rs` covers what only the public path can:

| Requirement | Covered by |
| --- | --- |
| §52 one long-lived WebSocket per participant, over the tunnel | the whole test |
| §53 file content, including binary, across the wire | 12 MiB binary asset on the data plane |
| §56–57 invite reaches and authenticates a remote participant | join from the invite alone |
| §58 TLS transport; `wss://…trycloudflare.com`, never loopback | `assert_public_endpoint` |
| §58 the Noise session survives a real TLS-terminating intermediary | every assertion in the test runs inside it |
| §58 a wrong session secret is refused through the live tunnel | step 3b: forged invite, same URL, no state and no online connection |
| §59–60 Quick Tunnel launch and `cloudflared` dependency | `weave host` with no flags |
| §62 tunnel identity is not session identity | `weave tunnel restart` keeps `session_id`, canonical state and conflicts, and yields a new URL and invite |
| §131–132 Git pack distribution over the public path | tree OID equality on both machines |
| §148, §186 queued work survives losing the endpoint | edit while the old tunnel is dead, then re-join |

It is `#[ignore]`d and additionally has its own workflow
(`.github/workflows/remote-tunnel.yml`, manual plus weekly), so a Cloudflare
outage can never redden an unrelated pull request:

```
cargo test --test remote_tunnel -- --ignored --test-threads=1 --nocapture
```

## Deliberate implementation choices

Where the specification leaves a detail open, these are the choices made and why:

- **Two SQLite files.** `host.sqlite` for canonical state, `state.sqlite` for the
  replica, so the two engines never contend on one connection. Blobs are shared, so
  content is stored once.
- **Full control snapshots.** Control state is small, so any change broadcasts a
  complete snapshot instead of an event log. This removes gap-tracking entirely and
  satisfies §99/§100 directly.
- **Loopback pair for the host's own replica.** Identical JSON frames rather than a
  shortcut path, so the host cannot drift from participant behaviour.
- **UTC commit timestamps.** Commit times use `+0000` rather than a local offset:
  deterministic, and it avoids the known unsoundness of reading the local timezone
  from a multithreaded process.
- **Conflict draft mode is scoped to the owning participant.** Only the machine
  whose candidate was rejected enters draft mode, so an unrelated participant is
  never frozen out of a file.
- **Idempotent retransmission on a live connection.** An in-flight operation with no
  result after 20 seconds is resent with the same `operation_id`, which §24 makes
  safe. This turns a lost frame into a delay rather than a stall.
- **`ring` as the pinned TLS provider.** `rustls` 0.23 will not pick one implicitly,
  and `aws-lc-rs` needs cmake and NASM on Windows. Weave installs `ring` explicitly
  rather than relying on feature unification to do it.
- **The network task is supervised.** It is an infinite loop, so if its task ends it
  ended abnormally; restarting is free because the outbox is durable and operations
  are idempotent, whereas not restarting strands the participant offline forever.
