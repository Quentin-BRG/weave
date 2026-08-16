# Weave CLI reference

Global options:

| Option | Meaning |
| --- | --- |
| `--repo <DIR>` | Operate on a different repository directory |
| `--verbose` | Verbose diagnostics on stderr (`WEAVE_LOG` overrides the filter) |
| `--json` | Machine-readable output (per subcommand) |

`--json` guarantees stable field names, no interactive prompts, the result on
stdout, diagnostics on stderr, and meaningful non-zero exit codes.

## Exit codes

| Code | Class |
| --- | --- |
| 0 | success |
| 2 | `UsageError` |
| 3 | `RepositoryError` |
| 4 | `SessionError` |
| 5 | `NetworkError` |
| 6 | `ProtocolError` |
| 7 | `ConflictError` |
| 8 | `GitError` |
| 9 | `IntegrityError` |
| 10 | `UnsupportedError` |
| 11 | `PersistenceError` |

With `--json`, a failure also prints `{"ok": false, "class", "message", "detail"}`
on stdout.

## Environment

| Variable | Effect |
| --- | --- |
| `WEAVE_HOME` | User-level Weave data directory (holds the installation actor identity). Lets one machine run several independent Weave identities. |
| `WEAVE_LAN_ADDRESS` | Address advertised to participants in `--lan` mode, when the default-route interface is not the reachable one. |
| `WEAVE_LOG` | `tracing` filter, e.g. `weave=debug`. |

---

## Session lifecycle

### `weave host [--lan] [--local]`

Starts the coordinator and this machine's replica. Requires a valid Git repository
with a checked-out branch, at least one commit, no Git operation in progress, and —
for a new session — a clean working tree. `r0` is the host working tree at session
creation.

- default: binds loopback and launches `cloudflared tunnel`
- `--lan`: binds all interfaces, no Cloudflare process
- `--local`: no remote endpoint at all

Prints the invite. Runs until Ctrl-C or `weave stop`.

### `weave join [--invite-file <PATH>] [--invite-stdin]`

Joins an existing session. You must already have a checkout of the same repository,
clean, on the session branch, at the session base commit (or at the latest
Weave-published commit). Weave does not clone.

Without a flag, the invite is read from a hidden prompt, because it contains the
session secret.

### `weave resume`

Restarts this repository's session after a crash or a `weave stop`, using the stored
session record. On the host this re-validates SQLite integrity, blob references, the
Git branch and any incomplete publication journal, then restarts the coordinator and
the transport. A resumed remote session normally receives a new Quick Tunnel URL and
prints a new invite; the logical session, its ID, its secret, its canonical state,
its Tasks and its conflicts are unchanged.

### `weave stop` / `weave leave`

`stop` shuts the daemon down and keeps the session record so `weave resume` works.
`leave` also forgets the record. Neither touches the working tree.

---

## Inspection

### `weave status [--json]`

```
Weave — investor-deck

Role: participant
Host: Quentin

Branch: main

Git publication:
8f21abc
Revision: r500

Live:
r548
48 revisions ahead

State:
9ae15f2c...

Connection:
online

Participants:
✓ Quentin
✓ Alice
✓ Bob

Active Task:
T-102 — Update market statistics

Outbox:
0 pending

Conflicts:
0
```

JSON fields include `active`, `role`, `branch`, `base_commit`, `git_publication`,
`published_revision`, `live_revision`, `revisions_ahead`, `state` (the deterministic
replica hash), `connection`, `sync_state`, `participants`, `active_task`,
`outbox_pending`, `conflicts_open`, `conflict_drafts`, `rejected_paths`, `notices`
and `file_count`.

When no session is running, `weave status --json` prints `{"active": false, …}` and
exits 0 — an agent can branch on it safely.

### `weave peers [--json]`

Actor, display name, role, online state, last known revision and active Task.
Presence is informational and ephemeral.

### `weave invite [--json]`

Reprints the current invite (host only). Fails for `--local` sessions.

### `weave rescan [--json]`

Forces an authoritative full repository rescan. Weave already rescans on start,
after reconnect, on watcher error, before a commit barrier and on divergence; this
is the manual escape hatch.

---

## Tasks

