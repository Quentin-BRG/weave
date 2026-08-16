---
name: weave-collaboration
description: Use whenever you are about to read or modify files in a repository that may be in a Weave live collaboration session. Teaches the shared-working-tree model, how to check for an active session, and the rule that Weave owns all Git-writing operations.
---

# Working inside a Weave session

Weave is a real-time collaboration layer above Git. Several people and agents edit
their own local copies of the same repository at the same time; one authoritative
host coordinator keeps a single shared live state. Git keeps its ordinary role:
durable history, branch identity, remotes.

There is nothing to call to "sync". You edit files normally; Weave observes the
filesystem and propagates changes.

## First: is a session active?

Before substantial file modifications, run:

```
weave status --json
```

- `"active": false` — no Weave session. Normal Git workflows apply. Stop here.
- `"active": true` — the working tree is **live shared state**. Follow the rest of
  this skill.

Useful fields: `role`, `branch`, `live_revision`, `published_revision`,
`revisions_ahead`, `connection`, `sync_state`, `participants`, `active_task`,
`outbox_pending`, `conflicts_open`, `rejected_paths`.

## How to behave in a live session

1. **Treat files as live shared state.** Someone else may be editing the same file
   right now.
2. **Expect files to change while you work.** Weave can rewrite a file underneath
   you when the host accepts someone else's change.
3. **Run `weave status --json` before substantial edits**, and inspect active Tasks
   with `weave task list --json`.
4. **Create a Weave Task before substantial changes** so collaborators can see your
   intent. See the `weave-task` skill.
5. **Re-read important files before finalizing changes** when `weave status --json`
   shows concurrent activity (other online participants, or `live_revision` moving).
   Do not rely on a copy you read several minutes ago.
6. **A soft lock is not exclusive permission.** An overlapping Task means "someone
   is working here"; it never means "do not touch".
7. **Never use raw Git write operations.** See below.
8. **Use Weave for Tasks, conflicts, commits and pushes**, never Git directly.

## The raw Git rule

While a Weave session is active, DO NOT run:

```
git add
git commit
git pull
git push
git merge
git rebase
git cherry-pick
git reset
git checkout
git switch
git stash
```

This applies to **host agents and participant agents alike**.

Weave detects Git state changed outside itself and pauses synchronization until the
expected state is restored; it will never repair it for you.

Read-only Git remains allowed and useful:

```
git status
git diff
git log
git show
```

## When Weave is paused or offline

- `sync_state.state == "paused"` — Git state changed outside Weave. Report the
  `reason` and `detail` to the user. Do not attempt to fix it with Git commands
  unless the user explicitly asks.
- `connection == "offline"` — the host is unreachable. Local editing is safe and
  every change is durably queued in the local outbox; it will be reconciled on
  reconnect. Do not try to work around it.

## Paths Weave cannot synchronize

`weave status --json` reports `rejected_paths`. Weave enforces a portable filename
subset so Windows, macOS and Linux participants can hold the same tree, and refuses
files above 10 MiB. If a file you need appears there, rename it or leave it out of
the session; do not try to force it through.
