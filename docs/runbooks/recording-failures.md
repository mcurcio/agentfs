# Runbook: recording failures

agentfs could not record a change, or could not turn records into
commits. These are the only failures the workload sees (design
[§3.5](../design.md#35-when-recording-fails)): every other failure, such
as a push, raises an alarm and is retried. Nothing the workload wrote is
lost from the backing tree; what may be missing is its history. The
usual cause is a state area that is full or cannot be written.

The commands below are for the phase 1 sidecar of
[deployment.md](../deployment.md), where the state area is `/data/state`
and the backing tree `/data/backing`, on one claim. Elsewhere, use your
own paths with `agentfs --state <state>`.

```sh
POD=$(kubectl get pod -l app.kubernetes.io/name=agentfs-example -o name | head -n1)
kubectl exec "$POD" -c agentfs -- agentfs status
```

## Symptoms

| What you see | Where | Section |
|---|---|---|
| `close`, `rename`, `unlink` or another call fails with `EIO` in the workload | the workload's own errors; many programs ignore errors from `close`, so the alarm is what to watch | [2](#2-a-failed-record-recordfailed-eio) |
| `alarm: record failed (<trigger>) for <paths>: <error>` | the sidecar's stderr; log event `agentfs.record_failed`; counter `agentfs.record.failures{trigger}`; alert `AgentfsRecordFailed` | [2](#2-a-failed-record-recordfailed-eio) |
| `alarm: commit failed, <n> record(s) waiting: <error>` | stderr; log event `agentfs.commit_failed`; counter `agentfs.commit.failures`; gauge `agentfs.journal.backlog` growing; alert `AgentfsCommitsFailing` | [3](#3-commits-failing-commitfailed) |
| `alarm: repack failed: <error>` | stderr; log event `agentfs.repack_failed`; counter `agentfs.repack.failures`; alert `AgentfsRepackFailed` | [1](#1-the-state-area-is-full-or-unwritable) |
| `<n> path(s) a previous mount could not record: <r> repaired from the backing tree, <s> still failing` | the sidecar's log at start | [4](#4-paths-a-mount-could-not-record-metarepairs) |
| `<n> path(s) could not be recorded ...` | the sidecar's log at stop | [4](#4-paths-a-mount-could-not-record-metarepairs) |

The `<error>` says what failed: `No space left on device` (`ENOSPC`),
`Disk quota exceeded`, `Read-only file system` (`EROFS`), `Permission
denied`, or an I/O error. The alerts are in
[deploy/alerts](../deployment.md#alerts).

## 1. The state area is full or unwritable

Check space and what uses it:

```sh
kubectl exec "$POD" -c agentfs -- df -h /data
kubectl exec "$POD" -c agentfs -- du -sh /data/state/git /data/state/journal /data/state/pending /data/state/lfs /data/state/meta
```

`agentfs.state.bytes{part}` reports the same parts (`git`, `journal`,
`pending`, `lfs`, `meta`) as apparent sizes, so a graph of it shows which
one grew. In the sidecar the backing tree shares the claim, so a workload
that fills its own files fills the state area too.

- **Grow the claim** when its StorageClass allows it
  (`allowVolumeExpansion: true`): raise `spec.resources.requests.storage`
  of the PVC. This is the fix that loses nothing.
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

Never delete files under `/data/state` by hand: history, the journal and
the frozen copies are the record.

**Unwritable** (`EROFS`, `Permission denied`): a filesystem the kernel
remounted read-only after an error needs the node's logs (`dmesg`) and
usually a new pod on a repaired volume; `Permission denied` means the
state area's owner or mode changed. The daemon runs as root and the
state area belongs to it: restore that ownership (`chown -R 0:0
/data/state` from the sidecar).

Kubernetes reports a claim's free space with the kubelet's
`kubelet_volume_stats_available_bytes`; an alert on it gives warning
before recording fails, for example:

```yaml
- alert: AgentfsClaimFilling
  expr: kubelet_volume_stats_available_bytes{persistentvolumeclaim="agentfs-data"} / kubelet_volume_stats_capacity_bytes{persistentvolumeclaim="agentfs-data"} < 0.10
  for: 15m
```

It is not in deploy/alerts, which covers agentfs's own metrics.

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
kubectl exec "$POD" -c agentfs -- git --git-dir /data/state/git log --oneline -3 agentfs/example -- <path>
```

A record whose frozen copies are gone is dropped with `RecordFailed` instead of
failing every later commit (design §19.1, "Crash findings"); record that
path again (section 4, last step).

## 3. Commits failing (CommitFailed)

Records are durable in the journal but cannot be committed: the git
objects or the branch ref cannot be written (section 1), or a ref is
locked. The records are safe and are retried, so history catches up by itself
when the cause is gone.

```sh
kubectl exec "$POD" -c agentfs -- agentfs status
# backlog:   37 record(s)      <- records waiting for a commit
kubectl logs "$POD" -c agentfs | grep 'commit failed' | tail -n 3
```

- Fix space or permissions as in section 1.
- A ref lock file a crash left (`<state>/git/refs/heads/agentfs/<volume>.lock`)
  is removed when the volume opens, so an error naming one clears with
  a restart of the pod: the journal is replayed at the next start.
- Do not downgrade agentfs while records are waiting: an older version may
  not read the journal ([upgrading-the-sidecar.md](upgrading-the-sidecar.md#rolling-back)).

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
  again through the mount, from the sidecar, keeping owner and mode:

  ```sh
  kubectl exec "$POD" -c agentfs -- sh -c 'cd /agentfs/workspace && cp -p notes/plan.md notes/plan.md.rerecord && mv notes/plan.md.rerecord notes/plan.md'
  ```

  A path that no longer exists in the backing tree is recorded as
  removed by creating and removing it through the mount. With the volume
  not mounted (the pod stopped), `agentfs --state /data/state --root
  /data/backing record <path>` records them directly.

## Verify

- `RecordFailed`, `CommitFailed` and `RepackFailed` alarms have stopped,
  and the alerts resolved.
- `agentfs status` shows the backlog draining, and `head:` moves.
- `git log` shows the paths named by the alarms.
