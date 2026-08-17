# Weave architecture

This document explains how the implementation is organised and why. It assumes the
Weave V1 specification as the source of truth; section numbers below refer to it.

---

## 1. Processes and threads

`weave host`, `weave join` and `weave resume` start a long-lived daemon. Every other
`weave` command is a short-lived client of that daemon (§28).

Inside the daemon:

```
            ┌──────────────────── daemon process ────────────────────┐
            │                                                        │
  tokio ────┤  WebSocket server (host)   local IPC server            │
  runtime   │  WebSocket client (guest)  cloudflared child           │
            │            │        ▲            │                     │
            │      Noise transport  │      JSON requests             │
            │       (JSON inside)   │                                │
            │            ▼        │            ▼                     │
            │  ┌──────────────────┴──┐   ┌──────────────────┐        │
            │  │   HostEngine        │   │   ClientEngine   │        │
            │  │  (host only)        │◄─►│  (every machine) │        │
            │  │  own OS thread      │   │  own OS thread   │        │
            │  └─────────┬───────────┘   └────────┬─────────┘        │
            │            │                        │                  │
            │      host.sqlite + blobs      state.sqlite + blobs     │
            │                                     │                  │
            │                            filesystem watcher thread   │
            └────────────────────────────────────────────────────────┘
```

Both engines are **synchronous single-threaded state machines**. They own their
SQLite connection, their blob store and their invariants outright; there is no async
locking around canonical state and no possibility of two tasks interleaving inside a
reconciliation. Async code exists only where it belongs: sockets, child processes
and timers.

The host runs a `ClientEngine` too. Its edits reach the coordinator through an
in-process loopback pair carrying the **identical JSON frames** a remote participant
would send (§5), so the host's path is never a special case. That pair is a pair of
channels between two threads of one process — no socket, no network — so it is
deliberately not encrypted; the encryption boundary is the network, and everything
that crosses it goes through Noise.

## 2. Module map

| Module | Responsibility |
| --- | --- |
| `error` | Error classes and exit codes (§157) |
| `util` | Time, durable writes, secrets, restrictive permissions |
| `path` | Canonical paths, portable filename policy, link safety (§47–49) |
| `model` | `FileEntry`, `Revision`, `FileOperation`, `Task`, `Conflict`, publication types |
| `blobs` | Content-addressed SHA-256 store (§16) |
| `db` | SQLite connection setup (WAL + `synchronous = FULL`) |
| `store_host` | Canonical manifest, revision log, operation results, control state |
| `store_client` | Replica, persistent outbox, publication journal |
| `gitx` | Every call into the `git` executable |
| `scan` | Reading the working tree, safe materialization |
| `watch` | `notify` watcher plus debouncing (§31–33) |
| `reconcile` | The three-way reconciliation matrix (§71–81) |
| `proto` | Wire messages, envelopes, replica state hash |
| `secure` | Noise handshake, PSK derivation, encrypted framing (`snow`) |
| `transport` | Bounded outbound queues, backpressure, route and frame limits |
| `host` | The coordinator state machine |
| `client` (+ `client_ipc`) | The replica state machine and CLI command handling |
| `ipc` | Loopback control endpoint and its blocking client |
| `tunnel` | Cloudflare Quick Tunnel lifecycle |
| `daemon` | Wiring: preconditions, sockets, IPC, watcher, shutdown |
| `cli` | clap definitions, dispatch, human rendering |
| `doctor`, `recover`, `bootstrap` | Diagnostics, integrity, `AGENTS.md` |

## 3. On-disk layout

Everything Weave owns lives under `.git/weave`, so removing it leaves an ordinary
Git repository (§15, §198):

```
.git/weave/
├── host.sqlite      canonical manifest, revision log, tasks, conflicts, publications
├── state.sqlite     this machine's replica, outbox and publication journal
├── blobs/ab/abcd…   content-addressed by SHA-256 over exact bytes
├── conflicts/C-XXXX candidate content written for humans and agents to diff
├── logs/
├── tmp/             merge scratch and temporary Git indexes
├── runtime.json     pid, IPC port, IPC token  (restrictive permissions)
├── session.json     session identity and secret (restrictive permissions)
└── daemon.lock      OS-level exclusive lock, released on process death
```

## 4. The life of an edit

