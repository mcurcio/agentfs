# Faster saves and landings in the tree views

**Proposed, not built.** This page proposes making a save and a landing cost less when a tree holds large files or many saves. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

Every save and take reads and rewrites the whole tree, so a large file taxes every later step of every file.
One run on a debug build: a 1 MiB file converges in 1.27 s and a 100 MiB file in 339.1 s, and five saves of small
files took 238 to 241 s to become visible while the large file was queued ([performance](../../performance.md)).
10,000 one-line saves took 2,561 s, because the stages grow with the line map, which is never pruned.

## What it proposes

- A memo of parsed snapshots keyed by commit.
- Writing only the files that changed.
- Lowering the line referee's own cost on large files.

Each is a change to the tree library with a measurement before and after. Milestone M1 of
[one engine](one-engine.md) takes the first release-build figures on a many-file workload and sets the tree-view
budgets; this slice comes before M2 when those figures miss them.
