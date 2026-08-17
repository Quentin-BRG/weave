# The blob plane

How Weave carries file content, and why file size stopped being a protocol
concern. This document is the design record for the work that lands before
`v0.1.0`; [ARCHITECTURE.md](ARCHITECTURE.md) and [PROTOCOL.md](PROTOCOL.md) are
updated as each phase lands and remain the reference for the shipped system.

---

## 1. The problem

Weave synchronizes complete file content, base64-encoded inside the JSON control
messages. That single decision produced every limit around it:

- a 10 MiB ceiling on any synchronized file (`MAX_SYNCED_FILE`),
- a 48 MiB application message limit sized to hold one such file,
- a 32 MiB per-connection outbound queue that **disconnects** a peer rather than
  slowing it down,
- host memory proportional to `file size x participants`, because each outbound
  queue holds its own base64 copy of a broadcast,
- whole files in RAM at every step: read, hash, base64 (peak ~2.3x), JSON.

The product consequence was worse than the engineering one. A repository holding
an ordinary 20 MiB PDF could not be represented, and the scanner's per-path
rejection silently excluded the file from the manifest — which meant the first
publication **deleted it from the Git tree**, because the published tree is built
from the Weave manifest alone.

## 2. The invariant

> Either the whole repository takes part in the session, or Weave refuses to
> start. There is no partially synchronized path.

Every participant always sees the same logical repository state, for creation,
modification and deletion alike, whatever the file size. No "frozen" or
"out of scope" third state exists: it would trade a permanent divergence between
replicas for a convenience, and divergence is the one thing a collaboration tool
may not offer.

## 3. Shape of the solution

The codebase is already content-addressed end to end: the manifest carries only
metadata (`blob_hash`, `size`, `git_mode`, `file_kind`), the blob store is keyed
by SHA-256 with atomic installation, and a `RequestBlobs` / `Blobs` pull already
exists alongside the `materialization_blocked` flag that suspends materialization
until content arrives.

Nothing conceptual is missing. What is wrong is that content travels **in the
control path** instead of being transferred beside it and referenced by hash. So
the work is largely subtractive: remove `content_b64`, and make the existing pull
path the only path — which also promotes rarely exercised recovery branches into
the nominal regime, where they get tested continuously.

### Two frame classes, one Noise session

Every application message carries its class — `0` control JSON, `1` blob chunk —
in the bit above the continuation flag that Noise framing already spent a
plaintext byte on, so the split costs nothing on the wire. The receiver refuses
a class change mid-message rather than assuming the sender behaves.

Content stays inside the Noise session. An out-of-band HTTP endpoint with range
requests would have given resumption for free, and was rejected: it would expose
file content to the tunnel operator, contradicting the property that Noise
protects the payload from Cloudflare itself. A second dedicated connection was
also rejected — a second handshake, a second route and a doubled reconnection
story buy less than prioritized interleaving costs.

### Control priority, and why a wire chunk is one Noise message

One `TransportState` is shared by the reader and writer of a connection behind a
mutex, because Noise nonces are per-direction counters that must advance in
order. A bulk transfer that holds that mutex starves acknowledgements and
heartbeats, and **the session looks dead** — the most damaging failure mode this
design can produce.

So each chunk is a separate application message, and the writer serves the
control queue at strict priority ahead of the data queue. The wire chunk is
sized so that a data message is *exactly one Noise message* — a shade under
64 KiB after the framing byte and the transfer header. That is the smallest
control latency the shared transport state permits: a control message waits for
the single frame already being written, and never for a multi-frame payload. It
is a latency bound, not a throughput parameter; the 0.06% header overhead it
costs is not worth trading away.

Disk streaming keeps its own, larger 256 KiB chunk. Reading the blob store and
writing the socket are different bottlenecks and have no reason to share a
constant.

### Backpressure that slows down instead of disconnecting

| | before | after |
| --- | --- | --- |
| control queue | 256 messages / 32 MiB, exceeding **disconnects** | unchanged, but content can no longer inflate it |
| data queue | — | 8 frames (&lt;512 KiB); a full queue **makes the sender wait** |

The control queue must still disconnect, and that is not a leftover: it is fed
by a synchronous state machine that has nowhere to wait. What changes is that
after phase 2 no file content passes through it, so the bound stops being
reachable by ordinary use.

