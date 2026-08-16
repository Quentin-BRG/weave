---
name: weave-task
description: Use before making substantial changes in an active Weave session. Creates and maintains a Weave Task describing intent, declares advisory file and line-range scopes, and reports overlaps with other participants' Tasks.
---

# Weave Tasks

A Task describes **intent**: what this participant or agent is trying to change.
Weave never executes a Task, schedules it, or assigns it to anyone. Its scopes are
**advisory soft locks**: informational, never enforced.

A participant may have **one active Task at a time**. Editing without a Task is
allowed; those revisions simply carry `task_id: null`.

## Workflow

1. Check the current state:

   ```
   weave task list --json
   ```

   Read the active Tasks and note anything that overlaps what you are about to do.

2. Create one concise Task describing the work, declaring the files you expect to
   touch. A line range is optional:

   ```
   weave task start \
     --description "Rewrite the pricing slide around enterprise value" \
     --file slides/07-pricing.tsx \
     --file slides/07-pricing.tsx:50-110
   ```

3. Inspect the reported overlap:

   ```
   weave task show <id> --json
   ```

   The `overlaps` array lists other active Tasks whose scopes intersect yours.
   **Treat overlap as context, not permission denial.** Starting the Task still
   succeeds. If the overlap looks like a genuine collision, tell the user and
   consider working elsewhere first.

4. Perform the edits normally. Weave attributes every accepted revision you produce
   to your active Task and records the paths it actually touched, so you do not have
   to predict them perfectly.

5. If the work expands beyond the declared scope, update it:

   ```
   weave task update <id> --description "..." --file <path> --file <path>
   ```

6. Complete the Task when the logical work is stable:

   ```
   weave task complete <id>
   ```

   Use `weave task cancel <id>` if the work is abandoned.

## Why completing matters

`weave commit prepare` **fails by default** while an active Task has contributed
accepted revisions that would be inside the prepared commit. A publication must
never claim to exclude work whose bytes it is actually committing. Complete or
cancel the Task first.

An active Task that has contributed no accepted revisions since the last
publication does not block anything.

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

This applies to **host agents and participant agents alike**. Completing a Task is
not a reason to reach for Git: publication goes through `weave commit prepare` and
`weave commit create`, and only the host coordinator builds the canonical Git
commit. Read-only Git (`git status`, `git diff`, `git log`, `git show`) stays
allowed.

## Line-range scopes go stale

A line range is recorded against the file entry it was declared on. When the file
moves on and Weave cannot map the range safely, the scope is marked `stale` and
degrades to file-level overlap. This is expected; re-declare the range with
`weave task update` if precision matters.
