# Space limits and history truncation

**Proposed, not built.** This page proposes limits on what a tree may use and a way to truncate history. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

Nothing limits a tree's size, and the line maps keep deleted versions and unlinked files with no trim rule. Losing
commits and their refs are pushed and kept.

## What it proposes

- A per-tree quota, as [the Trees domain](../domains/trees.md) §4 sketches, and a budget for all forks.
- A trim rule for line maps that keeps who changed what when.
- A way to truncate history only by an operator, as a manual procedure.

History is the product: nothing truncates it automatically, and a trim never drops a collision's record.