The data queue needs no credit protocol. Its producer is an async transfer pump
reading the blob store, so a full queue simply parks it, and a peer that stops
reading its socket propagates the stall back through TCP to that same pump. The
real queue is the file already on disk, which costs nothing to leave there. A
slow participant therefore transfers slowly instead of losing its session — a
failure mode removed rather than added.

### Upload before submit

A client stores its blob locally (durability first, unchanged), asks the host
which hashes it is missing, uploads those, and only then sends `SubmitOperation`
carrying `desired_entry` and no content. The host checks `blobs.has(hash)` and
answers a protocol error if it is absent.

The alternative — the host parking an operation until its blob arrives —
introduces an in-memory pending state, a timeout, and a bound to defend against a
client that announces and never sends. Upload-before-submit keeps the host state
machine processing only small, immediately actionable messages, and preserves
"an acknowledged operation is durable" exactly as it stands today.

### Broadcast then pull

`RevisionBroadcast` carries the revision only. Each participant notices the
missing blob and requests it, through the path that already exists. Host memory
per peer becomes O(1) instead of O(size x N).

### Streaming everywhere

No whole file is ever held in memory:

- `BlobStore::put_streaming` writes `tmp/<hash>.part`, hashes incrementally,
  fsyncs, and renames into place **only** if the content hashes to the announced
  value. Peak memory is one chunk, and a corrupt or hostile partial can never be
  installed.
- The scanner hashes into the blob store as it reads and returns entries only.
  This also removes `ScanResult.contents`, which loads the entire repository into
  RAM at session start.
- Materialization streams from the blob store to a temporary file, then renames.
- `gitx::hash_object` takes a file path: `git hash-object -w --path <repo path>
  <blob file>`, no stdin buffering.
- The publication pack travels on the data plane to a temporary file, then
  `git unpack-objects`. A pack containing a large blob has exactly the same
  problem as the blob, and it is on the critical path of `weave commit create`.

### Resumption

Blobs are immutable and content-addressed, so resumption is a byte offset. The
**receiver** decides it: a partial lives at `<blobs>/.partial/<hash>`, named after
the content it will hold, and accepting an offer re-hashes what is already there
and answers with that length as `from_offset`. Nothing about a transfer needs to
survive a crash except the bytes themselves — there is no journal to corrupt, and
an interrupted daemon resumes from whatever reached the disk.

Three properties make that safe:

- **The prefix is re-hashed, never trusted.** A torn write, a truncation or a
  deliberately damaged partial produces a hash mismatch at the end of the
  transfer, which destroys the partial and re-fetches from zero. A partial that
  is not a prefix of the announced content can delay a transfer; it can never
  install the wrong bytes.
- **A partial is opened for append**, so a stray write can never land before what
  has already been hashed, and a partial longer than the offer restarts at zero.
- **One writer per partial**, enforced by a process-wide claim set keyed on the
  absolute path. Several `BlobStore` handles are open in one daemon (the
  coordinator, the local replica, the IPC surface); a second writer for the same
  hash falls back to an anonymous temporary file, which is slower and always
  correct.

### Collection

Content becomes unreachable in ordinary use: a superseded file, a resolved
conflict, an abandoned transfer. `BlobStore::collect_garbage` takes a live set
and an age guard, and removes only what is in neither. It is conservative by
construction — the live set is derived from durable state (`ClientStore` and
`HostStore`), not from memory, and anything younger than the guard is kept
whatever the live set says, so a blob that has been ingested but not yet
committed to a store cannot be collected out from under its own operation.

It runs on the **replica** engine only, once every 15 minutes, because a host
machine has one blob store shared by its coordinator and its replica: collecting
from either alone would delete the other's content. The replica reads the
coordinator's live set directly from `host.sqlite`, so the union is swept once.

Publication packs are deliberately absent from the live set. They are derived
state; `send_publication` rebuilds the same pack, with the same hash, on every
announcement including to a reconnecting participant, so a collected pack
self-heals.

### One new barrier precondition

A participant may not report ready while it cannot reproduce the state it has
been told about — a blob still in transit, a publication pack that has not
arrived, or a conflict whose canonical side has still to be restored. Otherwise
`git read-tree` moves the index onto the published tree while the working file is
still absent, and `git status` shows a phantom modification.

