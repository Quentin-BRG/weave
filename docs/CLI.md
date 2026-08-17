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

### `weave host [--lan] [--local] [--max-file-size <SIZE>]`

Starts the coordinator and this machine's replica. Requires a valid Git repository
with a checked-out branch, at least one commit, no Git operation in progress, and —
for a new session — a clean working tree. `r0` is the host working tree at session
creation.

- default: binds loopback and launches `cloudflared tunnel`
- `--lan`: binds all interfaces, no Cloudflare process
- `--local`: no remote endpoint at all
- `--max-file-size <SIZE>`: the session's file size limit, default `128MiB`

A repository already holding a file above the limit does not start a session: the
refusal names the files and offers both remedies. On a resumed session the flag
changes the limit, and is refused if it would drop below content the session
already carries.

All three protect the session identically: participants authenticate and encrypt
with the same Noise handshake whether they arrive over `wss://` or over a plain LAN
`ws://`.

Prints the invite. Runs until Ctrl-C or `weave stop`.

### `weave join [--invite-file <PATH>] [--invite-stdin]`

Joins an existing session. You must already have a checkout of the same repository,
clean, on the session branch, at the session base commit (or at the latest
Weave-published commit). Weave does not clone.

Without a flag, the invite is read from a hidden prompt, because it contains the
session secret.

The session's file size limit decides whether a repository can be joined, and it
is only known once the host answers, so `weave join` says nothing about file
sizes before then. The `128MiB` default belongs to `weave host`, not to a join: a
session running at `512MiB` accepts a repository holding a 200 MiB file without
comment. If the session's own limit is genuinely too small, the join is refused
once the host has said so, naming the files.

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
`outbox_pending`, `conflicts_open`, `conflict_drafts`, `rejected_paths`, `notices`,
`file_count`, `max_file_size`, `oversize` and `disk`.

`oversize` lists every path in the session that is above the file size limit,
whoever owns it: `path`, `size`, `display_name`, `mine`, and `canonical` (true when
the session already holds earlier content for that path). It is empty in the
ordinary case. `disk` carries `available_bytes` — absent, rather than zero, on a
platform that will not report it.

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

Fails when a conflict is open, when any participant is holding back a file above
the session file size limit, or when an active Task contributed accepted revisions
that would be inside the target. `--allow-active-tasks` overrides the last check
deliberately; the other two are not overridable, because both mean some replica
cannot reproduce the state that would be committed.

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

## The session file size limit

### `weave limit show [--json]`

The limit in force, the largest file the session currently carries, and anything
above the limit right now. JSON fields: `max_file_size`, `max_file_size_human`,
`control_version`, `largest_file` and `oversize`.

### `weave limit set <SIZE> [--json]`

Changes the limit for the whole session. Any participant may ask; the host is the
authority and every replica observes the new value at the same `control_version`,
re-evaluating its own working tree against it — files that were held back are
captured and synchronized, and files that no longer fit start being held back.

`SIZE` is written the way people write it: `256MiB`, `512MB`, `2G`, or a plain
number of bytes. Both spellings of a unit mean the binary one. The limit must be
between 1 MiB and 8 GiB.

Lowering it below content the session already holds is refused, naming the file
that prevents it: canonical content cannot be un-synchronized by changing a number.

Prints the resulting state, so the output is the limit that is actually in force
rather than the one that was requested.

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

### `weave doctor [--install] [--json]`

A troubleshooting command, not a setup step. Nothing needs to be run after
installing: the installers already ran `weave doctor --install`, and `weave host`
and `weave join` run the checks they need on their own (see
[Automatic preflight](#automatic-preflight)).

| Invocation | Scope | Needs a repository |
| --- | --- | --- |
| `weave doctor` | Machine, installation **and** the current repository | yes |
| `weave doctor --install` | Machine and installation only | no |

`--install` covers what an installer can verify: the executable starts and
reports its version, the OS and CPU architecture are supported, the bundled
`cloudflared` is present, runs and matches the version the package claims, the
installation's licence files are readable, Git is available and recent enough,
and the bundled SQLite is usable. It never reads the current directory, a Git
repository, or user-level Weave state — the `.deb` and `.pkg` run it as root.

Without `--install`, the same checks run first and the repository checks follow:
repository discovery, `.git` usability, the checked-out branch, unsupported
repository features, filesystem writability, Weave metadata storage, path
portability, an already-running Weave daemon, and any Git operation in progress.

Exits non-zero when Weave is not ready.

`--json` prints one document and nothing else, on both success and failure —
`ready: false` is how a failure is reported, so the output stays parseable:

```json
{
  "scope": "install",
  "checks": [
    { "name": "Weave executable", "status": "pass", "detail": "…/weave (weave 1.0.0)" },
    { "name": "Platform",         "status": "pass", "detail": "linux x86_64" },
    { "name": "Weave package",    "status": "pass", "detail": "linux-x64-deb (weave 1.0.0)" },
    { "name": "Bundled cloudflared", "status": "fail",
      "detail": "no cloudflared in this installation (looked in …).",
      "hint":   "Reinstall Weave; the package is incomplete." }
  ],
  "ready": false
}
```

`scope` is `install` or `full`. `status` is `pass`, `warn` or `fail`; `hint` is
present only when there is something to do about it. `ready` is false if and only
if some check failed — warnings never make Weave unready.

### Automatic preflight

`weave host` and `weave join` reuse the `doctor` checks — the same functions, not
a second copy — restricted to what the requested mode actually needs. `weave
join` never involves `cloudflared`; `weave host --lan` and `weave host --local`
do not either. A remote `weave host` does.

The outcome is graded, and only the successful path is quiet:

- **Blocker** — Weave refuses to start, with one line naming the cause and one
  line saying what to do:

  ```
  Weave cannot start: this directory is not inside a Git repository.

  Run Weave from inside a Git working tree.

  Run `weave doctor` for full diagnostics.
  ```

- **Warning** — printed to stderr as `! name — detail` and startup continues. A
  dirty working tree is a warning, not a refusal.
- **Informational** — not printed at startup at all; `weave doctor` shows it.

A successful start prints no diagnostics. There is no doctor report on every
`weave host`.

### `weave recover [--rebuild] [--export <DIR>] [--json]`

Verifies revision and blob references, detects an incomplete Git publication, and
reports outbox state. `--rebuild` reconstructs the derived canonical manifest from
the durable revision history. `--export` copies the latest recoverable canonical
files to a safe directory. Recovery always prefers preserving data over automatic
destructive repair.

### `weave config list|get <KEY>|set <KEY> <VALUE> [--json]`

`display_name` is writable; `actor_id` is read-only.
