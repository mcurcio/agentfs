# Patches

Change a volume's files from outside it: queue a patch, and the next mount
applies it before the workload sees the tree. Producers are iac, an
operator or CI. This page is what the patch queue does and what it
promises. The procedures for when it blocks are in the runbook
[the patch queue](runbooks/patch-queue.md); every key is in the
[configuration reference](configuration.md).

```sh
# In the producer's git repository (its remote and credentials are used):
agentfs patch submit --volume demo --put config/app.yaml=./app.yaml -m "Raise the limit"
agentfs patch submit --volume demo HEAD          # a commit whose parent is on agentfs/demo

# On the volume:
agentfs --state /data/state status               # patches:   last applied 3, 0 queued, ok (from origin)
agentfs --state /data/state patch status         # the queue, a block and why
agentfs --state /data/state --root /data/backing patch apply   # with the volume stopped
```

## What a patch is

A patch is a branch: a line of one or more commits, each with exactly one
parent, on a **base**. The base is the merge base of the patch's tip and
the volume's branch head, so it is the head or any ancestor of it. A
one-commit patch is a branch of one, whose base is its parent.

The producer pushes the tip to `refs/agentfs/<volume>/inbox/<n>` on the
volume's patch remote. `<n>` is ten zero-padded digits from `0000000001`.
Any other name under `inbox/` is ignored and reported. A patch may change
at most 4096 paths from base to tip, and only regular files and symlinks.

## Submitting

`agentfs patch submit` runs in the producer's own git repository and
drives the producer's `git`, so its remote, credential helpers and SSH
setup apply. Plain git works too. The command:

1. lists the branch, `inbox/*` and `applied/*` on the remote;
2. fetches the head (`--depth` for shallow producers);
3. takes either a given tip whose commits since the merge base are a line
   of single-parent commits (a merge commit is refused), or builds one
   commit on the remote head from `--put PATH=FILE` and `--delete PATH`,
   with `-m` and `--author`. `--barrier` marks a built patch with the
   trailer `Agentfs-Barrier: yes`, and a given tip must carry it;
4. takes the number one past the highest inbox or applied number listed;
5. pushes with `git push --force-with-lease=<ref>: <oid>:<ref>`, which
   creates the ref only if it does not exist. A submitter that loses a
   race lists again and tries the next number (`--attempts`, default 10).

A commit that is already queued or applied is refused before anything is
pushed. Two submits at once can both pass that check, and the duplicate
rule below then guarantees the change lands once.

To fix a queued patch, `--replace N` swaps it (leased on the oid listed)
and `--skip N` replaces it with an empty commit on the head, so the queue
moves on. Both also fill a **gap**: a number below the highest listed that
the remote has neither as `inbox/N` nor as `applied/N`. Both refuse a
number the volume has already applied. Applied refs reach the remote
before inbox refs are deleted there, so the highest number listed never
goes down and a number is never reused.

## When it runs

Patches land only at a start with nothing attached. They never land while
a workload may be using the volume (see
[guarantees](guarantees.md)).

- **At mount**, after journal replay and before the service and the FUSE
  layer start. `agentfs run` does not run the queue.
- **`agentfs patch apply`** with the volume stopped does the same. It
  takes the operator's word that nothing is attached.
- **While a daemon holds the volume**, `agentfs patch apply` asks it over
  the control socket to judge the queue. Nothing lands, and the command
  answers "workload attached; lands at next mount" (exit 75). With no
  daemon it takes the state lock and runs the queue (exit 1 when blocked
  or a patch is set aside, 75 when the lock is held).

A start lands the queue only on evidence that no workload is attached.
Otherwise it judges the queue without writing the tree, says why, raises
`PatchQueueDeferred` when anything is queued, and leaves the patches for
the next start with nothing attached. The evidence is:

