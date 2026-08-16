---
name: weave-commit
description: Use when work in a Weave session should become a Git commit. Prepares an immutable publication target, writes a semantic commit message from Task history and the diff, and creates the publication through Weave rather than raw Git.
---

# Publishing a Weave session to Git

Weave revision history is *how collaboration actually unfolded*. Git history is
*how the team chooses to publish the completed work*. They are separate on purpose:
the working tree sits at the latest live revision while Git HEAD sits at the latest
published one.

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

This applies to **host agents and participant agents alike**. Weave owns every
Git-writing operation during a session; publishing is what this skill is for.
Read-only Git (`git status`, `git diff`, `git log`, `git show`) stays allowed and
is genuinely useful when writing the commit message.

## Who does what

Any authenticated participant may *request* a publication. Only the **host
coordinator** builds Git blobs, trees and the commit object, updates the canonical
branch, distributes the exact Git objects to every participant, and pushes to the
remote. Your machine never constructs the canonical commit, even if you are the one
running the commands. This removes any dependence on differing local Git filters.

## Workflow

1. Prepare:

   ```
   weave commit prepare --json
   ```

   This runs a short synchronization barrier across connected participants, then
   binds the publication to one immutable `target_revision`. Live editing continues
   afterwards; the preparation does not move.

   Read the result:

   - `prepare_id` — required by `commit create`. There is no implicit "latest".
   - `target_revision`, `previous_published_revision`, `parent_commit_oid`.
   - `included_tasks` — descriptions, contributors and touched paths. **This is
     your raw material for the commit message.**
   - `unassigned_revisions` — accepted work with no Task. It is included in the
     commit; mention it if it is substantial.
   - `contributors` — who contributed accepted revisions.
   - `diff_summary` — `added` / `modified` / `deleted` paths.
   - `disconnected_participants` — if non-empty, warn the user: unsynchronized local
     work on those machines cannot be part of this commit.

2. If preparation is **rejected** because a contributing Task is still active,
   finish or cancel that Task and prepare again:

   ```
   weave task complete <id>
   weave commit prepare --json
   ```

   Preparation is also rejected while any conflict is open. See the
   `weave-conflict` skill.

3. Inspect the actual change if the summary is not enough:

   ```
   git diff --stat
   git diff
   ```

   (Read-only Git is allowed.)

4. Write **one concise semantic message**. Weave never invents it. Describe the
   outcome, not the mechanics:

   ```
   docs: refine market narrative and pricing slides
   ```

5. Create the publication:

   ```
   weave commit create <prepare_id> --message "docs: refine market narrative"
   ```

   The message may also be supplied on stdin with `--message-stdin`.

6. Verify the result. The JSON response contains `descriptor.commit_oid`,
   `descriptor.target_revision` and `push_status`. If `push_status` is not
   `pushed`, report why. A push failure never invalidates the local publication;
   live collaboration continues and the push can be retried:

   ```
   weave push
   ```

   If the remote branch diverged because of external commits, Weave refuses to pull,
   merge or rebase automatically. Say so plainly and let the user decide.

## What the commit contains

The Git tree represents the **prepared revision**, reconstructed from Weave history
on the host — not the current working tree. After publication:

```
HEAD / index = published revision
working tree = latest live revision
```

so everything produced after the prepared revision correctly remains visible as
uncommitted work. That is expected, not a mistake to "fix" with Git.
