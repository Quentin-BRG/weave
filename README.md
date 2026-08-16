# Weave

**A lightweight real-time collaboration layer above Git.**

Weave lets two to five people — and their agents — work simultaneously on
independent local copies of the same Git repository while one authoritative host
maintains a single shared live state. Git keeps its ordinary role: durable history,
branch identity, remotes, portability.

```
Quentin / Codex ──┐
Alice / Codex ────┼────── Weave ────── shared live state
Bob / Codex ──────┘                        │
                                    semantic publication
                                           │
                                           ▼
                                          Git
```

Weave is a collaboration primitive, not an agent manager, an IDE, a Git
replacement, a CRDT platform or a distributed filesystem. Everyone runs their own
editor and their own agent; Weave only keeps the files in sync and turns the result
into ordinary Git commits when the team decides to publish.

- **CLI only.** One `weave` binary, Windows / macOS / Linux.
- **One authoritative host.** No P2P, no CRDT, no leader election.
- **No lost edits.** Weave never overwrites local bytes it has not already captured
durably.
- **Explicit conflicts.** Independent edits merge; overlapping edits become a Weave
conflict with every candidate preserved. The working tree never receives generated
Git conflict markers.
- **Ordinary Git, always.** Remove Weave and the repository is still a normal,
fully usable Git repository.

---



## Install

Weave needs a recent **Rust stable** toolchain and the **git** executable on PATH.
`cloudflared` is needed only for remote sessions.

```bash
cargo install --path .
```

or build in place:

```bash
cargo build --release
# target/release/weave
```

Check the machine and the repository:

```bash
weave doctor
```

---



## Quick start



### Host a session

From a clean repository with a checked-out branch:

```bash
weave host
```

Weave prints an invite:

```
weave1_eyJ2IjoxLCJ1Ijoi...
```

Share it over a channel you trust — **the invite grants full read/write access to
the session.** Other options:

```bash
weave host --lan     # local network, no Cloudflare process
weave host --local   # this machine only, no remote endpoint
```



### Join a session

You must already have a checkout of the same repository, clean, on the same branch,
at the session base commit. Weave does not clone.

```bash
weave join
Paste Weave invite:
> ‹hidden›
```

For automation:

```bash
weave join --invite-file invite.txt
weave join --invite-stdin < invite.txt
```



### Work

Just edit files. Codex, Claude Code, VS Code, `sed`, a formatter — Weave does not
care which process wrote the file.

```bash
weave status
weave peers
```



### Describe intent

```bash
weave task start --description "Rewrite the pricing slide" --file slides/07-pricing.tsx
weave task list
weave task complete <id>
```

Task scopes are **advisory soft locks**. They tell collaborators "Alice is working
here"; they never prevent anyone from editing.

### Resolve a conflict

```bash
weave conflict list
weave conflict show C-8F21     # writes every candidate to .git/weave/conflicts/
# edit the working file into one coherent result
weave conflict resolve C-8F21
```



### Publish to Git

```bash
weave commit prepare --json
weave commit create <prepare_id> --message "docs: refine market narrative"
weave push          # only if the automatic push did not succeed
```

`commit prepare` binds the publication to one immutable live revision and runs a
short barrier so nothing in flight is missed. Live editing continues afterwards; the
prepared commit still represents the revision it was bound to.

---



## The rule that matters most

**While a Weave session is active, Weave owns every Git-writing operation** — for
the host and for participants alike.

Do not run `git add`, `commit`, `pull`, `push`, `merge`, `rebase`, `cherry-pick`,
`reset`, `checkout`, `switch` or `stash`. Weave detects Git state changed outside
itself and pauses synchronization until the expected state is restored; it will
never pull, merge or rebase on your behalf.

Read-only Git stays available and useful: `git status`, `git diff`, `git log`,
`git show`.

---



## Agents

Weave ships a plugin containing four skills and **no MCP server**, plus a
repo-local marketplace so the plugin is discoverable straight from a checkout:

```
.agents/plugins/
├── marketplace.json
└── weave/
    ├── .codex-plugin/plugin.json
    └── skills/
        ├── weave-collaboration/    SKILL.md + agents/openai.yaml
        ├── weave-task/             SKILL.md + agents/openai.yaml
        ├── weave-conflict/         SKILL.md + agents/openai.yaml
        └── weave-commit/           SKILL.md + agents/openai.yaml
```

The CLI is the protocol surface; the skills teach an agent when and how to use it.

**The skills are provider-neutral.** Each `SKILL.md` is a plain skill document —
`name` and `description` frontmatter, then instructions — so the same four skills
work unchanged in Codex, in Claude Code, or in any agent that reads the open skill
format. Nothing in them names a vendor, and nothing in the Weave core depends on
one: any agent that can run a shell command can participate. The
`.codex-plugin/plugin.json` manifest and the optional `agents/openai.yaml` sidecars
carry Codex packaging and presentation metadata only; other agents ignore them and
lose nothing.

