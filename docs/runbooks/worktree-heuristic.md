# Runbook: worktree paths found by the heuristic

`git worktree add` writes an absolute `gitdir:` into the new worktree's
`.git` file: a path as the workload sees the mount, such as
`/workspace/main/.git/worktrees/wt`. agentfs maps it to a directory in
the tree through the places it knows the tree is seen (its *views*,
design [§5](../design.md#5-nested-repositories)). When the path is under
none of them, agentfs tries each suffix of the path from the tree root
instead (`main/.git/...`, then `.git/...`, and so on). That usually
finds the right repository, but it can be fooled: two repositories
whose admin directories end the same way, or a path the workload never
meant.

## Symptoms

- `agentfs.nested.gitdir_heuristic` is above zero and growing: each
  count is one `gitdir:` or `.git` symlink target resolved by its
  suffixes.
- Rarely, a worktree's files are recorded although it is a nested
  repository (the suffix found nothing), or a directory is skipped as
  nested that is not.

## Cause

agentfs was not told where the workload mounts the volume. It knows:

- the daemon's own mount point, and every bind mount of it in the
  daemon's mount namespace;
- each `--workload-mount PATH[=DIR]` (env `AGENTFS_WORKLOAD_MOUNTS`,
  comma-separated): `PATH` is where a container sees the tree, `=DIR`
  when it sees only the tree's directory `DIR` (a `subPath`);
- at a history service, the views each front reported.

A sidecar whose workload container mounts the shared volume at another
`mountPath` than the sidecar's own, and every CSI volume without
`workloadMounts`, are such cases.

## Fix

- **Sidecar:** add the workload's `mountPath` to the agentfs container,
  for example `--workload-mount /workspace`, or `/workspace=proj` for a
  `subPath: proj`. One flag per container mount that differs.
- **CSI:** set the `workloadMounts` StorageClass parameter (or the
  static PV's volume attribute) to the `mountPath`s pods use, comma
  separated, with the same `PATH[=DIR]` syntax
  ([csi.md §5](../csi.md#5-parameters)). A StorageClass change reaches
  only PVs provisioned after it: each PV keeps the volume context it was
  provisioned with (a PV's `csi` source is immutable), so recreate a
  static PV with the attribute added, or re-provision.
  A front takes the value when its pod is published (a front the node
  plugin restarts keeps it), so restart the pods afterwards.

The counter is cumulative per process: after the fix it stops growing,
and starts from zero at the next restart. Nothing in history needs
repair unless files were recorded or skipped wrongly; check with
`agentfs status` and `git ls-tree` as in
[recovery-and-undo.md](recovery-and-undo.md).

## Known limits

Views are one list per process, only ever added to. Two fronts of one
volume that see different directories at the same path (pods with
different `subPath`s at one `mountPath`) are both tried, never the
suffixes; a view is not dropped when its front stops. A path under a
known view that maps to no git directory is not nested, even if one of
its suffixes names one: its files are recorded rather than risk leaving
an ordinary directory unrecorded.
