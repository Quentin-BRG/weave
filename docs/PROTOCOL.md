# Weave V1 wire protocol

`protocol_version` is **1**. A peer that announces a different version is rejected.

## Transport

One long-lived WebSocket per participant, all logical messages multiplexed over it.
Remote sessions run through a Cloudflare Quick Tunnel, which supports WebSocket
upgrades; LAN sessions use a plain socket. The host coordinator binds only to
loopback in tunnel mode.

Serialization is JSON. File content travels as **complete bytes**, base64-encoded —
Weave V1 deliberately does not use textual deltas as the primary operation
representation. That removes patch-application ambiguity, simplifies recovery and
makes three-way reconciliation straightforward, at the cost of bandwidth Weave's
target workload can afford.

Maximum frame: 48 MiB (comfortably above a 10 MiB file after base64 expansion plus
overhead). Oversized messages are protocol errors.

## Authentication

The public Cloudflare hostname is **not** authentication. The client authenticates
with the session secret during the WebSocket upgrade:

```
Authorization: Bearer <session-secret>
```

The secret is compared in constant time. An unauthenticated client receives no
repository content: the upgrade is refused with `401`.

## Envelopes

Every frame is an object carrying `protocol_version` and `message_type`:

```json
{ "protocol_version": 1, "message_type": "submit_operation", "operation": { … } }
```

### Client → host

| `message_type` | Purpose |
| --- | --- |
| `hello` | Identity, branch, base commit and resume state. Must be the first frame. |
| `submit_operation` | One desired file state (§22) |
| `request_blobs` | Fetch content by SHA-256 |
| `request_manifest` | Ask for a fresh canonical snapshot (gap, divergence, invalid base) |
| `request_control_snapshot` | Ask for current Tasks / conflicts / publication |
| `report_conflict` | A conflict discovered during a local continuation rebase (§42) |
| `attach_local_candidate` | Attach the newest local candidate to a conflict (§43) |
| `resolve_conflict` | One atomic resolution with the expected canonical entry (§87) |
| `dismiss_conflict` | Close a conflict without changing canonical state |
| `task_start` / `task_update` / `task_complete` / `task_cancel` | Tasks |
| `commit_prepare` / `commit_create` | Git publication |
| `push_request` | Ask the host to push |
| `barrier_ack` / `barrier_ready` | Commit preparation barrier (§113–115) |
| `replica_hash` | Deterministic replica state hash for divergence detection (§108) |
| `presence` | Applied revision and active Task |
| `ping` / `pong` | Heartbeats |

### Host → client

| `message_type` | Purpose |
| --- | --- |
| `welcome` | Session info, snapshot revision, optional full manifest, control snapshot |
| `manifest_snapshot` | Fresh canonical manifest replacing the replica |
| `revision_broadcast` | One accepted revision, content inline |
| `operation_result` | The durable result for one `operation_id` |
| `blobs` | Requested content, batched by size |
| `control` | Full control snapshot (Tasks, conflicts, latest publication) |
| `presence` | Participant list |
| `barrier_start` / `barrier_end` | Commit preparation barrier |
| `publication` | Publication descriptor plus the exact Git objects as a pack |
| `prepare_result` / `commit_result` / `push_result` / `ack` | Command replies |
| `error` | Class, message and actionable detail |
| `host_state` | Host paused or degraded |
| `ping` / `pong` / `goodbye` | Liveness and shutdown |

## Revisions

Every accepted canonical filesystem mutation receives a monotonically increasing
`revision: u64`, assigned **only** by the host. A revision record keeps complete
before/after entry information, so creation, deletion, content change and
executable-mode change are all distinguishable and every retained revision is
reconstructible.

An operation that produces no canonical change consumes **no** revision, but still
receives a durable result bound to its `operation_id`.

## Operation identity

`operation_id` is a cryptographically random UUID. The host persists the result:

- retransmitted with an identical payload → the original result is returned;
- retransmitted with a different payload → protocol integrity error, never treated
  as a new operation.

This is what makes retransmission after a timeout or a reconnect safe.

## Base validation

The host does not trust `base_entry` alone. For every operation it verifies

```
manifest(base_revision)[path] == base_entry
```