Install it in Codex by registering the repository's marketplace, then installing
`weave` from the plugins directory:

```bash
codex plugin marketplace add Quentin-BRG/weave
# or, from a local checkout:
codex plugin marketplace add .
```

A repository-scoped marketplace is not picked up automatically — it has to be added
once, deliberately.

To reuse the same skills in Claude Code, point it at the skill directories:

```bash
cp -r .agents/plugins/weave/skills/* .claude/skills/    # this project only
cp -r .agents/plugins/weave/skills/* ~/.claude/skills/  # everywhere
```

No edits are needed; the `SKILL.md` files are the whole contract.

For a permanent, agent-independent activation hook, write the managed block into the
repository's `AGENTS.md`:

```bash
weave agent bootstrap
```

The block says "check `weave status`; if a session is active, follow Weave", so it
can stay committed forever and never needs updating when a session starts or stops.
It never overwrites unrelated instructions.

---



## Command reference


| Command                                             | Purpose                                                   |
| --------------------------------------------------- | --------------------------------------------------------- |
| `weave host [--lan|--local]`                        | Host a session (long-lived daemon)                        |
| `weave join [--invite-file|--invite-stdin]`         | Join a session (long-lived daemon)                        |
| `weave resume`                                      | Resume this repository's session after a crash or restart |
| `weave leave`                                       | Leave the session and forget its local record             |
| `weave stop`                                        | Stop the daemon, keeping the session record               |
| `weave status [--json]`                             | Live session state                                        |
| `weave peers [--json]`                              | Participants and presence                                 |
| `weave invite [--json]`                             | Reprint the invite (host)                                 |
| `weave rescan [--json]`                             | Force a full repository rescan                            |
| `weave task start|list|show|update|complete|cancel` | Tasks and soft locks                                      |
| `weave conflict list|show|resolve|dismiss`          | Conflict inspection and resolution                        |
| `weave commit prepare` / `weave commit create <id>` | Git publication                                           |
| `weave push`                                        | Ask the host to push                                      |
| `weave tunnel restart`                              | Replace a dead Quick Tunnel, same session                 |
| `weave agent bootstrap`                             | Manage the `AGENTS.md` block                              |
| `weave doctor`                                      | Readiness checklist                                       |
| `weave recover [--rebuild] [--export DIR]`          | Integrity diagnostics and safe recovery                   |
| `weave config list|get|set`                         | Local Weave configuration                                 |


Every command an agent drives supports `--json`: stable field names, no prompts,
machine-readable result on stdout, diagnostics on stderr, meaningful exit codes.

---



## What Weave refuses

Weave V1 rejects repositories using Git submodules, Git LFS, sparse checkouts,
secondary worktrees, tracked symlinks, gitlinks, custom clean/smudge filters or
`working-tree-encoding`. It enforces a portable filename subset so Windows, macOS
and Linux participants can hold the same working tree, and it refuses files above
10 MiB. `weave doctor` reports all of it up front.

---



## Tests

```bash
cargo test -- --test-threads=1
```

The end-to-end tests drive the real binary against real Git repositories, so they
spawn daemons and bind loopback sockets and must run one at a time.

One test leaves the machine: `tests/remote_tunnel.rs` runs a whole session over a
real Cloudflare Quick Tunnel — join from a `wss://…trycloudflare.com` invite,
bidirectional sync, Tasks, a conflict, a participant-requested publication, a
disconnect, and `weave tunnel restart`. It needs `cloudflared` and outbound HTTPS,
so it is opt-in:

```bash
cargo test --test remote_tunnel -- --ignored --test-threads=1 --nocapture
```



## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — how the pieces fit together
- [docs/PROTOCOL.md](docs/PROTOCOL.md) — the wire protocol and reconciliation rules
- [docs/CLI.md](docs/CLI.md) — every command, flag and JSON shape
- [docs/SECURITY.md](docs/SECURITY.md) — the security model and its limits
- [docs/SPEC-COVERAGE.md](docs/SPEC-COVERAGE.md) — where each specification section lives

---



## Security in one paragraph

Possession of the session secret grants full collaborative read/write authority over
the repository for the duration of the session. There are no user accounts, no
fine-grained authorization and no per-file permissions. Remote transport is
Cloudflare's HTTPS/WebSocket tunnel; Weave does not add application-level end-to-end
encryption on top of it. Read [docs/SECURITY.md](docs/SECURITY.md) before using
Weave with anything sensitive.

---



## License

Mozilla Public License 2.0. See [LICENSE](LICENSE).