1. **Capture.** The watcher reports a path; a debounce window collapses editor
   write/rename storms. The engine reads the file and compares the resulting entry
   with `materialized` — the last state Weave itself wrote or confirmed. Equal means
   the event was the echo of Weave's own write and is ignored **by content, never by
   timer** (§44).
2. **Durability first.** Different means genuine local work: the bytes go into the
   blob store, `local_seq` increments, and the per-path record is persisted before
   anything is sent (§34, §35).
3. **One in flight per path.** If an operation is already awaiting a result, the new
   desired state coalesces into `pending_local` instead of starting a second stream
   (§38).
4. **Submit.** A `FileOperation` carries the complete file content, the declared
   base revision and the base entry (§22, §23).
5. **Validate.** The host re-derives `manifest(base_revision)[path]` from its own
   history and refuses the operation if it does not match the declared base — a
   client cannot choose its own merge base (§25). Actor identity comes from the
   connection, never from the payload (§26).
6. **Reconcile.** See §5 below.
7. **Persist, then acknowledge.** Blob written and flushed → SQLite transaction
   records the revision, the manifest update and the `operation_id → result` mapping
   → commit → acknowledge → broadcast (§68). An acknowledgement means the host may
   crash immediately and still remember the result (§69).
8. **Apply.** Every participant applies revisions strictly in order. The watermark
   means "everything up to here is applied", never "highest seen" (§105).
9. **Materialize.** Before writing canonical bytes to a path, the engine
   synchronously re-reads the file. If it changed, that change is captured durably
   *first* and materialization is skipped (§36). This is the mechanism behind the
   no-lost-edits invariant (§7.3).

## 5. Reconciliation

`reconcile(base, current, incoming)` is a pure function plus `git merge-file`:

| Situation | Result |
| --- | --- |
| `incoming == current` | converge, no revision consumed |
| `current == base` | accept incoming directly |
| `incoming == base` | converge on canonical |
| both created the path, differing | `ConcurrentCreate` |
| one deleted, one modified | `DeleteModify` |
| both modified, text, clean merge | accept merged bytes, new revision |
| both modified, text, overlapping | `TextConcurrentEdit`, merge output discarded |
| both modified, binary or oversize | `BinaryConcurrentEdit` |
| incompatible mode change | `ModeConflict` |

Mode and content are reconciled together: a one-sided executable-bit change survives
a concurrent content change on the other side.

The same function performs the **client-side continuation rebase** (§40–42), with
`base = the in-flight candidate`, `current = the canonical result`,
`incoming = the newer local candidate` the user produced while the operation was in
flight. That is what stops a user's newest edit from silently reverting work the
host just merged.

## 6. Conflicts

A conflict stores every candidate in the blob store — base, canonical, incoming and
the latest local candidate — so nothing rejected exists only in RAM (§83).

On the machine that produced the rejected candidate, the path enters **conflict
draft mode**: the watcher keeps capturing edits durably but stops auto-submitting
them, so it cannot race ahead of `weave conflict resolve` (§85). The working file is
restored to canonical content only once the local candidate is durable (§42, §43).

`weave conflict resolve` sends one atomic request carrying the expected canonical
entry. If canonical moved in the meantime the host answers `ResolutionOutdated` and
the conflict stays open (§87).

## 7. Git publication

Live state and published state are deliberately separate (§7.6):

```
Git publication:  C18 = r500
Live state:       r500 → … → r548   ← working tree
```

`weave commit prepare`:

1. The host sends `BarrierStart` to every connected participant.
2. Each participant rescans, freezes its `local_seq` as a watermark, persists what it
   found, and flushes everything at or below that watermark (§113). Work created
   after the watermark is withheld locally and queued by the host until the target
   revision is fixed (§114).
3. A participant reports ready only once all pre-barrier work is accepted, converged
   or turned into an explicit conflict (§115).
4. The host fixes `target_revision`, refuses if an active Task contributed revisions
   inside the target (§117) or any conflict is open, and persists an immutable
   preparation (§120).

`weave commit create <prepare_id>`:

1. The host reconstructs the **historical manifest** at the target revision from its
   durable revision log — never from the live working tree (§127).
2. `git hash-object -w --path <path> --stdin` produces each blob, applying the host
   repository's own path semantics (§126).
3. A temporary index plus `git update-index --index-info` and `git write-tree`
   produce the tree without touching the real index or the working tree.
4. `git commit-tree` creates the commit; the requesting participant is the author,
   the host is the committer, and contributors with a usable Git address appear as
   co-author trailers. Weave never invents an address (§130).
