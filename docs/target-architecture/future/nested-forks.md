# Nested forks on trees

**Proposed, not built.** This page proposes forks of forks in the tree library and the mounted tree views. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What it proposes

A fork is the upstream of the forks inside it. Each collision is decided once, at the queue of the tree the change
lands in, and every tree below learns it by sync ([git-model §10](../git-model.md#10-nested-forks)). A fork receives
all changes from its upstream except what a lock or an open file holds back.

## What stands until then

The model runs nested forks. A fork's prefix sync past its upstream's fast-forward is checked in the model only.
The tree library and the mounted tree views do not cover nested forks.

## What it needs

- The queue of a fork lands into its own tree as a referee, and carries the landing on to its upstream.
- A nested landing names the writer it carries.
- Replay of the model's nested scenarios on the library and on real mounts, as `crates/agentfs-conformance` does for the cases in scope.
