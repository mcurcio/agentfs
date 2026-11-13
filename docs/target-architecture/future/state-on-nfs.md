# State areas on NFS on trees

**Proposed, not built.** This page proposes keeping a tree-view volume's state area on NFS. The commit that lands
this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

A CLI or sidecar state area of the recording engine may live on NFS 4.1 or later: its holder lock reaches the
other clients, so a second `serve-history` on another client exits 75, and an older NFS version is refused. The CSI
driver keeps worktrees on each node's own disk and refuses a base path on NFS. `ci/e2e-nfs.sh` (the ci workflow's
`e2e-nfs` job) checks this with two clients of one export.

The tree engine's single-writer lock on its state area has not been checked on NFS. The recording engine's support
goes when the `agentfs` binary moves (M7) and is deleted at M9 ([one engine](one-engine.md), "Deferred extras").

## What it proposes

Run the same two-client checks against a tree-view state area: the lock reaches the other client, an older NFS
version is refused by name, and the journal's flushes hold. Its gate decides whether a tree-view state area on NFS
is supported or refused at boot.
