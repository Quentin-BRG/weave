# Weave wire protocol

`protocol_version` is **2**. A peer that announces a different version is rejected.
Version 2 encrypts every application message; there is no compatibility mode with
the unencrypted version 1 protocol and no downgrade path. The break is visible in
three places, so a mismatch in either direction produces a specific error rather
than a silent fallback: the version number itself, the WebSocket route
(`/weave/v2`, previously `/weave`) and the invite prefix (`weave2_`, previously
`weave1_`).

## Transport

One long-lived WebSocket per participant, all logical messages multiplexed over it.
Remote sessions run through a Cloudflare Quick Tunnel, which supports WebSocket
upgrades; LAN sessions use a plain socket. The host coordinator binds only to
loopback in tunnel mode.

Every WebSocket frame Weave sends is **binary** and every payload is a Noise
transport message. A text frame received on a Weave connection terminates it.

Serialization inside the encrypted channel is JSON. File content travels as
**complete bytes**, base64-encoded — Weave deliberately does not use textual deltas
as the primary operation representation. That removes patch-application ambiguity,
simplifies recovery and makes three-way reconciliation straightforward, at the cost
of bandwidth Weave's target workload can afford.

Maximum application message: 48 MiB (comfortably above a 10 MiB file after base64
expansion plus overhead). Oversized messages are protocol errors.

## Session establishment

Authentication and confidentiality are the same step: a **Noise** handshake, run by
the [`snow`](https://crates.io/crates/snow) implementation of the
[Noise Protocol Framework](https://noiseprotocol.org/noise.html) (revision 34).
Weave does not implement the Noise state machine, key schedule, AEAD or nonce
handling itself.

```
Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s
```

- `NN` — no static keys, no certificates, no PKI. Weave V1 has no public-key user
  identities, and this pattern does not invent any.
- `psk0` — the pre-shared key is mixed in at the very start of the first message, so
  every handshake message including the first is bound to it.
- The ephemeral-ephemeral Diffie-Hellman gives each connection independent
  forward-secret transport keys.

The pre-shared key is **not** the session secret. It is derived with HKDF-SHA256
under explicit domain separation and used for nothing else:

```
salt = "weave-noise-psk-v1"
ikm  = session secret
info = "weave noise pre-shared key" || u16be(transport version) || session UUID
psk  = HKDF-SHA256(salt, ikm, info)[0..32]
```

The Noise prologue — authenticated by the handshake hash, so any disagreement fails
the handshake — is `"weave-noise-v1" || pattern name || u16be(transport version) ||
session UUID`.

The raw session secret is never transmitted. The handshake is two messages:

```
-> psk, e
<- e, ee
```

The initiator's message carries an ephemeral public key; the responder replies and
both sides move to transport mode. A peer that does not hold the same secret cannot
produce a message whose authentication tag verifies, so the handshake fails and no
application data is ever exchanged. The public Cloudflare hostname is not
authentication.

An unauthenticated peer may complete the WebSocket upgrade — it learns only that
something answers — but is given no session information, participant list, manifest
or repository state before the handshake succeeds. The unauthenticated window is
bounded by a 15 s handshake timeout covering the whole exchange, a 1 KiB cap on
handshake messages, and at most 32 concurrent pending handshakes.

Someone who records a genuine first handshake message can replay it and get a reply,
as in any `NN`-family pattern: the responder has no way to tell a replay from a new
initiator until the exchange fails. It gains nothing — deriving the transport keys
needs the initiator's ephemeral private key — and costs one handshake slot for at
most the timeout. That is the reason the two bounds above exist.

## Framing

A Noise transport message is at most 65535 bytes, so an application message larger
than that is split. Each WebSocket frame carries one Noise message whose plaintext
is:

```
[1-byte continuation flag][chunk]   flag: 1 = more chunks follow, 0 = last chunk
```

The flag is inside the ciphertext, and Noise's per-direction nonce counter is
implicit rather than transmitted. A dropped, reordered, duplicated, truncated or
forged frame therefore fails authentication instead of silently truncating or
reassembling a message.

Reassembly is bounded on two axes, checked before any chunk is appended. **Bytes:**
the running total may never exceed the protocol message limit, and the buffer is
reserved exactly so its capacity is bounded by that limit too. **Chunks:** a message
may span at most as many chunks as a maximum-size message needs, plus one. The chunk
bound exists because the byte bound alone does not stop a peer from streaming
empty continuation chunks forever — each authentic, none of them growing the buffer,
the message never completing. Exceeding either bound discards the partial message and
fails the connection; a peer holding the session secret is still only a peer.

Every reconnection — including one caused by a tunnel restart — performs a complete
fresh handshake with new ephemeral keys and fresh transport keys. Nonce counters and
partially reassembled messages are never carried across connections.

## Envelopes

Every application message is an object carrying `protocol_version` and
`message_type`, serialized and then encrypted:

```json
{ "protocol_version": 2, "message_type": "submit_operation", "operation": { … } }
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

1. complete the Noise handshake
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
weave2_<base64url(json)>
```

carrying the protocol version, the WebSocket URL, the session ID, the session
secret, the base commit, the branch and the repository name. The internal encoding
is not a user-facing API. Because it contains the secret, `weave join` reads it from
a hidden prompt by default, with `--invite-file` and `--invite-stdin` for controlled
automation.

The secret in the invite never reaches the network: it is the input to the PSK
derivation above, and only the derived key is used, and only inside the handshake.
A `weave1_` invite is refused by name — Weave 1 sessions were not encrypted, and
joining one is not something this version will silently do.