```
weave task start --description <TEXT> [--file <SCOPE>]... [--json]
weave task list [--json]
weave task show <ID> [--json]
weave task update <ID> [--description <TEXT>] [--file <SCOPE>]... [--json]
weave task complete <ID> [--json]
weave task cancel <ID> [--json]
```

A scope is `path` or `path:START-END`, for example
`slides/07-pricing.tsx:50-110`. Scopes are **advisory soft locks**: overlap is
reported, never enforced.

IDs accept the short form (`T-1A2B`) or the full UUID.

One active Task per participant. Editing without a Task is allowed; those revisions
carry `task_id: null` and `weave commit prepare` reports them as unassigned work.

---

## Conflicts

```
weave conflict list [--json]
weave conflict show <ID> [--json]
weave conflict resolve <ID> [--use working|canonical|local|incoming|delete]
                            [--content-file <PATH>] [--json]
weave conflict dismiss <ID> [--json]
```

`show` writes every preserved candidate to `.git/weave/conflicts/<short-id>/` and
returns inline text previews for text candidates, plus the incoming actor, the
incoming Task and the current canonical entry.

`resolve` defaults to the current working-tree content and submits **one atomic
request** carrying the expected canonical entry. If canonical moved meanwhile the
host answers `ResolutionOutdated` (exit code 7) and the conflict stays open — re-read
the file and resolve again.

---

## Git publication

### `weave commit prepare [--allow-active-tasks] [--json]`

Runs the synchronization barrier and binds the publication to one immutable target
revision. It does **not** create a Git commit.

Fails when a conflict is open, or when an active Task contributed accepted revisions
that would be inside the target. `--allow-active-tasks` overrides the second check
deliberately.

JSON includes `prepare_id`, `target_revision`, `previous_published_revision`,
`parent_commit_oid`, `included_tasks` (descriptions, actors, touched paths,
revisions), `touched_paths`, `unassigned_revisions`, `contributors`, `diff_summary`
and `disconnected_participants`.

### `weave commit create <PREPARE_ID> --message <TEXT> | --message-stdin [--json]`

Creates the prepared publication on the host. There is no implicit "latest
preparation"; the ID is required. The Git tree represents the prepared revision, not
the live working tree. After it lands:

```
HEAD / index = published revision
working tree = latest live revision
```

so later work correctly remains visible as uncommitted changes.

### `weave push [--json]`

Any participant may request it; only the host Git process performs it. Automatic
push after publication is the default when an upstream exists. A diverged remote is
reported and never auto-reconciled.

---

## Tunnel

### `weave tunnel restart [--json]`

Replaces a dead Quick Tunnel. The session keeps its ID, secret, canonical state,
Tasks and conflicts, and receives a new URL and a new invite. Tunnel identity is
not session identity: nothing has to be recreated because a URL changed.

Participants are still pointed at the old hostname, so they go offline. Local
editing continues and every change stays durably queued; each participant rejoins
with the new invite:

```bash
weave stop
weave join --invite-file new-invite.txt
```

Their queued work is reconciled on reconnect, exactly as after any other outage.
The new hostname can take a few seconds to resolve; Weave retries until it does.

---

## Agents

### `weave agent bootstrap [--json]`

Creates or updates the managed block in the repository's root `AGENTS.md`, leaving
every unrelated instruction untouched. The block says "check `weave status`; if a
session is active, follow Weave", so it can stay committed permanently.

---

## Diagnostics

### `weave doctor [--json]`

Checks the Git executable, repository state, unsupported repository features,
filesystem writability, Weave metadata, SQLite, `cloudflared`, path portability, Git
filter compatibility and active external Git operations. Exits non-zero when Weave
is not ready.

### `weave recover [--rebuild] [--export <DIR>] [--json]`

Verifies revision and blob references, detects an incomplete Git publication, and
reports outbox state. `--rebuild` reconstructs the derived canonical manifest from
the durable revision history. `--export` copies the latest recoverable canonical
files to a safe directory. Recovery always prefers preserving data over automatic
destructive repair.

### `weave config list|get <KEY>|set <KEY> <VALUE> [--json]`

`display_name` is writable; `actor_id` is read-only.