5. `git update-ref` moves the branch with compare-and-swap semantics (§133).
6. `git read-tree` moves the index to the published tree without touching working
   files, so live work after the target remains visible as uncommitted changes
   (§134).
7. `git pack-objects` produces the exact objects; every participant installs them
   with `git unpack-objects`, verifies the commit and tree OIDs before touching
   branch metadata (§132), and journals `objects_installed → ref_updated →
   index_updated → complete` so `weave resume` can finish an interrupted publication
   (§135, §195).

Only the host pushes (§138, §139). A diverged remote is reported, never
auto-reconciled (§140).

## 8. Crash safety

- SQLite runs WAL with `synchronous = FULL`; an acknowledged operation survives an
  immediate power loss.
- Blobs are written to a temporary sibling, flushed, then atomically installed
  before any revision may reference them. An orphaned blob after a crash is fine; a
  durable revision pointing at a missing blob is not (§68, §145).
- The participant outbox is durable, so a crash, a network failure or a stopped
  daemon all recover the same way: on restart the mandatory rescan finds whatever
  changed while Weave was not watching, and operation idempotency (`operation_id`)
  makes retransmission safe (§24, §147, §182).
- The daemon lock is an OS-level exclusive file lock, so it cannot outlive a dead
  process (§30).

## 9. Backpressure and limits

Each remote connection has a bounded outbound queue — 256 messages and 32 MiB — and
a participant that exceeds either bound is disconnected rather than allowed to grow
host memory without limit. It recovers through ordinary reconnection
synchronization (§65, §197). File content does not travel on that queue: it is
streamed on a separate data plane, chunk by chunk, paced by a fixed window of
in-flight frames (`docs/BLOB-PLANE.md`).

File size is therefore a resource budget rather than a protocol constraint. The
session carries its own limit in `ControlSnapshot`, default 128 MiB, readable with
`weave limit show` and changeable with `weave limit set`; a file above it is
preserved untouched on its own machine, reported to the whole session, and blocks
Git publication until it is resolved. Text above 1 MiB is treated as binary for
merge purposes (§51).

## 10. The encrypted channel

Every network connection between two Weave processes is a Noise session. The
handshake runs immediately after the WebSocket upgrade and before a single byte of
application state; only when it completes does the host register the connection with
its engine. `src/secure.rs` is the whole of it, and it is deliberately small: it
configures `snow` with the pattern, the derived PSK and the prologue, and adds
chunking on top of the transport state. The state machine, key schedule, AEAD and
nonces belong to the library. [docs/PROTOCOL.md](PROTOCOL.md) has the parameters and
[docs/SECURITY.md](SECURITY.md) has the properties.

Two consequences shape the surrounding code. First, one `TransportState` is shared
by the reader and writer tasks of a connection, behind a mutex, because Noise nonces
are per-direction counters that must advance in order — a poisoned mutex is treated
as fatal for that connection rather than something to continue past. Second, a
connection is the unit of key material: a reconnect, a supervised restart or a
`weave tunnel restart` performs a complete fresh handshake with new ephemeral keys,
and nothing — nonce state, partially reassembled messages — carries over. That costs
nothing, because the durable outbox and idempotent `operation_id`s already made
reconnection free.

## 11. TLS

Remote participants connect over `wss://`, so the process needs a `rustls`
crypto provider. `rustls` 0.23 refuses to choose one implicitly, and a missing
provider does not fail the connection — it *panics inside the connection task*,
which would leave a participant permanently offline with no path back. Weave
therefore pins `ring` (no external build toolchain on any supported platform) and
installs it explicitly, once, before the first connect. The connection task is
also supervised: if it ever ends abnormally it is restarted, because a durable
outbox plus idempotent operations make a restart free.

TLS stays even though the payload is already encrypted. The two protect different
things — TLS authenticates the tunnel endpoint and hides the Noise session from the
network path to Cloudflare, Noise protects the payload from Cloudflare itself — and
dropping one because the other exists would be a downgrade.

Both behaviours are covered by `tests/remote_tunnel.rs`, which is the only test
that leaves the machine.

## 12. Deliberate non-goals

No P2P, no CRDT, no MCP server, no agent orchestration, no host migration or leader
election, no automatic reconciliation with external Git history, no first-class
distributed rename (a rename is a delete plus a create, and Git may detect it
heuristically later).
