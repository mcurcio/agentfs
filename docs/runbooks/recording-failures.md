# Runbook: recording failures

agentfs could not record a change, or could not turn records into
commits. These are the only failures the workload sees ([guarantees](../guarantees.md)): every other failure, such
as a push, raises an alarm and is retried. Nothing the workload wrote is
lost from the backing tree; what may be missing is its history. The
usual cause is a state area that is full or cannot be written.

Sections 1 to 4 are the recording engine's, which the CSI driver runs
([csi.md](../csi.md#9-operations)): run their commands in the node plugin
pod of the node that holds the volume. Sections 5 to 7 are the tree
views', which the sidecar, the Docker examples and `agentfs mount` run
([trees](../trees.md)); there `$STATE` is the volume's state area (the
sidecar's and Docker's `/data/state`).

```sh
NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
  --field-selector spec.nodeName=<node> -o name)
STATE=/var/lib/agentfs-csi/volumes/<pv>/state
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- agentfs --state "$STATE" status
```

## Symptoms

| What you see | Where | Section |
|---|---|---|
| `close`, `rename`, `unlink` or another call fails with `EIO` in the workload | the workload's own errors; many programs ignore errors from `close`, so the alarm is what to watch | [2](#2-a-failed-record-recordfailed-eio) |
| `alarm: record failed (<trigger>) for <paths>: <error>` | the node plugin's log (`[front ...]`, `[service ...]`); log event `agentfs.record_failed`; counter `agentfs.record.failures{trigger}`; alert `AgentfsRecordFailed` | [2](#2-a-failed-record-recordfailed-eio) |
| `alarm: commit failed, <n> record(s) waiting: <error>` | stderr; log event `agentfs.commit_failed`; counter `agentfs.commit.failures`; gauge `agentfs.journal.backlog` growing; alert `AgentfsCommitsFailing` | [3](#3-commits-failing-commitfailed) |
| `alarm: repack failed: <error>` | stderr; log event `agentfs.repack_failed`; counter `agentfs.repack.failures`; alert `AgentfsRepackFailed` | [1](#1-the-state-area-is-full-or-unwritable) |
| `<n> path(s) a previous mount could not record: <r> repaired from the backing tree, <s> still failing` | a front's log at start | [4](#4-paths-a-mount-could-not-record-metarepairs) |
| `<n> path(s) could not be recorded ...` | a front's log at stop | [4](#4-paths-a-mount-could-not-record-metarepairs) |
| `journal corruption at byte <offset>`, or `<ref> names lost object <oid>`, opening a tree state area | `agentfs init` or `mount` | [5](#5-a-tree-state-area-that-refuses-to-open) |
| `tree JournalTail: <n> bytes at <offset>` | a tree actor's stderr at open | [5](#5-a-tree-state-area-that-refuses-to-open) |
| `the operator's rules refuse the volume: info/attributes line <n>: <attribute> ...` | `mount`'s error at start (exit 1) | [5](#5-a-tree-state-area-that-refuses-to-open) |
| `tree HistoryBuildFailed: <error>`, or a tree view's metrics with `history.health` `failed` or an `oldest_unbuilt_ns` that keeps growing | a tree actor's stderr; `--metrics-output` | [6](#6-a-tree-volumes-history-is-not-built) |
| `agentfs mount` exits 75 or 1 at start, or does not exit after SIGTERM | the mount's stderr | [7](#7-a-tree-mount-that-will-not-start-or-stop) |

The `<error>` says what failed: `No space left on device` (`ENOSPC`),
`Disk quota exceeded`, `Read-only file system` (`EROFS`), `Permission
denied`, or an I/O error. The alerts are in
[deploy/alerts](../deployment.md#alerts).

## 1. The state area is full or unwritable

Check space and what uses it:

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- df -h "$STATE"
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- du -sh "$STATE"/git "$STATE"/journal "$STATE"/pending "$STATE"/lfs "$STATE"/meta
```

`agentfs.state.bytes{part}` reports the same parts (`git`, `journal`,
`pending`, `lfs`, `meta`) as apparent sizes, so a graph of it shows which
one grew. The backing tree and the state area share the node's
`basePath` disk ([csi.md](../csi.md#11-where-worktrees-live)), so a
workload that fills its own files fills the state area too.

- **Grow the disk under `basePath`.** This is the fix that loses
  nothing; the driver enforces no capacity.
- **`lfs` is large:** objects that every remote already has can be
  removed with `agentfs lfs prune` (try `--dry-run` first,
  [lfs-storage.md](lfs-storage.md)).
- **`pending` or `journal` is large:** these are records not yet
  committed. They shrink once commits work again (section 3). Never delete
  them: they are the only copy of history not yet committed.
- **`git` is large:** loose objects shrink at the next repack
  (`agentfs.repackInterval`, default `1h`); a repack also needs free space
  for the new pack. Large files belong in LFS from now on
  (`agentfs.lfsThreshold`, [configuration](../configuration.md#git-lfs)).
- **Free space in the backing tree** (the workload's own files) if they
  are what filled the claim.

Never delete files under the state area by hand: history, the journal and
the frozen copies are the record.

**Unwritable** (`EROFS`, `Permission denied`): a filesystem the kernel
remounted read-only after an error needs the node's logs (`dmesg`) and
usually a repaired disk; `Permission denied` means the state area's
owner or mode changed. The node plugin runs as root and the state area
belongs to it: restore that ownership (`chown -R 0:0 "$STATE"` in the
node plugin pod).

## 2. A failed record (RecordFailed, EIO)

The synchronous step failed, so the call returned an error and the change
is not in history yet:

- a file whose content failed to record stays marked as changed: its next
  close records it;
- a rename, unlink, rmdir, link, symlink or setattr had already changed
  the backing tree: it returned `EIO` and its paths went on the repair
  queue;
- a sweep (every `agentfs.sweepInterval`, default `30s`) retries both.

So once the cause is fixed (section 1), recording catches up by itself
within a sweep. To check, the paths are in the alarm:

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- git --git-dir "$STATE"/git log --oneline -3 agentfs/<volume> -- <path>
```

A record whose frozen copies are gone is dropped with `RecordFailed` instead of
failing every later commit; record that
path again (section 4, last step).

## 3. Commits failing (CommitFailed)

Records are durable in the journal but cannot be committed: the git
objects or the branch ref cannot be written (section 1), or a ref is
locked. The records are safe and are retried, so history catches up by itself
when the cause is gone.

```sh
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- agentfs --state "$STATE" status
# backlog:   37 record(s)      <- records waiting for a commit
kubectl -n agentfs-csi logs "$NODE_POD" -c agentfs-csi | grep 'commit failed' | tail -n 3
```

- Fix space or permissions as in section 1.
- A ref lock file a crash left (`<state>/git/refs/heads/agentfs/<volume>.lock`)
  is removed when the volume opens, so an error naming one clears with
  a restart of the history service ([csi-recovery.md](csi-recovery.md)):
  the journal is replayed at the next start.
- Do not downgrade agentfs while records are waiting: an older version may
  not read the journal.

It is fixed when `agentfs status` shows `backlog:   0 record(s)` (or a
small number that keeps changing), `agentfs.commit.count` rises, and the
alert resolves.

## 4. Paths a mount could not record (meta/repairs)

Paths still on the repair queue when the daemon stops are saved in
`<state>/meta/repairs` (a front's in `meta/fronts/slot-N.repairs`), and
the next mount records each from the backing tree before it serves
anything. Its start shows the result:

```
3 path(s) a previous mount could not record: 3 repaired from the backing tree, 0 still failing
```

- **All repaired:** nothing to do.
- **Some still failing:** the cause is still there. They stay queued and
  the sweep retries them; fix section 1 and watch for `RecordFailed` to
  stop.
- **Not saved:** at the stop the log said `<n> path(s) could not be
  recorded, nor saved for the next mount ...: <paths>` (the state area
  could not take the file either). History differs from the backing tree
  at those paths until each changes again. Record them by writing each
  again through the mount, from the workload's pod, keeping owner and mode:

  ```sh
  kubectl exec <pod> -- sh -c 'cd <mount path> && cp -p notes/plan.md notes/plan.md.rerecord && mv notes/plan.md.rerecord notes/plan.md'
  ```

  A path that no longer exists in the backing tree is recorded as
  removed by creating and removing it through the mount. With the volume
  not staged (no pod uses it), `agentfs --state "$STATE" --root
  <basePath>/<pv>/backing record <path>` in the node plugin pod records
  them directly.

## 5. A tree state area that refuses to open

This section is for the tree actor ([trees](../trees.md)),
whose journal is the segment files in `journal/` and whose refs are a copy of it.
Recovery is manual:
agentfs repairs neither case itself.

- **`CorruptJournal(offset)`.** A frame is bad and a later good frame's durable mark
  says a record at or after the bad one had been flushed to disk, so this is not a torn
  write: the disk lost data it had acknowledged. The offset counts the bytes of the
  segments in name order. It also means the valid records end before the point
  `built.log` names: the builder made those records durable before it built them,
  so the disk lost them (the offset is where the records end). Truncating
  `built.log` instead would move refs back past saves the views served.
  `CorruptJournal(0)` also means the journal is missing, holds no frame or holds
  no checkpoint.
- **`MissingObject`.** A pointer's commit, tree or blob is gone. The error names the
  pointer and the object.

Do this before anything else:

```sh
cp -a "$STATE" "$STATE.saved"
git --git-dir="$STATE/history.git" for-each-ref > "$STATE.saved.refs"
```

Those refs are the last copy the actor made, and they show what the journal said.
Truncating the segment that holds the offset there, and removing every later
segment, loses every later record: those saves stop existing, and the same refs
show what they were. Recovery rebuilds the refs from the
journal on open, so a truncation overwrites any newer copy.

A `tree JournalTail` line with a `journal/torn` file means frames no flush had
covered were dropped at open: calls made since the last `close`, `fsync` or FSYNCDIR
before a power cut. This is the expected result of a cut, and the actor keeps going. It
can also be bit rot in records not yet flushed, which looks the same: the log line and
the file are the evidence. The file keeps the bytes of every tear, appended.

A `tree JournalFsyncFailed` line means a flush of the journal (a segment, or the
directory after a new segment) failed and the actor stopped: every later call
answers `EIO`. Fix the disk or space problem, then restart; recovery reads every
frame still in the segments.

An error `the operator's rules refuse the volume: info/attributes line <n>:
<attribute> is not built for tree volumes` means the operator's rules hold a rule tree volumes do not build: any
`agentfs.<name>` attribute, `filter=lfs`, or `agentfs=<value>`
([trees](../trees.md#rules-and-the-untracked-area)). The volume was refused before
anything was written, so its state is as it was. Edit
`$STATE/history.git/info/attributes`: remove the attribute from that line, or use
`-agentfs` to keep the path out of history, then mount again. An unreadable
`info/exclude` or `info/attributes` fails the open the same way with its I/O error.

A `tree BaseFile` line means the builder could not open a checked-out file to read
a map from (often `Too many open files`): the file keeps its journal frames, which
then keep their segments' space. Raise the process's hard limit on open files.

## 6. A tree volume's history is not built

A tree view records every call in its journal before the call returns, and a
builder thread makes the commits behind it ([trees](../trees.md#history-trails-the-files)).
When a batch fails, recording goes on and nothing reachable changes: the batch is
retried at the next step or wait. Takes, `settle` and anything reading history wait
until it builds or answer the failure. A volume whose builder fails still opens and
serves: a restart never waits for history, so a failing build shows as this alarm,
never as a volume that will not mount.

- **`no space`, `errno <n>`.** The state area is full or failing: free space or fix
  the disk ([1](#1-the-state-area-is-full-or-unwritable)). The next step retries. A
  builder that never recovers leaves every journal record in `journal/`, so the
  state area fills and the workload's calls fail with `ENOSPC`.
- **`git fast-import failed: <first line>`.** The packer exited or was killed. Its
  first stderr line says why; the next step retries with a new child.
- **`histogram diff: ...` or `git diff failed: ...`.** The alignment of a save
  failed. A save that is not an append runs one `git diff`, which reads both
  versions of the file whole: an edit of a file near the host's memory is
  killed there, and every retry fails the same way. Give the volume's
  container the memory (about twice the file), or replace the file with a new
  one, which aligns with no `git diff`. Its two files are in `align/` in the
  state area while it runs and are removed after.
- **`disagrees with step <n>`.** The builder's replay of step `n` does not give what
  the actor recorded (a length, a digest of the file's pieces, or a name). Every
  retry fails the same way. Preserve the state area (`cp -a "$STATE" "$STATE.saved"`)
  and report it with `journal/` and `built.log`: it is a bug, and no history is built past step `n`
  until a release fixes it.
- **A wedged builder**: `oldest_unbuilt_ns` grows while `health` stays `current`.
  Waiters wait without a timeout. Find the `git fast-import` child of the agentfs
  process (`pgrep -P <pid> -a git`) and kill it: the batch fails, waiters get the
  error, and the next step retries.

What `fast-import` left behind is cleared at the next open: `objects/pack/tmp_*`,
`.keep` files, an index without its pack, and `refs/build/*`. An unreferenced pack
from a batch whose built-log entry was never written stays; nothing names it.

## 7. A tree mount that will not start or stop

`agentfs mount` stages the volume, attaches a client for each view and drains the
volume when it stops ([lifecycle](../trees.md#volume-lifecycle)). Each move is a
commit on `refs/agentfs/volume` or `refs/agentfs/clients/<tree>`, so the record
says where it is:

```sh
git --git-dir="$STATE/history.git" log --format='%s' refs/agentfs/volume
git --git-dir="$STATE/history.git" show refs/agentfs/clients/root:row.json
```

These chains are built behind the journal, so they lag while the builder lags or
fails ([6](#6-a-tree-volumes-history-is-not-built)).

**It will not start.**

| Exit and message | Meaning and fix |
|---|---|
| 75, `the volume is busy: another process holds its state area` | Another `mount`, `init` or `fork end` holds the state area's lock. Stop it, or wait for its drain to finish |
| 1, ``no volume here: run `agentfs init --volume NAME` first`` | `mount` never creates a volume: run `init` first |
| 1, `the volume here is "<a>", not "<b>"` or `another volume is here` | The state area belongs to another volume; nothing was written. Point the launcher at the right state area, or fix the name |
| 1, `this fork has had its writer` | The fork's one read-write client has been used (C4 in the [library](../library/core.md#domain-invariants)). Mount it with `--fork-read-only`, or use a new key. A writer killed rather than unmounted comes back with the same launcher, tree, label and access |
| 1, `the fork under this --fork-key has ended` | `fork end` ended it. Use a new key |

**It will not stop.** After SIGTERM or SIGINT the process exits 0 only once the
volume is released. It waits while:

- **a client is attached.** A view whose file a process still holds open ends
  when that file closes. Close it, or stop the process that holds it. A
  `tree ReconcileFailed` line means a client's end could not be recorded (the
  state area's disk): the drain asks again at the stop, and the next mount's
  reconcile ends it. Fix the disk, or let the grace period end the process.
- **a queue holds an entry.** An entry is taken only once history is built
  behind it, so a failing builder holds the release: the volume stays
  `Draining`, recording goes on, and the builder's alarm says why
  ([6](#6-a-tree-volumes-history-is-not-built)). Fix the cause, or let the
  grace period end the process: nothing recorded is lost, and the next mount
  builds it.

An exit of 1 after a recording failure means the tree actor stopped
([5](#5-a-tree-state-area-that-refuses-to-open)): restart once the disk is fixed.

Log lines that change nothing: `tree UnstageFailed` (the volume stays staged;
the next stop asks again), `tree DetachFailed: client=<id>` (the next mount's
reconcile ends that client), `tree SettleFailed` (a fork's end or drop waits for
the next judgement, at the latest the next mount's) and `tree EventFailed`.

## Verify

- `RecordFailed`, `CommitFailed` and `RepackFailed` alarms have stopped,
  and the alerts resolved.
- `agentfs status` shows the backlog draining, and `head:` moves.
- `git log` shows the paths named by the alarms.