The participant stays silent rather than answering `BarrierReady { ok: false }`,
which is how outstanding local work is already handled: the host's 20 second
barrier timeout is the backstop, and a transfer that finishes inside it lets the
publication succeed with everybody included.

## 4. The canonical session limit

Size is no longer a protocol constraint. What remains is a **resource budget**,
dominated by the star topology: the host uploads every change to every
participant over one uplink, and Weave transfers whole content by design, so the
cost of one modification is `size x participants` on a single machine.

```
S_max ~= (acceptable propagation time x host uplink) / participants
```

At a 60 second budget that lands between ~60 MB on VDSL and ~240 MB on symmetric
fibre. The default is **128 MiB**, configurable far higher for LAN sessions.

The limit is **canonical session state**, not a local preference. It lives in
`ControlSnapshot`, which already carries a monotonic `control_version`, is
broadcast to every participant, is cached durably by clients, and is re-delivered
on reconnect through `Welcome`. Raising it is a session decision that every
participant observes at the same control version.

### At startup

`weave host` refuses to start when the initial scan finds a file above the limit,
naming the file and the two remedies: raise the session limit, or remove the file
from the session. A session whose initial state cannot be fully represented is
never started.

`weave join` cannot know the canonical limit before connecting, so its preflight
checks against the default as a hint and the authoritative check runs on
`Welcome`. Failing it ends the join with a clear error instead of starting a
degraded session.

### A file created above the limit during a session

The working file is never touched, and the path never enters a partially
synchronized state.

| | |
| --- | --- |
| local bytes | preserved exactly; Weave neither reads, rewrites nor deletes the file |
| capture | none — no blob, no operation, no revision |
| durable state | `oversize { path, size }` in the client store, reported to the host |
| visibility | `weave status` (human and `--json`), a persistent notice, and the host's `ControlSnapshot` so **every** participant sees which file blocks publication and whose it is |
| the rest of the session | unaffected; every other path synchronizes normally |
| `weave commit prepare` | refused while any participant reports an oversize path, listing path, size, owner and remedy |

Two ways out, both ordinary:

1. **Delete or shrink the file.** Below the limit it becomes an ordinary create
   and enters the normal pipeline. Nothing special is needed: it was never
   canonical, so no revision has to be undone.
2. **Raise the session limit.** On the new control version every client
   re-evaluates its oversize set, captures the file, and submits it through the
   normal blob pipeline. It becomes a canonical revision like any other.

Lowering the limit is refused while any manifest entry exceeds the new value.

### The one residual divergence, stated plainly

A path that is *already canonical* and then grows above the limit is the only
case where replicas differ: canonical holds the old content, the author's disk
holds the new. Weave will not capture it (over the limit) and must not
materialize over it (priority 1, no lost edits).

That window is bounded and explicit: it is reported everywhere, and it blocks
publication, so a divergent state can never be committed or pushed. The
alternative — capturing the oversize candidate and restoring canonical content on
disk, as conflict draft mode does — keeps replicas identical and preserves the
bytes in the blob store, at the cost of reverting a working file the user may be
actively editing with an external tool. The conservative reading of "the local
file is always preserved" won; this is the decision most worth revisiting with
usage.

## 5. What does not change

The reconciliation matrix, the revision log, the manifest, operation idempotency,
conflict semantics, the barrier and publication protocol, and the Noise model are
untouched. They operate on `FileEntry` values and a blob store, never on bytes in
flight. The work is confined to the transport, the blob store, the I/O paths, and
the places where `content_b64` appears.

## 6. Phases

| Phase | Contents | Risk | |
| --- | --- | --- | --- |
| **0 — Local streaming** | streaming blob store, scanner hashing into it, streaming materialization, `gitx` from files | low | done |
| **1 — Transport** | frame class, priority writer, waiting data queue | medium | done |
| **2 — Protocol v3** | `content_b64` removed everywhere, pull-based broadcast, upload-before-submit, conflict blobs, publication pack | **high** | done |
| **3 — Robustness** | offset resumption, barrier precondition, blob GC, fast rescan, stability detection | medium | done |
| **4 — Policy** | canonical session limit and its state machine, startup refusal, disk checks, docs | low | |

