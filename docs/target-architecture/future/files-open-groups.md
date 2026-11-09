# Files changed while open: database groups

**Proposed, not built.** This page proposes saving files that are held open, such as a database and its journal, as one group. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

A save happens at close. A file held open is recorded call by call in the tree's journal, saves nothing until its
close and holds back the incoming changes to it ([git-model §6](../git-model.md#6-save-and-locks)). A log file that is
never closed is never saved.

## What it proposes

A **group** is a set of files recorded whole: a database and its `-wal` or `-journal`. The [Rules domain](../domains/rules.md)
names the kinds. At a chosen point (an `fsync`, an interval, the last close) the tree records every member as one
atomic commit, as the existing engine's fsync class does. A group is shown, hidden and recorded whole.

## Open choices

- What counts as a safe point on a mounted tree, when no program calls `fsync`.
- How a capture interacts with a take waiting on one of the members.
- Restore of a live database still needs the workload stopped.
