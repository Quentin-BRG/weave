---
name: weave-conflict
description: Use when weave status reports open conflicts, or after a Weave operation reports that changes could not be merged automatically. Reads every preserved candidate, produces one coherent reconciled file, and resolves the conflict atomically.
---

# Resolving a Weave conflict

Weave merges independent concurrent text edits automatically. When two changes
touch the same lines, or a binary file is edited concurrently, or one side deletes
what another modified, Weave stops and creates an explicit **conflict** instead of
guessing.

Two guarantees shape how you work here:

- **No work is ever discarded.** Every candidate is stored durably.
- **The canonical file never contains Git conflict markers.** Weave discards
  merge-marker output rather than materializing it.

## Workflow

1. List conflicts:

   ```
   weave conflict list --json
   ```

2. Read everything about one conflict:

   ```
   weave conflict show <id> --json
   ```

   Read all of it:

   - `candidates.base` — the common ancestor content.
   - `candidates.canonical` — what the host currently holds; this is authoritative.
   - `candidates.incoming` — the rejected candidate.
   - `candidates.local` — the newest local candidate on the originating machine,
     when the author kept editing after submitting. Prefer this over `incoming`:
     it contains their latest work.
   - `incoming_actor`, `incoming_task` — who was doing what, and why.
   - `candidate_files` — the same content written to `.git/weave/conflicts/<id>/`
     so you can diff it with ordinary tools.
   - `working_tree_path` — the file to edit.

3. **Infer both intentions.** The point is not to pick a winner; it is to produce
   the result both authors would have wanted.

4. **Work from the current canonical file.** The working file has already been
   restored to canonical content, and the path is in conflict-draft mode: your edits
   are captured durably but are deliberately *not* auto-submitted, so the watcher
   cannot race ahead of your resolution.

5. Produce **one coherent reconciled file**. Never write `<<<<<<<`, `=======` or
   `>>>>>>>` by hand.

6. Validate when it makes sense — build, render, run the relevant check.

7. Resolve:

   ```
   weave conflict resolve <id>
   ```

   By default this submits the current working-tree content as one atomic
   resolution. Other sources are available when appropriate:

   ```
   weave conflict resolve <id> --use canonical    # keep the host content
   weave conflict resolve <id> --use local        # take the latest local candidate
   weave conflict resolve <id> --use incoming     # take the rejected candidate
   weave conflict resolve <id> --use delete       # resolve by deleting the path
   weave conflict resolve <id> --content-file <p> # supply bytes from a file
   ```

   If the conflict is not a real problem and canonical state should simply stand:

   ```
   weave conflict dismiss <id>
   ```

## The raw Git rule

Resolving a conflict never involves Git. While a Weave session is active, DO NOT
run:

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

This applies to **host agents and participant agents alike**. In particular, do not
try to "fix" a conflict with `git checkout`, `git merge` or `git reset` — Weave
conflicts are not Git conflicts, the canonical file is not in a Git conflicted
state, and touching Git will only pause synchronization. Read-only Git
(`git status`, `git diff`, `git log`, `git show`) stays allowed and is useful for
understanding the change.

## `ResolutionOutdated`

The host verifies that the canonical file has not changed since your resolution was
based on it. If it has, the resolution is refused with `ResolutionOutdated` and the
conflict stays open. That is not an error on your part: re-read the file, redo the
reconciliation against the new canonical content, and resolve again.

## Conflicts block publication

`weave commit prepare` fails while any conflict is open. Resolve or dismiss them all
before preparing a Git publication.