by re-deriving the historical entry from its own revision log. A mismatch is
`ProtocolError::InvalidBase`; the operation is not merged. A client cannot select
an arbitrary merge base.

## Reconciliation matrix

With `B = manifest(base_revision)[P]`, `C = current canonical entry`,
`I = incoming desired entry`:

| Condition | Outcome |
| --- | --- |
| `I == C` | convergence, no new revision |
| `C == B` | accept `I` (create, modify, delete, mode-only change) |
| `I == B` | convergence on `C` |
| `B` absent, `C ≠ I` | `ConcurrentCreate` |
| `B` present, exactly one side deleted | `DeleteModify` |
| both absent | already converged |
| both changed, all three text and ≤ 1 MiB | three-way merge |
| clean merge | new canonical revision with the merged bytes |
| conflicting merge | `TextConcurrentEdit`; **merge-marker output is discarded** |
| both changed, any side binary or oversize | `BinaryConcurrentEdit` |
| both changed mode incompatibly | `ModeConflict` |

Mode reconciliation runs alongside content: a mode change on one side only is
preserved; the same change on both sides converges.

The canonical working tree never receives automatically generated `<<<<<<<`,
`=======` or `>>>>>>>`.

## Persistence ordering

An accepted operation becomes durable **before** it is acknowledged:

1. write the new blob to temporary storage
2. flush it
3. atomically install it
4. begin the SQLite transaction
5. record the revision
6. update the canonical manifest
7. record `operation_id → result`
8. commit the transaction
9. acknowledge
10. broadcast

An orphaned blob after a crash is acceptable. A durable revision referencing a
non-durable blob is not. The host may crash between 8 and 10; clients recover
canonical state on reconnect and must never assume "everyone received a revision"
merely because the host accepted it.

## Control state

Tasks, conflicts, the latest Git publication and session configuration are **control
state**, not filesystem revisions, and consume no revision numbers. The host keeps a
`control_version` that increments whenever durable control state changes. Because
the data is small, Weave resynchronizes it wholesale: any change broadcasts a
complete snapshot, and a reconnecting client always receives one. A participant that
was disconnected during a Task completion, a conflict creation or a Git publication
therefore cannot miss it.

## Joining and reconnecting

Join:

1. authenticate
2. validate branch and base commit compatibility
3. capture a consistent snapshot `rS` (manifest and revision from the same point)
4. transfer the manifest
5. transfer the blobs the client lacks
6. materialize the canonical working tree
7. replay revisions after `rS`
8. refresh the control snapshot
9. enter live mode

Reconnect reports the latest **contiguous** applied revision, the control version,
the last installed publication and any pending operation IDs. The host replays the
missing revisions, or replaces the replay with a fresh snapshot when the gap is
large or the replica hash diverges at the same revision — both paths converge to the
same state.

## Replica state hash

Sorted by canonical path, hashing `path`, `git_mode` and `blob_hash`. Exposed in
`weave status` and reported periodically. A mismatch at the same canonical revision
is `ReplicaDivergence`: the client resynchronizes from a fresh snapshot rather than
continuing to publish changes based on a corrupted replica.

## Backpressure

Each connection's outbound queue is bounded by **both** message count (256) and
queued bytes (32 MiB). Exceeding either disconnects that participant. A slow
participant can neither block canonical revision processing nor grow host memory
without bound; it recovers through ordinary reconnection.

## Paths

Repository-relative, `/`-separated, valid UTF-8, never absolute, never containing
`..`, never targeting `.git`. Deserialization validates, so no unvalidated path can
enter the system from the wire.

Because sessions mix Windows, macOS and Linux, Weave enforces a portable subset:
no Windows reserved device names, no trailing spaces or dots, no characters Windows
forbids, no control characters. Two paths whose keys collide under the documented
normalization — **Unicode NFC followed by Unicode lowercase** — cannot coexist.

## Invites

An invite is an opaque URL-safe payload:

```
weave1_<base64url(json)>
```

carrying the protocol version, the WebSocket URL, the session ID, the session
secret, the base commit, the branch and the repository name. The internal encoding
is not a user-facing API. Because it contains the secret, `weave join` reads it from
a hidden prompt by default, with `--invite-file` and `--invite-stdin` for controlled
automation.
