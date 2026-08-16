# Working on Weave itself

Weave is a real-time collaboration layer above Git. The V1 specification is the
source of truth; `docs/SPEC-COVERAGE.md` maps every specification section to the
code that implements it. Read it before changing anything structural.

## Priorities, in order

When a low-level detail is undefined, choose the simplest behaviour that preserves:

1. **No lost edits.**
2. One authoritative canonical state.
3. Deterministic global revision ordering.
4. Durable acknowledgement semantics.
5. Explicit rather than silent conflict.
6. No corruption of the ordinary Git repository.
7. Live collaboration without manual Git synchronization.
8. Minimal complexity appropriate to small document and slide projects.

A feature that weakens one of these to support a broader use case is outside V1.

## Layout

- `src/host.rs` — the coordinator state machine (the only assigner of revisions).
- `src/client.rs`, `src/client_ipc.rs` — the replica state machine and CLI handling.
- `src/reconcile.rs` — the three-way reconciliation matrix.
- `src/store_host.rs`, `src/store_client.rs` — durable state.
- `src/gitx.rs` — every call into the `git` executable.
- `weave-plugin/` — the Codex plugin: skills only, no MCP server.
- `tests/` — end-to-end tests driving the real binary against real repositories.

Both engines are synchronous single-threaded state machines on their own OS
threads. Async code belongs to sockets, child processes and timers only. Do not
introduce shared mutable canonical state across threads.

## Before you commit

```
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
```

The end-to-end tests spawn real daemons and bind loopback sockets, so they must run
one at a time.

`tests/remote_tunnel.rs` is the only test that leaves the machine. It is
`#[ignore]`d so ordinary runs never depend on `cloudflared`, on Cloudflare, or on
outbound network access. Run it when you touch the transport, the tunnel lifecycle
or the invite format:

```
cargo test --test remote_tunnel -- --ignored --test-threads=1 --nocapture
```

If you change reconciliation, the outbox, materialization or publication, add or
extend a test in `tests/` that fails without your change. Correctness here is not
something to assert in a comment.

<!-- weave:begin -->

## Weave collaboration

This repository supports Weave live collaboration.

Before substantial file modifications, run:

    weave status --json

If a Weave session is active:

- Treat the working tree as shared live state; other people and agents are editing it now.
- Follow the installed Weave collaboration/task/conflict/commit skills.
- Create a Weave Task before substantial changes:
  `weave task start --description "..." --file <path>`.
- Inspect overlapping active Tasks with `weave task list --json`; overlap is context, not a lock.
- Re-read important files before finalizing changes when concurrent activity occurred.
- Never perform raw Git write operations: no `git add`, `commit`, `pull`, `push`, `merge`,
  `rebase`, `cherry-pick`, `reset`, `checkout`, `switch` or `stash`.
  Read-only Git (`status`, `diff`, `log`, `show`) stays allowed.
- Use Weave for Git publication: `weave commit prepare` then `weave commit create <prepare_id>`.
- Resolve conflicts with `weave conflict show/resolve`; never write Git conflict markers by hand.

A non-host may request a Weave commit, but only the host coordinator builds the canonical Git
objects, updates the branch and pushes.

If no Weave session is active, normal Git workflows apply.

<!-- weave:end -->