Phase 0 stands alone and ships value without a protocol change: it removes the
memory ceiling before the wire changes at all. Phase 1 likewise changes no
protocol message: it builds the plane and leaves it unused, and a peer that
sends on it early is dropped rather than silently ignored.

Two supporting pieces belong to this work rather than to a later one, because
they are what make large files bearable in practice. Both landed with phase 3:

- **Fast rescan.** The mandatory full rescan on every reconnect is O(total
  repository bytes) and rehashes everything. Size and mtime become a cache in
  front of the hash, as Git's index does — the hash stays the only truth, the
  metadata only decides whether to recompute it. `scan::ScanCache` records the
  metadata *observed before the read*, so a write that races the read advances
  the mtime past what was recorded and forces a rehash; it refuses to answer for
  an entry whose blob is no longer in the store; it recomputes the file mode on
  every hit, because `chmod` need not move the mtime; and it declines to cache an
  entry written within Git's two-second "racily clean" window at all. The cache
  is in memory only — it is an optimization, and rebuilding it costs exactly the
  full scan that would otherwise have happened anyway.
- **Stability detection.** A tool writing a large file for tens of seconds
  outlives the debounce window, so Weave would hash a partial state, transfer it,
  and transfer again once it settles. A file of 8 MiB or more is captured only
  once it has held still: primarily because nothing has written to it for a whole
  window (`now - mtime >= STABILITY_WINDOW_MS`), and failing that — coarse or
  skewed timestamps, a clock behind the filesystem — because two observations a
  window apart agree on size and mtime. Anything smaller is captured
  immediately, since re-transferring it is cheaper than delaying it.

  Waiting has two consequences that matter more than the wait itself. The
  canonical state the local change is based on is frozen at the *first* sighting
  of the file changing, not at the moment it settles: a revision that arrives
  while the file is still being written is competing with that edit, and
  recording it as the base would silently turn a concurrent edit into a
  sequential one and lose the conflict. And canonical content is never written
  over a file that has not settled — materialization first tries to capture, and
  if capture declines because the file is still moving, the write is deferred to
  a later tick rather than overwriting bytes Weave has not recorded. Every
  unsettled path is re-examined each tick, and the working tree is
  re-synchronized as soon as one of them settles.

## 7. Test matrix

End-to-end, driving the real binary, as everything in `tests/` does. The
blob-plane cases live in `tests/blob_plane.rs`; the framing and installation
invariants they rest on are unit-tested in `src/blobwire.rs` and `src/blobs.rs`,
where a corrupt or truncated transfer can actually be constructed — on the wire,
Noise authentication fails long before a hash check would. The one exception is a
damaged *partial*, which is local state rather than wire state and so can be
corrupted end-to-end, between a crash and the restart that resumes from it.

| Test | What it defends | |
| --- | --- | --- |
| large file created, modified and deleted, 3 participants | the invariant itself: identical logical state throughout | phase 2 |
| a file past both old ceilings, in both directions | that neither the 10 MiB message limit nor the 32 MiB queue bound survives anywhere | phase 2 |
| many concurrent transfers, three senders | transfer ids stay isolated; no blob receives another's bytes | phase 2 |
| daemon crash mid-transfer | a `.part` never installs; the working tree never shows a partial file; the transfer completes after restart | phase 2 |
| concurrent binary edit on a large file | conflict with both candidates preserved whole, and resolution uploading a large candidate back | phase 2 |
| publication including a large file | pack over the data plane, and the published tree equals the parent tree modified only where revisions exist | phase 2 |
| small edits during a large transfer | **control-plane starvation** — the regression that turns a healthy session into an apparently frozen one | phase 2 |
| crash mid-transfer, then reconnect | resumption from a **non-zero** offset, proved against the bytes held at the moment of the crash, and nothing left behind afterwards | phase 3 |
| a damaged partial recovered after a crash | a partial that is no longer a prefix is destroyed rather than installed, and the content is fetched again | phase 3 |
| publication prepared while a participant is still receiving | the barrier waits: `prepare` cannot return before that participant can reproduce the state, and it is not written off as disconnected | phase 3 |
| collection with no age guard at all | live content survives on both replicas, the host's whole revision history still resolves, and unreachable content is actually removed | phase 3 |
| file created above the session limit | preserved locally, publication blocked, both remedies, and the raise propagating as a control version | phase 4 |
