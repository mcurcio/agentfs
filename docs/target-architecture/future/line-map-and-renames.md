# Where the line map lives, and two renames to one path

**Proposed, not built.** This page proposes answers to two open questions in the git model. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## Where the line map lives

The model keeps the line map in each file's blob. A real tree keeps the file's bytes as they are. The proposal is a
sidecar per file or a notes ref, written in the save's own commit, hidden from views and pushed or stripped by
Replication. The tree library keeps the line maps in `metadata.json` in the commit today
([trees](../../trees.md)). This slice changes that.

## Two renames to one path before the upstream decides

The upstream's queue decides it and keeps the path, but a fork that syncs first would show two files at one path. The
proposal: the fork keeps its own name until decided and shows the upstream's file under its previous name. Ledger
R11.5 is the rule's home.
