# Runbook: moving a CSI volume

The CSI driver runs tree volumes ([csi.md](../csi.md)). Each claim's
volume lives on the disk of the node that created it, for good, and the
driver never serves a volume an older driver made on the recording
engine. This runbook covers both moves an operator makes by hand:

- [moving a recording-format volume onto the tree engine](#moving-a-recording-format-volume-onto-the-tree-engine):
  a claim made before the driver ran tree volumes;
- [moving a tree volume to another node](#moving-a-tree-volume-to-another-node):
  not built; its files can be copied, and its history does not continue.

Throughout, `<base>` is the node plugin's `--base-path` (default
`/var/lib/agentfs-csi/volumes`) and `$NODE_POD` the node plugin pod on the
volume's node:

```sh
NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
  --field-selector spec.nodeName=<node> -o name)
```

The PV names its node in `nodeAffinity` (`kubectl get pv <pv> -o yaml`).
Neither procedure is run in CI as written.

## Moving a recording-format volume onto the tree engine

**Symptoms.** After the driver's upgrade, a pod of an old claim waits in
`ContainerCreating` with a `FailedMount` event, or a deleted old claim's
PV stays `Released` with `VolumeFailedDelete` warning events, each
saying:

```text
volume <pv> is a recording-format volume (<base>/<pv>): this driver never serves or deletes it; archive it and remove the directory by hand (docs/runbooks/moving.md)
```

The plugin also names each such directory in its log at start
(`recording-format volume <pv> at <dir> is not served by this driver`).
kube-state-metrics' `kube_persistentvolume_status_phase{phase="Released"}`
can alert on the PVs left this way.

A recording-format directory holds `backing/` (the files the pods saw),
`state/` (its state area; the history is `state/git`, branch
`agentfs/<volume>`) and the old driver's `csi.json`. The volume name is
the PV's `spec.csi.volumeAttributes.volume`. Nothing in this procedure
needs the old driver; run it before or after the upgrade.

1. **Stop the old claim's pods**, so nothing writes the directory.

2. **Commit what the old volume's journal holds**, if anything. The
   recording commands still read a recording state area:

   ```sh
   kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
     agentfs --state <base>/<pv>/state --root <base>/<pv>/backing flush
   ```

3. **Archive the history**, whole, as a git bundle, and copy it off the
   node:

   ```sh
   kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
     git --git-dir <base>/<pv>/state/git bundle create /tmp/<pv>.bundle --all
   kubectl -n agentfs-csi cp "${NODE_POD#pod/}":/tmp/<pv>.bundle ./<pv>.bundle -c agentfs-csi
   ```

   Keep it wherever you keep history (a git server: `git clone
   <pv>.bundle`, then push it). Check it with `git bundle verify`.

4. **Archive the files** to carry into the new volume. The branch's tip
   holds every recorded file:

   ```sh
   kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
     git --git-dir <base>/<pv>/state/git archive --format=tar \
       -o /tmp/<pv>.tar agentfs/<volume>
   kubectl -n agentfs-csi cp "${NODE_POD#pod/}":/tmp/<pv>.tar ./<pv>.tar -c agentfs-csi
   ```

   Files history never recorded (ignored or oversize ones) are only in
   `<base>/<pv>/backing`; take them with `tar -C <base>/<pv>/backing -cf
   /tmp/<pv>-backing.tar .` instead if the workload needs them.

5. **Create a new claim on a tree class** (any class of the driver, now
   that it runs tree volumes), and a pod that mounts it. A helper pod on
   the same claim must be the only pod: one pod per claim.

6. **Unpack the archive through the new claim's mount**, from that pod,
   so every file is recorded in the new volume's history:

   ```sh
   kubectl -n <ns> cp ./<pv>.tar <pod>:/tmp/<pv>.tar
   kubectl -n <ns> exec <pod> -- tar -C <mount path> -xf /tmp/<pv>.tar
   ```

   Then delete the helper pod and point the workload at the new claim.

7. **Remove the old directory by hand** on the old node, once the bundle
   is safe:

   ```sh
   kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- rm -rf <base>/<pv>
   ```

8. **Delete the old claim**, if it is not deleted yet. The provisioner's
   next retry of DeleteVolume finds no volume and succeeds, and the PV
   goes.

What the move costs: the new volume's history starts at the unpacked
files (one save per file the pod wrote); the old history stays in the
bundle, not in the new volume. Ownership, modes beyond the executable
bit, timestamps and extended attributes are what `tar` and the pod's
user give them.

## Moving a tree volume to another node

**Not built.** A tree volume never leaves its node, and a class with
`startFrom` is refused (`startFrom is not built on tree volumes`).
Draining or losing the node leaves its pods `Pending`.

While the node is up, copy the files out through the volume's mount into
a new claim on the other node:

1. Stop the workload, then run one pod on the old claim (on the old node)
   and archive its mount: `kubectl -n <ns> exec <pod> -- tar -C <mount
   path> -cf /tmp/files.tar .` and `kubectl cp` it out.
2. Archive the history if you want it: `git --git-dir
   <base>/<pv>/state/history.git bundle create /tmp/<pv>.bundle --all` in
   the old node's plugin pod, as above.
3. Create a new claim whose first pod is scheduled on the other node, and
   unpack the files through its mount, as in step 6 above.
4. Delete the old claim: DeleteVolume keeps its directory on the old node
   (`csi.retained.json`). Remove `<base>/<pv>` there by hand once you no
   longer need it.

The new volume's history starts at the copied files; the old one's does
not continue. A volume on a node that is lost for good is lost with its
disk: push is not built, so no remote holds its history.

## Related

- [csi-recovery.md](csi-recovery.md): crashed hosts and node plugins on a
  node that is still up.
- [csi.md](../csi.md#11-where-volumes-live): where volumes live.
