# Weave security model

Read this before using Weave with anything you would not hand to everyone in the
session.

## What the session secret grants

A Weave session has one 256-bit secret, generated from the operating system's
cryptographically secure RNG. **Possession of that secret is full collaborative
authority**: read and write access to every synchronized file in the repository for
the lifetime of the session, plus the ability to create Tasks, resolve conflicts and
request Git publications and pushes.

Explicitly, V1 has:

- no user accounts;
- no fine-grained authorization;
- no per-file permissions;
- no revocation of an individual participant short of ending the session.

The invite carries the secret. Treat an invite exactly as you would treat a
credential: send it over a channel you trust, and do not paste it into a shared
document, an issue tracker or a chat log that outlives the session.

Because the invite is a credential, `weave join` reads it from a hidden prompt by
default. `--invite-file` and `--invite-stdin` exist for controlled automation so the
secret never has to appear in a shell history or a process listing.

Anyone holding the invite can join at any point for as long as the session runs.
Sharing an invite is not revocable: ending the session is the only way to withdraw
access, and a new session means a new secret.

## Transport confidentiality

Weave encrypts the application protocol end to end, between the host process and
each participant process, **independently of whatever carries the bytes**. The
construction is a standard [Noise](https://noiseprotocol.org/noise.html) handshake —
`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`, run by the `snow` crate — keyed by a
pre-shared key derived from the session secret with HKDF-SHA256 and explicit domain
separation. [docs/PROTOCOL.md](PROTOCOL.md) states the derivation, the prologue and
the framing exactly. Weave implements no cryptographic primitives and no handshake
state machine of its own.

The raw session secret is never sent over the network, in any mode, at any point.

### Audit status of the cryptographic dependencies

**`snow` 0.10.0 — the Noise implementation Weave depends on — has not received a
formal, published third-party security audit.** This is stated plainly because it
is the single largest unreviewed component in Weave's security story, and no
amount of "standard protocol, standard crate" changes that.

What is true alongside it: `snow` is a widely deployed implementation of a
publicly specified protocol (Noise revision 34), and it does not implement the
primitives itself. Those come from crates with their own review histories —
`curve25519-dalek` 4.1.3, `chacha20poly1305` 0.10.1 and `blake2` 0.10.6 from the
RustCrypto project. The key derivation uses `hkdf` 0.12.4 over `sha2` 0.10.9.
Weave adds no primitives and no handshake state machine of its own, so the
unaudited surface is `snow`'s state machine and framing, not the mathematics
underneath it.

The custom part Weave *does* own — chunking application messages across Noise
messages, and the reassembly bounds on the receiving side — is described in
[docs/PROTOCOL.md](PROTOCOL.md) and is deliberately small: a one-byte
continuation flag carried inside the ciphertext, and two hard limits on
reassembly (total bytes and chunk count). It has not been externally audited
either.

### What this means against Cloudflare

Remote sessions use a **Cloudflare Quick Tunnel**: HTTPS and WebSocket traffic
terminates at Cloudflare and is forwarded to a loopback listener on the host.
Cloudflare therefore terminates TLS and sees the WebSocket frames — but the frames
carry Noise ciphertext.

Cloudflare (or any other intermediary on the path) can observe:

- the IP addresses of the host and of each participant;
- the `trycloudflare.com` hostname and the `/weave/v2` request path;
- when connections open and close, and how long they last;
- how many frames go each way, when, and how large each one is;
- that the traffic is a Noise session, from the handshake's shape;
- that the origin is Weave — `GET /` answers with the literal string `weave`. That
  is the only unauthenticated response the host gives and the only plaintext Weave
  writes to a network socket; it carries no session information, not the session
  id, not the branch, not a participant count.

Cloudflare cannot read: file paths, file contents, blobs, manifests, snapshots,
revision contents, Task names, descriptions or state, conflict data and candidates,
resolutions, participant messages, Git publication metadata, commit messages,
queued outbox operations, control-state snapshots, or the content hashes and Git
OIDs that identify any of it. All of that lives inside the encrypted channel.
`tests/encrypted_transport.rs` reads the actual socket bytes through an
in-test proxy and asserts that repository sentinels never appear in them; a
companion test proves the scan catches that plaintext when encryption is bypassed.

Weave V1 does **not** attempt to hide traffic metadata. There is no padding, no
cover traffic and no traffic shaping. An observer who knows the session exists can
tell when people are working and roughly how much data moves.

Quick Tunnels are Cloudflare's development and testing feature: they hand out
temporary `trycloudflare.com` hostnames without an account, carry no SLA, and are
subject to Cloudflare's in-flight request limits. A normal two-to-five person
session needs only a handful of persistent WebSocket connections, which fits
comfortably, but Weave V1 is not a production hosting story.

### TLS is still there

Removing TLS because the payload is encrypted would be a downgrade, not a
simplification, so remote sessions keep `wss://` exactly as before. The client side
uses `rustls` with the `ring` provider, installed explicitly at startup and
verifying server certificates against the webpki root store. There is no option to
skip certificate verification, and `tests/remote_tunnel.rs` asserts that a
participant reaches the host only through a `wss://…trycloudflare.com` endpoint on
the `/weave/v2` route — a regression that quietly downgraded to loopback or plain
`ws://` would fail there.

### Authentication

The public hostname is **not** authentication. Anyone who finds the URL still needs
the session secret, because the handshake is the authentication: without the right
derived key a peer cannot produce a message that authenticates, the handshake
fails, and no repository state — not the manifest, not a path, not the participant
list — is ever sent. A peer may complete the WebSocket upgrade before proving
anything, which tells it only that something is listening. That window is bounded by
a 15-second handshake timeout, a 1 KiB cap on handshake messages, and a cap of 32
concurrent pending handshakes.

Those bounds matter because a recorded first handshake message can be replayed: the
host cannot distinguish a replay from a new initiator until the exchange fails, and
this is a property of the pattern rather than something Weave papers over. A replay
learns nothing — the transport keys need the initiator's ephemeral private key — and
occupies one of the 32 slots for at most 15 seconds.

There is no fallback to the older unencrypted protocol. A version 1 peer, or any
peer that cannot complete the handshake, gets a clear error.

### LAN and local mode

`weave host --lan` skips Cloudflare entirely and serves the local network over plain
`ws://`. The WebSocket is unencrypted; **the Weave payload inside it is not**. LAN
participants run the identical Noise handshake with the identical parameters — there
is deliberately no separate, weaker LAN path — so someone on the same network sees
connection metadata and ciphertext, not repository content.

`weave host --local` exposes no remote endpoint at all.

### What end-to-end encryption does not protect

The property Weave provides is precise: an intermediary that carries the traffic
cannot read the session. It is worth being equally precise about what that leaves.

- **The endpoints see everything, by design.** The host and every participant hold
  the same session secret and therefore the same plaintext. Weave is a
  collaboration tool; encryption protects the channel, not the collaborators from
  each other.
- **A compromised participant is a compromised session.** Whoever controls a joined
  machine can read every synchronized file and change any of them, and Weave cannot
  distinguish that from legitimate work. There is no per-participant revocation and
  no fine-grained permission in V1.
- **Files are plaintext at rest.** Every replica materializes the repository into an
  ordinary working tree on disk. Weave adds no encryption there; local disk
  encryption and file permissions are the operating system's job.
- **Pushed Git content leaves the session.** When the host publishes and pushes,
  the commits go to the configured Git remote under whatever confidentiality that
  remote provides. The Noise session covers the collaboration traffic, not the
  destination of a `git push`.
- **Metadata is visible**, as described above.

## Local secrets

Two secrets live on disk, both under `.git/weave`, both written with restrictive
permissions (mode `0600` on Unix, an inheritance-stripped user-only ACL on Windows):

| File | Contents |
| --- | --- |
| `runtime.json` | daemon pid, loopback IPC port, local IPC token |
| `session.json` | session identity and the session secret |

The local control endpoint binds **loopback only** and requires the bearer token
from `runtime.json`. That IPC never leaves the machine and is deliberately not
wrapped in Noise; loopback plus the token plus the file permissions is the boundary
there.

The derived Noise pre-shared key and the per-connection transport keys exist only in
memory. The PSK is held in a `Zeroizing` buffer, the session secret is a
`SessionSecret` newtype over `Zeroizing<String>` so that dropping it wipes the
bytes instead of merely freeing them, and transport keys live inside the `snow`
state for the lifetime of one connection.

The honest limit of that: zeroization only covers the copies Weave controls.
`serde_json` builds an ordinary `String` while parsing an invite or
`session.json`, the operating system may have paged any copy to disk before it
was wiped, and the invite itself sits in a file, a clipboard, or terminal
scrollback. Zeroizing narrows the window in which a secret is recoverable from
process memory; it does not close it, and it is not a substitute for ending the
session when it is over.

None of it is ever logged. Not the session secret, not the derived PSK, not the
transport keys, not the IPC token, not the full invite, and not decrypted payloads.
Cryptographic errors are reported as the fact that a handshake or a frame was
rejected, without quoting the underlying error, the buffer or any key material, and
the type holding a live channel implements neither `Debug` nor `Clone` so it cannot
be formatted into a log line or a panic message by accident. Verbose logging is
limited to revision IDs, operation IDs, paths, hashes, connection lifecycle events
and Git OIDs.

## Trust boundaries

- **Participants are trusted.** Weave validates protocol integrity — declared merge
  bases, content hashes, path policy, operation identity, message size — but a
  participant holding the secret is by design able to change any synchronized file.
- **Actor identity is bound to the connection.** The host never trusts an
  `actor_id` taken from a message payload. It does not, however, distinguish two
  holders of the same secret; identity here is attribution, not authorization.
- **Content is verified.** Blobs are content-addressed with SHA-256 and verified on
  read; a blob whose bytes do not hash to their name is an integrity error, not
  something Weave silently accepts. The host refuses to substitute client bytes for
  a missing canonical blob and stops accepting mutations instead.
- **Paths are validated on deserialization**, so no message can introduce an
  absolute path, a `..` traversal, a `.git` target, or a name that is unsafe on one
  of the supported platforms.
- **Links are never traversed.** Weave refuses to follow symlinks, Windows
  junctions and other reparse points inside the repository, so no synchronized path
  can escape the repository root.

## Git integrity

During a session Weave owns every Git-writing operation. It monitors the current
branch, the branch ref, HEAD and the Git index; on unexpected external change it
**pauses** and reports the expected and current state. It never attempts an
automatic pull, merge, rebase or reset in response.

The check runs every few seconds and pauses only on a problem it still sees on
the following check. Publishing moves HEAD and then the index in two separate Git
commands, and the host and participant engines run on separate threads against
one repository, so a single observation can catch Weave mid-write and read it as
an external change. A change a user really made is still there a few seconds
later; a publication window is not. The cost is that a genuine external change is
detected one check later than it happens.

Only the host constructs Git objects and only the host pushes. Participants install
the exact objects the host produced and verify the commit and tree OIDs before
touching branch metadata; branch updates use compare-and-swap semantics, and a
mismatch pauses that replica with a `GitIntegrityError` rather than rewriting
history.

If Weave is removed entirely, the repository remains an ordinary, fully usable Git
repository. You lose live revision metadata, Tasks, conflict history and
coordination — never Git history or repository usability.

## Reporting a vulnerability

Open a security advisory on the repository rather than a public issue.
