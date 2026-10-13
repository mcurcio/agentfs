# Runbook: a moved directory could not be reconciled

A directory rename is journaled as one move and then *reconciled* on the
commit thread: the destination is walked and anything that recording
every file there would have decided differently is recorded (design
[§3.1](../design.md#31-change-boundaries)). Reconciling kept failing,
so the move was committed as journaled. Nothing the workload wrote is
lost from the backing tree; history may differ from it beneath the
destination until each affected file changes again or is recorded by
hand.

## Symptoms

- A `ReconcileFailed` alarm (stderr `alarm: reconciling the move of
  <from> to <to> failed: <error>`, log event `agentfs.reconcile_failed`
  with both paths and the error).
- `agentfs.reconcile.failures` went up. Each earlier failed attempt is a
  line `reconciling the move of <from> to <to> failed (attempt <n>): ...`
  on stderr.
- Commits were held back while it retried: a duty that fails is retried
  after 0.5 s, doubling to 8 s, and is given up only after 5 failures
  spanning at least 30 s. `agentfs.journal.backlog` grows meanwhile and
  drains once it is given up.

Files that vanish while the walk runs (a build deleting temporaries), or
are replaced by a symlink, FIFO or device, are skipped, never a failure.
A failure is an I/O error: reading a directory or a file under the
destination (`EIO`, `EACCES` on a directory the daemon cannot read,
`EMFILE`), freezing a copy into `pending/` (`ENOSPC`), or appending to
the journal.

## What may be wrong in history

Beneath `<to>` only, until each path changes again:

- files that were not tracked before the move (renamed out of an ignored
  directory) are missing;
- moved files the rules exclude at the new path (renamed into an ignored
  directory or a nested repository) are still there;
- a file changed within 60 s of the move whose last change was not
  journaled (another front's write that raced the rename, or a change
  made outside the mount) has its old content.

Files refused at their new path (beneath `.gitmodules`, too long, over
`agentfs.maxSize`) are never in history after a move, reconciled or not.

## Fix

1. Find the cause in the error: disk space and permissions of the state
   area (`pending/`, `journal/`), and of the backing tree under `<to>`.
2. Compare history with the tree under the destination:

   ```sh
   agentfs --state "$STATE" --root "$BACKING" status
   git --git-dir "$STATE/git" ls-tree -r --name-only agentfs/<volume> -- '<to>/'
   (cd "$BACKING" && find '<to>' -type f -o -type l | sort)
   ```

3. Record what differs. `agentfs record` takes files and symlinks, and
   records a path that is gone as its removal:

   ```sh
   cd "$BACKING" && find '<to>' \( -type f -o -type l \) -print0 |
     xargs -0 agentfs --state "$STATE" --root "$BACKING" record
   ```

   With the mount running, pass `--history-service` as for any front
   (single-process mounts: stop the mount first; the state area is
   locked while it runs). Recording an unchanged file changes nothing in
   history.

If the alarm repeats for every move, the state area is likely full or
read-only: see `agentfs.journal.backlog` and the free space of its
filesystem.