- **A single-process mount** writes `meta/attached.json` before the mount
  can be seen. It removes it only after a stop whose unmount was not busy,
  whose session ended within `--unmount-grace` (no file still open, and
  the filesystem not still mounted elsewhere, such as a kubelet `subPath`
  bind) and with no stale layer below it. A start that finds the mark
  defers (`unclean_stop`): the last mount was killed or crashed, or a
  workload held it as it stopped.
  - One exception is a mark another pod wrote. When the mount point lies
    in the pod's own volume, the mark records that pod's uid
    (`AGENTFS_POD_UID`). A start in a different pod, whose mount point is
    in its own volume, lands the queue over it: nothing attached to the old
    pod's mount can reach the new pod's volume.
  - A mark from the same pod (agentfs restarted alone), a mark without a
    uid, or a start that is not pod-scoped (Docker, the CLI, a hostPath or
    a memory-backed `emptyDir`) defers.
  - A mount stacked over a stale one (`--on-stale-mount recover`) defers
    too (`stale_mount`).
  - A state area never mounted has no mark.
- **A history service** defers while a front holds a lease
  (`fronts_running`), and when started with `--workload-attached`
  (`workload_attached`). The CSI node plugin passes that whenever a pod of
  the volume is published on the node, so a plugin restart under running
  pods never lands the queue.

A run goes in this order:

1. commit what is journaled, so the head is the tree's state;
2. fetch `inbox/*` from the patch remote, best effort, within
   `agentfs.patchFetchTimeout` (default 30s; `0` or `off` skips it). A
   failure raises `PatchFetchFailed` and the queue runs with what is
   already fetched. Local inbox refs the remote no longer has are pruned;
3. scan the branch for applied patches;
4. apply the queue in order, from the last applied number plus one;
5. move the refs and write the status.

## Applying a patch

- **Merge.** The `git` program merges, chosen by `agentfs.patchIntegrate`.
  `merge` (the default) is `git merge-tree --write-tree`: the patch's
  whole diff, a three-way merge against its base. `rebase` replays the
  patch's commits one by one and holds the patch as a `conflict` at the
  first that conflicts. The merge runs in the state area's git directory
  with `core.fsync=objects`, and only the operator's
  `$GIT_DIR/info/attributes` steers it: a `merge=<name>` attribute picks a
  built-in driver or a `merge.<name>.driver` of the repository's config,
  such as `mergiraf`, which the image ships. A driver that cannot run
  leaves the patch **not judged** (rule `merge`), never a conflict.
- **Write.** The merged tree is written into the backing tree like a
  restore: the paths whose head and merged entries differ are planned and
  checked (an unrecorded change or something in the way refuses), then
  written, with LFS content from the cache or the remotes.
- **Record.** The write is one atomic record of kind `patch`, so it
  becomes one commit, never grouped with workload changes, whatever the
  patch's length. Its author is the tip's author and its committer is the
  replica's identity. It carries `Agentfs-Trigger: patch`,
  `Agentfs-Patch: <tip>` and `Agentfs-Inbox: <n>`. Its message is the
  tip's, followed for a branch of several commits by one line per commit.
- **A patch that changes nothing** gets a marker commit with the head's
  tree and the same trailers.
- **A duplicate** (the same patch commit queued under two numbers) is
  consumed by a marker commit with the head's tree, carrying
  `Agentfs-Inbox: <n>` and the subject `duplicate of patch <m>: <summary>`.
  `PatchDuplicate` is raised. Nothing is merged or written, so a change
  the workload reverted since stays reverted.
- **Refused at record or commit time**, the patch's writes are put back
  and the queue blocks. A write that fails part way is recorded as it
  stands, as an operation without an inbox number, so the tree matches
  history again.

## A patch that cannot land is set aside whole

Nothing of a patch is ever written in part. A patch that cannot land stays
queued with a verdict, every run judges it again, and the queue goes on.
In order, a patch is:

- **held** for a reason below marked held (`PatchHeld` is raised);
- **waiting** behind an earlier set-aside patch that is a barrier, or that
  changes one of its paths;
- after a clean merge, judged by the **acceptance rule**,
  `agentfs.patchAccept`. `cleanMerge` (the default) lands it. `approval`
  lands it once `refs/agentfs/<volume>/approved/<n>` on the patch remote
  names its tip (`agentfs patch approve`), else it is **awaiting**.
  `command` runs `$GIT_DIR/hooks/patch-accept`: exit 0 lands, 1 holds it as
  `refused`, 75 leaves it awaiting. A rule that cannot judge leaves it
  **not judged**, raises `PatchRuleFailed` and `PatchHeld`, and never
  lands it.

