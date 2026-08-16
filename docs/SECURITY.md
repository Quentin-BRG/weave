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

## Transport confidentiality

Remote sessions use a **Cloudflare Quick Tunnel**: HTTPS and WebSocket traffic
terminates at Cloudflare and is forwarded to a loopback listener on the host.

Weave V1 does **not** add application-level end-to-end encryption independent of
Cloudflare. Confidentiality against Cloudflare itself is therefore not a property
this design provides, and this document states that rather than implying otherwise.

Quick Tunnels are Cloudflare's development and testing feature: they hand out
temporary `trycloudflare.com` hostnames without an account, carry no SLA, and are
subject to Cloudflare's in-flight request limits. A normal two-to-five person
session needs only a handful of persistent WebSocket connections, which fits
comfortably, but Weave V1 is not a production hosting story.

The public hostname is **not** authentication. Anyone who finds the URL still needs
the session secret; an unauthenticated client receives no repository content — the
WebSocket upgrade is refused with `401` before any state is exchanged. The secret is
compared in constant time.

`weave host --lan` skips Cloudflare entirely and serves the local network. The same
secret-based authentication applies; the traffic is plain WebSocket on the LAN.

`weave host --local` exposes no remote endpoint at all.

## Local secrets

Two secrets live on disk, both under `.git/weave`, both written with restrictive
permissions (mode `0600` on Unix, an inheritance-stripped user-only ACL on Windows):

| File | Contents |
| --- | --- |
| `runtime.json` | daemon pid, loopback IPC port, local IPC token |
| `session.json` | session identity and the session secret |

The local control endpoint binds **loopback only** and requires the bearer token
from `runtime.json`. It is never exposed publicly.

Neither the session secret, the IPC token, the full invite nor file contents are
ever written to logs. Verbose logging is limited to revision IDs, operation IDs,
paths, hashes, connection lifecycle events and Git OIDs.

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
