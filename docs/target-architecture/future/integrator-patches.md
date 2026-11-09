# Integrator patches, restore and revert on trees

**Proposed, not built.** This page proposes sources other than a fork's commits and syncs entering a tree's queue. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

Only a fork's commits and syncs enter a tree's queue ([git-model §5](../git-model.md#5-the-queue-one-referee-per-tree)).
The existing patch queue ([patches](../../patches.md)) and `restore` and `revert` work on the existing recording
engine and not on trees.

## What it proposes

An integrator's patch is a branch that enters the queue as a fork's change does and is judged by the same referee.
A sketch: `restore` and `revert` become commits on the tree's branch, written through the same save path. Each carries a
writer trailer, so that no commit holds two writers and the history says who did what.

## Open choices

- Whether a patch waits until nothing is attached (the existing queue's rule) or lands live: see
  [landing approvals and timing](landing-approvals.md) and [live landing](live-landing.md).
- How a restore interacts with a file held open ([files changed while open](files-open-groups.md)).
