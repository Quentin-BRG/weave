# Session lifecycle and Git adoption (protocol 4)

This document updates the V1 behavior for stopped sessions, admission, shared
ignores and recovery. The invariants in SPEC-COVERAGE.md still apply: preserve
edits, one canonical state, ordered revisions and durable acknowledgements.

## Stop, leave and status

`stop` and Ctrl+C stop the daemon and preserve the session. After a successful
`status --json` reports `daemon_state: "stopped"`, ordinary Git operations are
allowed. `session_saved` indicates whether `resume` can reopen the session.
An IPC timeout, permission error or corrupt runtime is an error, not evidence
that the daemon stopped. A failed status exits nonzero with a structured error.

`leave` waits for shutdown, takes the repository lock, archives state and local
work, and removes the session record. It works without a running daemon and is
idempotent. The working tree remains available. A success means departure has
completed. Runtime cleanup and engine shutdown happen before releasing the lock.

## Git while stopped

The supported host workflow is `stop`, ordinary Git on the same branch, then
`resume`. Pulls, direct commits, rebases and rewinds are accepted. A different
branch requires leaving and creating a new session. Git changes while running
still pause collaboration.

The immutable initial commit is historical metadata. `git_state` identifies the
current adopted branch, commit, tree, sequence and origin (`initial`, `external`
or `weave`). `git_publication` identifies the last publication produced by Weave.
An external adoption creates no synthetic Git commit and performs no push.
`unpublished_changes` compares the canonical content with that adopted Git tree;
it does not count external adoption revisions as unpublished collaborative work.

Before serving participants, the host archives its state and index, reads both Git
trees and reconciles unpublished canonical changes and additional local changes.
Incompatible candidates become explicit conflicts; Git supplies their initial
canonical version. Revision numbers, operation results, Tasks and pending work
are retained. A durable adoption journal replays each operation idempotently.
Commit preparation compares against the adopted Git tree, and stale preparations
are invalidated by adoption or changes to the excluded path set.

## Joining and recovery

After the Noise handshake, the host announces its current Git state and a complete
pack on the existing encrypted blob plane. The client verifies the commit/tree,
protects its old commit under `refs/weave/recovery/`, archives its index and work,
and journals its conditional reference update and index installation. The expected
old reference is the observed local commit, which need not be a parent. Canonical
state waits behind this alignment; subsequent publications install in order.

A first join with local work asks for backup, discard of affected changes, or
cancel (the default). Automation supplies `--local-changes=backup|discard|cancel`.
No blanket `git clean` or `reset --hard` is used. Backup keeps the original work
separate. Unrelated untracked files remain local until edited, and affected ignored
files require the same explicit policy. Recovery archives retain independent
copies of their blobs; ordinary collection cannot delete their content.

```
weave recover --list --json
weave recover --backup <id> --export <new-directory>
```

Exports contain the saved SQLite databases, blobs, original index where captured,
independent staged blobs under `index-objects/` (with their NUL-delimited Git stage
listing in `stages.z`), and `working-tree/` files. They are recovery material, not an automatic merge into
a live session. Commit recovery references also protect local Git history.

## LAN and connection health

Hosts try the previous LAN port on resume and choose a new one if it is occupied.
`WEAVE_LAN_ADDRESS` overrides the advertised address. The host checks the address
every five seconds and reports changes. `weave invite refresh` durably updates the
LAN address and prints an invitation with the same session identity and secret.
The host shares it manually; clients can supply it to `weave join` while running.
The replacement address is authenticated before it is saved. There is no network
discovery mechanism.

Encrypted pings run every five seconds. Missing responses expire the connection
after fifteen seconds. TCP/WebSocket connection establishment has a ten-second
limit, separate from the Noise handshake limit. Reconnection backs off to at most
eight seconds. Status distinguishes transport connection, session admission and
completed synchronization, and reports pending work and the last host response.
The last response timestamp advances only on an authenticated heartbeat reply.

## Shared ignores and diagnostics

Git evaluates the host's canonical `.gitignore` files independently of personal
excludes. Paths tracked by the adopted commit remain tracked. Incoming paths are
checked by the host. Newly excluded untracked canonical files consume ordered
removal revisions, but replicas preserve their local files. Excluded candidates
are archived and their conflicts stop blocking publication. A retained session is
cleaned by the same mechanism.

A materialization error is isolated to its path and reported in `rejected_paths`;
it prevents a complete-sync claim and a successful publication barrier. Ignored
npm launchers are not rewritten. Git identity is reread when preparing or creating
a publication, with participant identity updates bound to the authenticated peer.

## Compatibility

All participants must use application protocol 4. Noise framing, the encrypted
route and the `weave2_` prefix remain unchanged. New invitations omit the commit.
Old encrypted invitations from protocols 2 and 3 can supply an address and secret;
actual peers must still speak protocol 4. Plaintext `weave1_` invitations remain
rejected.

Schema 1 databases are copied before a transactional migration to schema 2.
Metadata moves to `meta_v4`; the legacy `meta` view deliberately requires protocol
4, causing older releases (which ignored schema versions) to fail on opening it.
Do not run a pre-upgrade binary on a migrated session. Backups retain the original
databases for export and inspection.
