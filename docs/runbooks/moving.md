# Runbook: moving a volume to another node

Each claim's worktree (its files and its branch `agentfs/<volume>`) lives
on the disk of the node that created it, for good: one branch per
worktree ([design §14.2](../design.md#142-csi-driver-built), §19.1
2026-10-16). A volume never moves. To continue its work on another node,
you make a **new volume started from the old branch**: a claim on a
start class (`startFrom: <remote>:agentfs/<old volume>@<commit>`) gets a
new worktree on the node its pod is scheduled to, whose history begins at
that commit, fetched once from the remote, on a new branch
`agentfs/<new volume>`. Which branch is current is your choice; nothing
fences the old one. Move when a node is lost, drained for good or being
retired.

What a move costs: the new volume has a new branch, so readers, iac's
patches and alerts must be pointed at it by hand, and the old volume's
patch queue and `diverged/*` refs stay with the old branch. A start
writes the commit's tree, so files history does not record (ignored or
oversize ones), ownership (set the pod's `fsGroup`), modes beyond the
executable bit, timestamps, xattrs and empty directories are not carried
over. A local-only volume (no remote) cannot move until it has one: add
a `[remote]` section to its profile, and its first push sends the whole
history; until then its history dies with its disk.

This procedure is not run in CI as written; `ci/e2e-csi-move.sh` runs the
same steps on a two-node kind cluster.

## Symptoms

- `kubectl get nodes` shows the volume's node `NotReady`, or you are
  retiring it.
- The workload's pods are `Pending` with a `FailedScheduling` event
  naming a volume node affinity conflict: the PV is pinned to the node
  (`topology.agentfs.mcurcio.github.io/node`).

## Steps

1. **Find the volume and the commit to start from.** The PV, its volume
   name and node:

   ```sh
   kubectl get pvc <claim> -n <ns> -o jsonpath='{.spec.volumeName}{"\n"}'
   kubectl get pv <pv> -o jsonpath='{.spec.csi.volumeAttributes.volume}{"\n"}{.spec.nodeAffinity}{"\n"}'
   ```

   Then the branch's tip on the remote the new volume will start from (a
   push remote of the profile):

   ```sh
   git ls-remote <remote url> refs/heads/agentfs/<volume>
   ```

   - **The old node is up** (a planned move): stop the workload first
     (step 2), then check the old worktree from that node's plugin pod.
     Every remote must show no unpushed commits, and the tip `git
     ls-remote` shows is the commit to pin:

     ```sh
     NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
       --field-selector spec.nodeName=<old node> -o name)
     kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
       agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state status
     ```

     `remote: ... 0 unpushed` on every remote line, and `last pushed`
     equal to that tip. If a remote lacks commits, wait for the release
     scan to push them (the volume is not staged once the pods are gone),
     or run `agentfs push` there.
   - **The old node is lost**: pin the remote's tip. Commits the lost
     node had not pushed are on its disk only. If it comes back, its
     release scan pushes them to the old branch, and the new volume's
     start-branch alarm names them
     ([start-branch-moved.md](start-branch-moved.md)).
   - **The remote was on the lost node** (a git server pod with storage
     on it): recover the remote first; nothing can start without it.

2. **Stop the workload**: scale it to 0 and wait for its pods to go.

   ```sh
   kubectl scale deployment/<workload> -n <ns> --replicas=0
   ```

   A pod on a lost node stays `Terminating`: delete it with `--force
   --grace-period=0` once you know the node is down.

3. **Delete the claim.** Its PV turns `Released`: DeleteVolume runs on
   the old node, and waits (`UNAVAILABLE`, retried) while a remote lacks
   the branch's head, or the node is gone.

   ```sh
   kubectl delete pvc <claim> -n <ns>
   ```

4. **Create a start class** pinned to the commit, with the old class's
   profile and parameters:

   ```yaml
   apiVersion: storage.k8s.io/v1
   kind: StorageClass
   metadata:
     name: <class>-move-<volume>
   provisioner: agentfs.mcurcio.github.io
   parameters:
     profile: <profile>
     startFrom: "<remote>:agentfs/<volume>@<commit>"
   reclaimPolicy: Delete
   volumeBindingMode: WaitForFirstConsumer
   ```

   `<remote>` is a remote section of the profile (`origin`), `<commit>`
   the full id from step 1. A `refs/...` name such as a `diverged/*` ref
   is refused: start from a branch.

5. **Re-create the claim** on the start class (same name, so the
   workload needs no change), and **scale up**:

   ```sh
   kubectl scale deployment/<workload> -n <ns> --replicas=1
   ```

   The pod is scheduled to a node, and that node's provisioner makes the
   new worktree there from the pinned commit. A claim that stays
   `Pending` names the reason in its `ProvisioningFailed` events (an
   unreachable remote is retried; a commit not on the branch is not).

6. **Delete the start class** once the claim is bound: any later claim
   on it would start from the same commit, in any namespace.

   ```sh
   kubectl delete storageclass <class>-move-<volume>
   ```

7. **Point readers at the new branch**: the new volume's branch is
   `agentfs/<new volume>` (its `volume` attribute in `kubectl get pv`).
   Retarget iac's patches (the new volume's inbox) and any alerts or
   dashboards keyed on the old volume.

8. **The old PV.** If the old node comes back, its release scan pushes
   what it had not, and DeleteVolume completes on its own (`onDelete:
   delete` removes the old worktree; `retain` keeps it on that node's
   disk, `csi.retained.json`). If the node is gone for good, the PV
   stays `Released` with the provisioner's finalizer: remove it, and the
   old worktree's unpushed commits are lost with the disk.

   ```sh
   kubectl patch pv <old pv> --type json \
     -p '[{"op":"remove","path":"/metadata/finalizers"}]'
   ```

## Related

- [start-branch-moved.md](start-branch-moved.md): the old branch received
  commits after the move.
- [divergence.md](divergence.md): a remote branch another writer wrote.
- [csi-recovery.md](csi-recovery.md): crashed fronts, services and node
  plugins on a node that is still up.
- [history-service-down.md](history-service-down.md): fronts failing
  with `EIO` while the history service is down.