A run **stops** only at a `gap`, `unrecorded` changes or a failure. That
patch and the later ones stay queued, unjudged, `PatchBlocked` is raised,
and the volume mounts anyway.

| Reason | When | Effect |
|---|---|---|
| `conflict` | The merge conflicts; the paths are listed | held |
| `gap` | The next number is missing while later ones are queued | stops |
| `parent_not_in_history` | The parent is not the head or one of its ancestors | held |
| `merge_commit` | More than one parent | held |
| `unsupported` | A root commit, a gitlink, a path agentfs does not record | held |
| `too_large` | More than 4096 paths | held |
| `refused` | The acceptance rule refused it | held |
| `unrecorded` | The tree differs from history where the patch writes (an unrecorded change, a pending repair, something in the way) | stops |
| `failed` | Writing or recording failed, or an LFS object is missing | stops |

The producer fixes a held patch by replacing it (`--replace N`, rebased
onto the head) or skipping it (`--skip N`). An `unrecorded` block is the
operator's: record the paths named (`agentfs record`), or finish the
repair or restore that left them. The next mount, or `agentfs patch
apply`, carries on.

A queue that cannot run at all (the history or state area cannot be read,
the tree or LFS cache cannot be opened, a ref cannot be written) raises
`PatchBlocked` with reason `failed` and inbox 0. `agentfs.patch.blocked`
and `agentfs status` show it, and the volume mounts anyway.

**Held marks.** Before it judges a number, the queue marks a set-aside
number `n` with the local ref `refs/agentfs/<volume>/held/<n>`, never
pushed. The mark keeps `inbox/<n>` from being treated as stale, even once
later numbers land. It is deleted when `n` lands or when its inbox entry
is gone (`PatchIgnored`, `withdrawn`).

## One commit per patch, after any crash

Whether patch `<n>` is applied is decided by the branch: the newest
first-parent commit whose last message paragraph carries
`Agentfs-Trigger: patch`, `Agentfs-Patch` and `Agentfs-Inbox: <n>`.
`meta/patch.json` caches the scan and is never authoritative. Refs move
only after the commit, so every crash converges on one commit per patch:

| Crash after | Next run |
|---|---|
| the tree write, before the record | the check finds the content already there and records it without writing |
| the tree write, before the record, and the producer then replaced, skipped or withdrew the patch | the patch being written is noted in `meta/patch.json` (`in_flight`) before the write; the run puts those paths back to the head's state, or records them as they stand without an inbox number, before applying anything |
| the record, before the commit | journal replay commits it; the scan finds it |
| the commit, before the ref moves | the scan finds it; the refs are moved |

An inbox entry at or below the last applied number is bookkeeping when it
names the applied patch. Otherwise, unless it has a held mark, it is
`stale`: reported under `ignored`, raised as the warning `PatchIgnored`,
never applied, and deleted on the remote by the next queue push.

## Refs, status and the patch remote

- **Refs.** `applied/<n>` is the patch commit itself. `inbox/<n>` is
  deleted locally once `applied/<n>` is written, and on the remote by the
  queue push once `applied/<n>` is there (leased on the oid listed). See
  [remotes](remotes.md).
- **Status.** `refs/agentfs/<volume>/status` is a parentless commit holding
  `status.json`, rewritten at every run and force-pushed: a snapshot, not
  history. It lists the volume, branch and head, `last_applied`,
  `applied_now`, `duplicate_now`, the `queue` (each set-aside patch with its
  `verdict`: `held`, `waiting`, `awaiting` or `not_judged`), `blocked`,
  `ignored` and `fetch`. `agentfs status` prints a `patches:` line, and
  `agentfs patch status [--json]` the whole status.
- **The patch remote** is `agentfs.patchRemote`: a remote's name, or
  `none`. By default it is the only remote when exactly one is configured,
  else none: no fetch and no queue push, while patches already in the local
  inbox still apply. A name that matches no valid remote also means none.

## Judging, notifying and approving

A daemon that holds the volume only **judges**: it fetches the inbox
(unless `--no-fetch`), runs the per-patch rules read only, and writes the
status. A patch that would land gets the verdict `ready`, which is
advisory, because it was judged against the head as it was.

