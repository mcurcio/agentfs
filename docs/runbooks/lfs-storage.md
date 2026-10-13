# Runbook: LFS storage

Large files stored as Git LFS objects take space in three places: the
volume's local object cache in the state area, partial downloads beside
it, and every remote's LFS server. This runbook covers watching that
space, freeing the local cache safely, and getting objects back. Design:
[§7](../design.md#7-large-files-and-lfs-phase-2); settings:
[Git LFS](../configuration.md#git-lfs).

## Symptoms

- `agentfs.state.bytes{part="lfs"}` grows, or the state volume fills up
  (`df` on the state area's filesystem). Every LFS object ever committed is
  in the cache until it is pruned.
- An LFS server or host quota is reached: pushes fail with stage `lfs`
  (`refused`, often `413` or `422`); see
  [stuck-pushes.md](stuck-pushes.md#lfs-stage-failures).

## Where LFS bytes live

| Place | What | Freed by |
|---|---|---|
| `<state>/lfs/objects/aa/bb/<oid>` | Every object committed or downloaded | `agentfs lfs prune`, for objects every remote holds |
| `<state>/lfs/incomplete/<oid>` | Downloads that were interrupted, kept to resume | finishing the download, or `agentfs lfs prune` once nothing holds them |
| `<state>/lfs/tmp/` | Objects being written | cleared at every mount |
| Each remote's LFS server | Every object pushed | nothing in agentfs: see [Objects on servers](#objects-on-servers) |

```sh
du -sh "$STATE/lfs/objects" "$STATE/lfs/incomplete"
```

## 1. Check what the servers hold

A prune deletes a cached object only when **every** valid remote's LFS
endpoints (upload, and download when it is another endpoint) confirm
they hold it. Ask first, deleting nothing (safe while the volume is
mounted):

<!-- ci: lfs-dry-run -->
```sh
agentfs --state "$STATE" lfs prune --dry-run
```

It prints each endpoint and how many cached objects it confirmed, the
objects some remote lacks (`kept N object(s) ... (origin lacks 3)`), and
`dry run: would prune N object(s), B byte(s)`. A remote that lacks
objects is usually one whose pushes are failing or that was added
recently: push it first ([stuck-pushes.md](stuck-pushes.md)).

The prune refuses, deleting nothing, when a remote is misconfigured
(`RemoteMisconfigured`) or has no LFS endpoint agentfs can reach (an SSH
remote without `lfsurl` on a server without `git-lfs-authenticate`, an
`https` endpoint in a build without the `https` feature): nothing could
be confirmed there. Fix or remove that remote. With no remote at all,
nothing is pruned.

## 2. Prune

<!-- ci: lfs-prune -->
```sh
agentfs --state "$STATE" lfs prune
```

- Only an operator runs it; agentfs never prunes on its own, and never
  deletes anything on a server.
- It can run while the volume is mounted. Restores, reverts, patch applies
  and `agentfs lfs fetch` hold the cache lock (`<state>/lfs/lock`) while
  they use the cache; the prune takes it only for its deletions, waits
  `--wait-lock` (default: not at all) and exits 75, deleting nothing, while
  one is running. Retry, or `--wait-lock 5m`.
- `--older-than 24h` keeps objects cached in the last day, which a
  restore of recent history is likely to need.
- Unfinished downloads in `lfs/incomplete/` that nothing is resuming are
  deleted too.

Pruning trusts each server's answer: an object a server claims to have
but has lost can then only come from another remote. If you doubt a
server, prune with that remote's objects verified first (for example
`git lfs fsck` in a clone).

## 3. Get objects back

Restores and reverts download what they need from the remotes, in name
order, before writing anything, so a pruned object costs only a download.
To fill the cache ahead of time (before cutting the network, before a
large restore):

<!-- ci: lfs-fetch-head -->
```sh
agentfs --state "$STATE" lfs fetch
```

This fetches what the branch head references; `agentfs lfs fetch <rev>`
what another commit references, and `--all` every object of every commit
on the branch. `--remote NAME` asks only that remote; `--dry-run` counts
what is missing without contacting a server. It takes no state-area lock,
so it runs beside a mount. Exit 1 means some object could not be fetched:
the error names it and what each remote said.

## 4. A new remote, or one that fell behind, after a prune

Pushing commits uploads their LFS objects first, from the cache. An
object the new remote's server lacks and the cache no longer has fails
the push:

```text
newremote: failed (local, stage lfs): 2 LFS object(s) that remote newremote lacks are missing from the local cache (<state>/lfs/objects), so they cannot be uploaded: big.bin (<oid>), ...; run `agentfs lfs fetch --all` to bring them back from another remote
```

Fetch them back from the remotes that have them, then push:

<!-- ci: lfs-new-remote -->
```sh
agentfs --state "$STATE" lfs fetch --all
agentfs --state "$STATE" push --remote "$NEW_REMOTE"
```

Objects the new server already has need no cached copy.

## 5. Interrupted downloads

A download that stops (a timeout, a restart, a killed `agentfs`) keeps
what arrived in `<state>/lfs/incomplete/<oid>`, and the next download of
that object asks the server for the rest (an HTTP `Range` request) and
checks the whole object's SHA-256 at the end. Nothing to do: re-run the
restore or `agentfs lfs fetch`. A server that ignores `Range` sends the
whole object again. Partials nobody resumes are deleted by the next
prune.

## 6. Server quotas and per-object limits

LFS hosts cap the size of one object (GitHub: 2 GiB, up to 5 GiB on some
plans) and often the total storage. Tell agentfs a remote's object cap so
files over it are refused when they are written, instead of blocking that
remote's pushes later:

```sh
git --git-dir "$STATE/git" config remote.origin.agentfsMaxLfsObjectSize 2g
```

The smallest cap across remotes applies at record time (`PathRefused`,
reason `lfs_object_too_large`); a push to a remote checks its own cap on
the objects it would upload and fails as `too_large` at stage `lfs`
([stuck-pushes.md](stuck-pushes.md#lfs-stage-failures)). Total storage on
a server only grows: see [Objects on servers](#objects-on-servers).

## 7. Limiting growth

- Store fewer, smaller objects: `agentfs.lfsThreshold`, operator
  `filter=lfs` and `agentfs.store` rules decide what goes to LFS; rules
  that exclude build outputs keep them out of history entirely
  ([path rules](../configuration.md#operator-attributes)).
- Files rewritten in place (databases) are captured at most once per
  `agentfs.coalesce` interval; a longer interval stores fewer versions.
- `agentfs.maxSize` refuses files over a size, whatever the store.

## Objects on servers

LFS objects cannot be deleted from most servers without deleting the
repository (GitHub), and deleting them breaks every clone of a commit
that references them. agentfs never does. To remove content from history
and then from a server, follow [history-rewrite.md](history-rewrite.md),
then use the host's own tools to purge unreferenced objects.

## In Kubernetes

Run the commands in the container that runs agentfs for the volume:

```sh
# The sidecar (deploy/sidecar): AGENTFS_STATE is set in the container.
kubectl -n "$NS" exec "$POD" -c agentfs -- agentfs lfs prune --dry-run
# The CSI driver: in the node plugin of the volume's node, with the
# volume's state area under the driver's --base-path (see csi.md and
# csi-recovery.md for finding $NODE_POD and $PV).
kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
  agentfs --state "/var/lib/agentfs-csi/volumes/$PV/state" lfs prune --dry-run
```
