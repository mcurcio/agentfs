# Runbook: worktree paths found by the heuristic

`git worktree add` writes an absolute `gitdir:` into the new worktree's
`.git` file: a path as the workload sees the mount, such as
`/workspace/main/.git/worktrees/wt`. agentfs maps it to a directory in
the tree through the places it knows the tree is seen (its *views*,
design [§5](../design.md#5-nested-repositories)). When the path is under
none of them, or its exact mapping names no git directory, agentfs tries
each suffix of the path from the tree root instead (`main/.git/...`,
then `.git/...`, and so on). That usually finds the right repository,
but it can be fooled: two repositories whose admin directories end the
same way, or a path the workload never meant.

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
  ([csi.md §5](../csi.md#5-parameters)). It reaches fronts started after
  the change (a new publish, or a front restarted by the node plugin);
  existing PVs keep the volume context they were provisioned with, so
  edit a static PV or re-provision.

The counter is cumulative per process: after the fix it stops growing,
and starts from zero at the next restart. Nothing in history needs
repair unless files were recorded or skipped wrongly; check with
`agentfs status` and `git ls-tree` as in
[recovery-and-undo.md](recovery-and-undo.md).

## Known limits

Views are one list per process, only ever added to. Two fronts of one
volume that see different directories at the same path (pods with
different `subPath`s at one `mountPath`) are both tried, and the suffixes
after them; a view is not dropped when its front stops.
