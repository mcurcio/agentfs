# Patches

Change a volume's files from outside it: queue a patch, and the next mount
applies it before the workload sees the tree. The design, with every
guarantee, is [design §12](design.md#12-patch-queue-for-iac-phase-3-built).

```sh
# In the producer's git repository (its remote and credentials are used):
agentfs patch submit --volume demo --put config/app.yaml=./app.yaml -m "Raise the limit"
agentfs patch submit --volume demo HEAD          # a commit whose parent is on agentfs/demo

# On the volume:
agentfs --state /data/state status               # patches:   last applied 3, 0 queued, ok (from origin)
agentfs --state /data/state patch status         # the queue, a block and why
agentfs --state /data/state --root /data/backing patch apply   # with the volume stopped
```

- A patch is one commit with one parent on the volume's branch, at
  `refs/agentfs/<volume>/inbox/<n>` (ten digits). Submitters that race
  for a number take the next one: the ref is created with
  `git push --force-with-lease=<ref>:`.
- The mount fetches the inbox (`agentfs.patchFetchTimeout`, default 30s),
  cherry-picks each patch onto the head through the same checks as
  `restore`, and records it as one commit (`Agentfs-Trigger: patch`,
  `Agentfs-Patch`, `Agentfs-Inbox`). A crash at any point applies nothing
  twice: the branch's trailers decide what is applied.
- A patch that conflicts (or is otherwise refused) is set aside whole and
  the rest go on; a later patch on its files, or any later patch behind a
  barrier (`--barrier`), waits. An acceptance rule (`agentfs.patchAccept`:
  a clean merge by default, `patch approve`, or a hook) decides whether a
  clean merge lands. A gap, unrecorded changes or a failure stops the
  queue; the mount carries on either way. Fix it with `patch submit
  --replace N` or `--skip N` (both also fill a missing number), then
  restart.
- The push to the patch remote (`agentfs.patchRemote`) moves
  `applied/<n>`, deletes `inbox/<n>` and updates the status ref
  (`refs/agentfs/<volume>/status`, holding `status.json`).
- The same patch commit queued twice applies once: the later number gets
  a marker commit (`duplicate of patch <m>`), so a change is never
  applied again.

Runbook: [the patch queue](runbooks/patch-queue.md).