After every run and every judging, the verdicts are compared with those of
the last delivered notice. When any changed, one notice goes to each
configured method:

- the hook `$GIT_DIR/hooks/patch-notify`, when it exists (run without a
  shell, the notice on stdin);
- the webhook `agentfs.patchNotifyUrl` (one POST with an
  `Idempotency-Key` of `<volume>:<status commit>`, and a bearer token from
  `agentfs.patchNotifyCredentialFile`).

Both are bounded by `agentfs.patchNotifyTimeout`. A failure raises
`NotifyFailed` and the next queue event sends the same changes again:
at least once, with no timer and no retry loop. Patches a run landed whose
notice failed are kept and sent with every notice until one is delivered.
A receiver dedupes on volume, inbox, patch and verdict.

## Serving the queue over git

`agentfs serve-git` makes the volume's history repository a patch source:
a git server over smart HTTP (`git http-backend`) for any producer that
speaks git. It refuses to start while `agentfs.patchRemote` names a
remote. Every request authenticates as a pusher of
`[agentfsPusher "<name>"]` ([configuration](configuration.md)), whose
rights (`offer`, `approve`, `barrier`, `integrator`) say what it may
write. Replacing a barrier patch needs `barrier` too.

Only the branch and the queue's refs are listed to clients. That hides
names, not objects. A push is **admitted before git keeps it**: the
`pre-receive` hook refuses the whole push unless every update is allowed
and every offered tip reads as a patch against the head. On arrival the
`post-receive` hook judges the queue and notifies, and never lands.
`POST /agentfs/apply` (right `integrator`) is `agentfs patch apply`.

## Checkpoints

`agentfs checkpoint` cuts one commit per intent on
`refs/agentfs/<volume>/change-line`: the head's tree, with the previous
checkpoint and the tips of the patches landed since as parents. A marker
commit merges it back into the branch, so the branch's first-parent history
and the queue's trailer scan are unchanged. A remote with
`agentfsPush = changeLine` receives the change line instead of the branch.

## Forks of forks

A linked fork's patch remote is its fork remote, so it can itself be
forked. A fork of a fork rolls its change up one level: its finisher pushes
its history to the fork remote, lists `refs/agentfs/<parent>/` there, and
queues its change create-only as the parent's next inbox number. A change
the parent already applied, or one the parent queued while it has not
finished, is not queued again. The parent's queue lands the change like any
patch at its next mount with nothing attached, and the parent's own change
then carries it up.

- A parent that has finished cannot take it, so the child publishes as a
  top-level fork and raises `ForkParentFinished`. It may be raised twice
  after a crash and is never lost.
- Finishing never runs the queue, so a child's change that arrives after
  the parent's last mount is not in the parent's change. Each fork whose
  `finish` ref is held lists its own inbox and raises `ForkInboxNotCarried`
  for each entry its history did not land. The entry stays queued on the
  remote and nothing lands it.
- A fork's applied numbers are its own: it never inherits its source's. A
  fork whose base is missing from its history is corrupt, and counting its
  inbox numbers fails (`ForkBaseMissing`).

See [forks](forks.md) and the runbook section on
[forks of forks](runbooks/patch-queue.md#9-forks-of-forks).

## Telemetry

Gauges `agentfs.patch.queue_depth`, `agentfs.patch.blocked` and
`agentfs.patch.held`; an `agentfs.patch` span per patch; alarms
`PatchBlocked`, `PatchHeld`, `PatchRuleFailed`, `PatchFetchFailed` and
`NotifyFailed`; warnings `PatchIgnored` and `PatchQueueDeferred`; events
`PatchApplied` and `PatchDuplicate`. See [telemetry](telemetry.md).

## Limits

- Patches do not land while a workload runs. A queue held at a start with
  something attached waits for the next start with nothing attached.
- A patch lands whole or not at all. A clean merge is not assumed to be an
  acceptable change: use `agentfs.patchAccept` to decide.
- Patch commits must be single-parent lines. Merge commits, root commits
  and gitlinks are held.
- `agentfs serve-git` hides ref names from clients but not objects.
