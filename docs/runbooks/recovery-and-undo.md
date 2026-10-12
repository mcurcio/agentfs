# Runbook: recovery and undo

Bring files back to an earlier state after an agent (or anyone) changed
them by mistake. Design: [§15](../design.md#15-recovery-and-undo).
`ci/e2e-recovery.sh` runs this procedure end to end through a real mount
in CI.

Use it when a file or directory in a volume holds the wrong content, or
a whole change (one commit) must be undone. For content that must leave
history altogether (a recorded secret) use
[history-rewrite.md](history-rewrite.md) instead: a restore adds a new
commit, it never removes old ones.

Throughout:

```sh
STATE=/data/state       # the state area
BACKING=/data/backing   # the backing tree
VOL=hermes              # the volume (agentfs.volume)
BRANCH=refs/heads/agentfs/$VOL
```

## 1. Find the change

History is ordinary git, in the state area and on every remote it was
pushed to. Reading it needs no lock and is safe while the volume is
mounted.

```sh
# Every version of a file, newest first, with what changed it.
git --git-dir "$STATE/git" log --follow --format='%H %ad %s' --date=iso "$BRANCH" -- notes/plan.md
# One version's content, and a whole change.
git --git-dir "$STATE/git" show <commit>:notes/plan.md
git --git-dir "$STATE/git" show --stat <commit>
```

- **Trailers** ([§9](../design.md#9-commit-metadata)) say who made each
  change: `Agentfs-Uid` and `Agentfs-Pid` (the writing process),
  `Agentfs-Trigger` (`close`, `rename`, `unlink`, ...), `Agentfs-Pod` and
  `Agentfs-Node`, and `Agentfs-Seq`, the journal position.
- `agentfs --state "$STATE" status` shows the head and the backlog (records
  not yet committed; a restore waits for them).
- The remote's web view (GitHub, Gitea) shows the same branch,
  `agentfs/<volume>`, once pushed.
- In commands below, `<commit>` is any git revision: a full or short id,
  or `HEAD~3`. `HEAD` means the volume's branch head.

## 2. Pause the workload

A restore can run with the workload live, but a workload writing the same
files at the same moment can overwrite the restore again (nothing is
lost: its writes are recorded too). Pause it first when you can:

- **Keep the mount, pause the agent.** In Kubernetes, stop the agent
  inside the pod (its own pause control, or `kubectl exec` a
  `kill -STOP <pid>` into its container) while the agentfs sidecar keeps
  running, and run `agentfs restore` in the sidecar (`kubectl exec -c
  agentfs ...`). Resume the agent afterwards (`kill -CONT`). Elsewhere,
  stop the agent process and leave `agentfs mount` running.
- **Stop everything.** Scale the workload to zero (this stops the
  sidecar too), then run `agentfs` in a one-off pod that mounts the
  volume's PVC: with no mount running it writes the backing tree directly
  (`--root`). Scale back up afterwards.

## 3. Restore or revert

Two commands, both recorded as **one commit** with
`Agentfs-Trigger: restore`, authored by you:

```sh
# A file, a symlink, a directory, or `.` for the whole tree, as it was in <commit>.
agentfs --state "$STATE" restore notes/plan.md <commit>
# Undo one commit's changes (against its first parent).
agentfs --state "$STATE" revert <commit>
```

| Flag | Meaning |
|---|---|
| `--dry-run` | Print the plan, write nothing |
| `--force` | `revert`: also revert paths changed again since `<commit>` (those later changes stay in history). Both: write files a workload holds open for writing through the mount |
| `--root DIR` | The backing tree; needed only when no mount is running |
| `--wait-lock D` | With no mount running: wait for the state-area lock |

**Restore** follows `git restore --source=<commit> -- <path>`: every file
`<commit>` has under the path is written back, and every file history has
there now that `<commit>` did not is removed. Files that were never
recorded (untracked) are never touched. **Revert** applies the inverse of
the commit's diff: files it added are removed, files it changed or
removed come back. Merge commits are refused. The commit must be on the
volume's branch; for anything else (another branch, a diverged ref) use
plain git (§5).

### Which mode runs

- **The volume is mounted** (`agentfs mount` running): the command asks
  the mount over its control socket (`$STATE/meta/control/agentfs.sock`,
  root or the daemon's user only). The daemon waits until history has
  caught up with the tree, checks, and writes through its own mount, so
  the workload sees the restored files at once and the FUSE layer records
  them. `--root` is not needed.
- **Nothing runs**: the command takes the state-area lock itself, commits
  anything journaled, writes `--root` directly and records what it wrote.
- **`agentfs run` (or `record`, `flush`, `push`) holds the state area**:
  exit status 75. `run` serves no restores: stop it (or mount instead).

### Output and exit status

```
  write notes/plan.md
  remove notes/junk.md
restored 1 path(s), removed 1: commit 4f0c2e1d...
```

`already matches: nothing to do` when the tree and history both already
are as asked. When the tree already holds what you asked for but history
does not (an earlier restore stopped before recording, or you put the
file back by hand without `agentfs record`), the path is still listed:
with no mount running it is recorded, with nothing rewritten; through a
mount it is refused as not recorded yet (pause the workload so the mount
records it, then retry).
Exit status: `0` done, `1` refused or failed (a reason and a hint on
stderr), `75` the state area is busy or the mount is running another
restore.

### Refusals

Every check runs before anything is written, so a refusal changes
nothing.

| Refusal | Why | What to do |
|---|---|---|
| `... is not a commit in the history repository` | Unknown revision | Find it with `git log` (§1) |
| `commit ... is not on the volume's branch` | Another branch, a diverged ref, a foreign commit | Plain git (§5) |
| `nothing at <path> in commit ... or in the branch head` | Wrong path | `git show --stat <commit>` |
| `commit ... changes nothing` / `is a merge` | Revert of an empty or merge commit | Restore the paths instead |
| `<path> would not be recorded: ...` | The rules (`info/exclude`, `-agentfs`, an ignore for an untracked path) or the checks (blob size limit, git fsck rules) keep it out of history; a restore never writes what history would not show | Change the rules, or restore it with plain git |
| `<path> differs from history: it holds changes that are not recorded yet` | The tree has content history does not, so writing would lose it. Never overridden, even by `--force` | Let it be recorded (pause the workload; with no mount, `agentfs --root ... record <path>`), then retry |
| `<path> is in the way: a file / a symlink / a directory with files that is not in history` | An untracked file where a restored one goes, or a symlink as a parent directory (never followed) | Move it away or record it, then retry |
| `changed again since the reverted commit: ...` | `revert` of a path a later commit changed too | `--force` reverts it anyway; the later version stays in history |
| `open for writing through the mount: ...` | The workload holds the file open | Pause it, or `--force` |
| `history is behind the tree` | Records are not being committed (see `CommitFailed` alarms) | Fix that first (`agentfs status`) |

Each path is re-checked just before it is written. If the workload
changed it, or opened it for writing through the mount, since the
checks, it is left alone and the command stops with `... since the
check, so it was left alone`: run it again to check anew. A workload
that opens and writes a file in the moment between that re-check and
the write, without closing it, loses those unrecorded writes: pause the
workload for anything it may be writing.

A failure part way records what was written, reports the error and exits
1, with `no commit` if nothing was recorded. A file is rewritten only
after the space its new content needs is allocated, so a full disk
(`ENOSPC`) or a file size limit leaves it as it was; an I/O error while
writing can leave it part written, and with no mount running the command
then records it as it is, so the tree and history agree. Either way,
once the cause is fixed, running the same command again finishes the
job: what the tree already holds is recorded, the rest written.

## 4. Undo a restore

A restore is a commit like any other. Revert it:

```sh
agentfs --state "$STATE" revert <restore commit>
```

## 5. Plain-git alternatives

For anything the commands refuse, plain git always works. With the
workload stopped (§2), write the files into the backing tree, then
record them so history follows:

```sh
# One file from any commit, even one off the branch.
git --git-dir "$STATE/git" show <commit>:notes/plan.md > "$BACKING/notes/plan.md"
agentfs --state "$STATE" --root "$BACKING" record notes/plan.md
```

For many files, export them from the commit (this only reads the state
area):

```sh
git --git-dir "$STATE/git" archive <commit> notes | tar -x -C "$BACKING"
agentfs --state "$STATE" --root "$BACKING" record notes/plan.md notes/todo.md
```

`record` takes the state-area lock, so with the volume mounted, write
through the mount instead (`cp` into the mount point): the mount records
it.

## 6. What history does and does not restore

History holds file content, the executable bit and symlinks
([§6](../design.md#6-metadata-and-permissions)). A restore writes those.

- A file rewritten in place keeps its inode, owner, other mode bits and
  extended attributes; only its exec bits follow history.
- A file created anew (it was missing, hard-linked, or another type)
  gets mode `0644` or `0755` and the owner of its directory (when the
  restoring process may chown: root).
- Ownership, other permission bits, timestamps and extended attributes
  were never recorded, so they are not restored.
- Empty directories are not recorded: directories a restore empties are
  removed (up to the restored path's parent), and directories it needs
  are created.
