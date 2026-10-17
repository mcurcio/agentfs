# Runbook: the start branch moved

A volume started from another worktree's branch (a move,
[moving.md](moving.md)) watches that branch: at every push to its start
remote it lists the start branch, and while the branch holds commits this
volume has not taken in or accepted, it raises **`StartBranchMoved`**
([design §11.3](../design.md#113-limits-and-rejected-pushes)). That happens when the
old node comes back after the move and its release scan pushes the
commits it had not pushed: they reach the old branch, never the new
volume's. Nothing is lost or overwritten; the alarm says the two
histories differ, until you decide.

## Symptoms

- `alarm: start branch moved: refs/heads/agentfs/<old> on <remote>, which
  this volume started from at <pinned>, is now at <tip>` in the history
  service's log (the node plugin's log under CSI, `[service <pv>]`).
- The log event `agentfs.start_branch_moved` and the gauge
  `agentfs.start.moved{remote}` at 1 ([telemetry.md](../telemetry.md)).
- `agentfs status` on the new volume:

  ```text
  start:     origin:agentfs/<old>@<pinned>
  start branch moved: <pinned>..<tip> (agentfs start accept --tip <tip>)
  ```

## Steps

1. **See what the old branch holds that the new volume does not**, in
   any clone of the remote:

   ```sh
   git fetch <remote url> refs/heads/agentfs/<old>
   git log --stat <pinned>..<tip>
   ```

2. **Decide.** Either the commits matter and the workload should have
   them, or they are stale and the new volume's history is the one to
   keep. Both end at step 4.

3. **Take them in, if they matter**: as a patch queued for the new volume
   (`agentfs patch submit`, [patches.md](../patches.md)), or by copying
   the files through the new volume's mount. Either records them in the
   new volume's history as new commits. A merge of the two branches is
   your own git work, outside agentfs.

4. **Accept the tip** on the new volume (CSI: in the node plugin pod of
   the node that holds it; it takes no lock, so the volume may stay in
   use):

   ```sh
   NODE_POD=$(kubectl -n agentfs-csi get pod -l app.kubernetes.io/name=agentfs-csi-node \
     --field-selector spec.nodeName=<node> -o name)
   kubectl -n agentfs-csi exec "$NODE_POD" -c agentfs-csi -- \
     agentfs --state /var/lib/agentfs-csi/volumes/<new pv>/state start accept --tip <tip>
   ```

   It fetches the start branch, checks `<tip>` is on it, and saves it as
   `refs/agentfs/<new volume>/accepted/<tip>` (created once, never
   moved). The next push sends that ref with the branch, so the old
   commits are also kept under the new volume's name, and no longer
   raises the alarm for any commit `<tip>` reaches. If the old branch
   moves again, the alarm comes back for the new commits: repeat.

5. **Check**: after the next push (`pushInterval`, or `agentfs push`),
   `agentfs status` has no `start branch moved:` line and the gauge is 0.

Deleting the old branch does not clear the alarm (and runbooks never
delete a worktree's branch); accepting does.

## Related

- [moving.md](moving.md): moving a volume to another node.
- [divergence.md](divergence.md): a branch another writer wrote.
