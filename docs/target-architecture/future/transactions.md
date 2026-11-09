# Transactions on trees

**Proposed, not built.** This page proposes transactions: a program stages changes to several files and they land together or not at all. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What it proposes

A transaction is a frozen branch that snapshots the fork's head at begin. The program's saves commit there, and it
reads its own edits. Commit lands the branch in the fork as a child lands in its upstream, whole or not at all.
Rollback deletes the branch and leaves no trace in any tree or history. The model and its rules are
[git-model §9](../git-model.md#9-transactions-a-frozen-branch).

## What stands until then

The executable model in `crates/agentfs-model` runs transactions. The tree library and the mounted tree views do
not. A save lands whole or loses whole; there is no way to group several files.

## What is open

- **The integration's API is not defined.** A sketch, not a rule: `begin(tree)` returns an id and a staging
  directory, normal file I/O writes there, `commit(id)` answers `Committed`, `Lost{lines}` or `Empty`, and
  `rollback(id)` drops it. Something must end a transaction whose integration died, so the id needs a holder.
  Ledger R5 holds the guidance: a transaction stages file changes, and has no "who".
- **A file open inside a transaction** holds back that fork's syncs until it closes (ledger R9.18). The model counts
  only opens on the tree.
- **At commit, a file that is not open follows the normal merge rules** (ledger R10.11). The model switches two
  rules off at a commit instead.
- **Programs sharing the tree during a transaction.** A program that knows nothing of transactions closes a file
  with a deferred call, reopens it and reads the old bytes.
- **Deferring other programs' calls** on a transaction's files is the current model. A separate staging directory
  would shrink the deferral to the moment of commit.
