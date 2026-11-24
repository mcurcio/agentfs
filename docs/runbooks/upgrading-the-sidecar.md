# Runbook: upgrading the sidecar

Moving a workload's agentfs sidecar ([deployment.md](../deployment.md),
[`deploy/sidecar`](../../deploy/sidecar)) to another image, and back, and
moving a volume of the recording engine to the tree engine the sidecar
now runs. An upgrade is a pod replacement: the old sidecar stops after the
workload and drains the volume, then the new pod mounts it. The CSI driver
is upgraded differently ([csi.md](../csi.md)).

```sh
POD=$(kubectl get pod -l app.kubernetes.io/name=agentfs-example -o name | head -n1)
```

## Before

1. **Pick the image by commit.** Tags are 40-character commit ids of
   `main` (and `latest`, which moves): use `ghcr.io/mcurcio/agentfs:<sha>`
   or a digest, never `latest`
   ([deployment.md](../deployment.md#1-the-image)). The commits between
   the running one and the new one (`git log --oneline <old>..<new>` in a
   clone of agentfs) say what changed, and [CHANGELOG.md](../../CHANGELOG.md) lists the
   releases.
2. **Note where the volume is:**

   ```sh
   kubectl exec "$POD" -c agentfs -- agentfs --version
   kubectl exec "$POD" -c agentfs -- git --git-dir /data/state/history.git log -1 --format=%H root
   ```

   Keep the commit. A volume of the recording engine (its pod mounts
   `/data/backing`, and `/data/state/git` holds its history) is moved once,
   below, instead of upgraded.
3. **No recording failures open.** `tree HistoryBuildFailed` in the
   sidecar's log should be resolved first
   ([recording-failures.md](recording-failures.md)).

## Upgrade

Change the image of **both** the `agentfs-init` and `agentfs` containers
(with kustomize, one `images:` entry for `ghcr.io/mcurcio/agentfs`
covers both), and apply. The Deployment's `Recreate` strategy stops the
old pod before the new one starts, so only one daemon ever holds the
`ReadWriteOncePod` claim and the state area's lock.

The old sidecar's stop, within `terminationGracePeriodSeconds` (60 s in
the example):

1. the workload containers stop first, then the sidecar gets SIGTERM;
2. it drains the volume: files still open are saved, the view is
   unmounted, and a clean stop leaves history built through its last
   record;
3. with a `subPath` consumer the kubelet's bind holds the view, so the
   sidecar waits after its drain until the kubelet kills it at the grace
   ([deployment.md](../deployment.md#3-the-pod-piece-by-piece)).

If the old pod is killed before its drain (the grace ran out, or the node
died), nothing is lost: the new daemon recovers the journal at start and
builds what was left unbuilt.

### When the state format changes

A tree volume carries its state format in `/data/state/format`
(`agentfs-trees-15` today; [trees.md](../trees.md#the-journal)). An image
whose format differs refuses the volume with nothing written: `init` exits 1
naming another format, and the pod does not become Ready. Formats are never
converted, and [CHANGELOG.md](../../CHANGELOG.md) says which release changes
the format. To move a volume across such a release:

1. With the old image still running, note the history's head (step 2 of
   [Before](#before)) and copy out what the workload needs to carry over,
   through its view.
2. Scale the workload to 0 and move the old state area aside on the claim
   (`mv /data/state /data/state.agentfs-trees-14`). Never delete it: its
   `history.git` keeps every change made on the volume, readable with plain
   `git`.
3. Deploy the new image. Its `init` creates a new volume in `/data/state`;
   copy the saved files back in through the new view.

A rollback across a format change is the same move the other way: the
older image refuses the newer format too.

## Verify

```sh
kubectl rollout status deployment/agentfs-example
kubectl exec "$POD" -c agentfs -- agentfs --version
kubectl exec "$POD" -c agentfs -- agentfs probe /agentfs/workspace
kubectl logs "$POD" -c agentfs | head -n 20
```

(`POD` again after the rollout: the pod is new.)

- The pod is Ready: the readiness probe sees a live mount.
- The history's head is the commit you noted, or a descendant of it
  (`git --git-dir /data/state/history.git merge-base --is-ancestor <old>
  root` in the sidecar).
- The log's `not built:` lines are the operator settings you expect
  ([deployment.md](../deployment.md#4-configuration)).
- A change by the workload appears in `git log root`.

## Moving a recording volume to the tree engine

A volume the sidecar recorded with the recording engine has `/data/backing`
(the files) and `/data/state` (the journal and `git/`, its history) on the
claim. The tree engine's `init` refuses that state area, with nothing
written, naming this runbook: the pod with the new image stops at
`agentfs-init` until the volume is moved. Every step is required, in
order.

1. **With the old image, push the backlog.** Every remote must hold the
   head, because the old history does not move into the new volume: it
   stays on its remotes and in the archive.

   ```sh
   kubectl exec "$POD" -c agentfs -- agentfs push
   kubectl exec "$POD" -c agentfs -- agentfs status
   ```

   Each `remote:` line must say it holds the head (`0 unpushed`). A volume
   with no remote skips this step: step 2's archive is then the only copy
   of its audit trail.
2. **Archive the old volume.** Scale the workload to 0. The Job of
   step 3 starts by moving `/data/state` and `/data/backing` aside on the
   claim, to `/data/recording-archive/`. Never delete them. The archived
   git directory stays readable with plain git:

   ```sh
   git --git-dir /data/recording-archive/state/git log --oneline agentfs/<volume>
   ```

3. **Deploy the new pod spec once with a migration Job.** With the new
   image, privileged (FUSE needs `/dev/fuse` and `CAP_SYS_ADMIN`), as
   root, with the claim at `/data` and the new ConfigMap (settings and
   rules) at `/etc/agentfs`, run the same steps as the Hermes migration
   ([deployment.md](../deployment.md#migrating-the-existing-data)): the
   archive of step 2, `init`, the rule files, then the archived files
   copied in through the mount:

   ```sh
   set -eu
   mkdir /data/recording-archive
   mv /data/state /data/backing /data/recording-archive/
   agentfs --state /data/state init --volume <volume> --include /etc/agentfs/config
   mkdir -p /data/state/history.git/info
   cp /etc/agentfs/exclude /data/state/history.git/info/exclude
   cp /etc/agentfs/attributes /data/state/history.git/info/attributes
   mkdir -p /mnt/volume
   agentfs --state /data/state mount /mnt/volume -- sh -ec '
     cd /data/recording-archive/backing
     find . -name .git -prune -print0 | xargs -0 -r cp -a --parents -t /mnt/volume
     cp -a . /mnt/volume/'
   ```

   The block is not resumable: give the Job `backoffLimit: 0`, so a
   failure stops it instead of a retry stopping at `mkdir`. After a
   failure with `/data/recording-archive/state` in place, the archive
   step ran: move a partial `/data/state` aside (`mv /data/state
   /data/state.failed`), fix the cause from the Job's log, and run the
   block again from its `agentfs ... init` line.

   The rule files go in before the mount, which reads them when the
   volume opens; each nested repository's `.git` goes first, so the files
   beside it stay untracked. Then check the history (`git --git-dir
   /data/state/history.git log --stat root`) and scale the workload back
   to 1 with the new pod spec. The new volume's history starts at this
   import; nothing is pushed until push is built for tree volumes, and
   the old branch stays on its remotes.

## Rolling back

**To the recording engine**, after a move: scale the workload to 0, move
the tree volume's `/data/state` aside (never delete it: its history holds
every change made since the move), move `/data/recording-archive/state`
and `/data/recording-archive/backing` back to `/data`, and deploy the old
pod spec with the old image. Changes made on the tree volume since the
move are in its history, not in the recording volume: copy what you need
back by hand.

**Between images of the tree engine**: set the older image and apply.

Commits the newer version made stay in history: a rollback changes the
binary, never the history.

## What is tested

The kind job of the `image` workflow checks the stop and start an upgrade
relies on: a replaced pod finds its volume, and its last write before the
stop is in history ([deployment.md](../deployment.md#8-how-it-is-tested)).
The Hermes kind job runs the migration Job of
[deployment.md](../deployment.md#migrating-the-existing-data), the same
shape as step 3, as written. A move of a real recording volume, the
rollback to the recording engine, and a rollout from one released image to
another are not run in CI.
