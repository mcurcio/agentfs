# Transactions on trees

**Proposed, not built.** This page proposes transactions: a program stages changes to several files and they land together or not at all. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What it proposes

A transaction is a frozen branch that snapshots the fork's head at begin, with its own index. The program's saves
stage in the index, with no commit, and it reads its own edits there. Commit records the transaction as one commit on
begin and lands it in the fork as a child's change lands in its upstream, whole or not at all: a fast-forward if the
fork has not moved (ledger R5.5). Rollback, or a crash, deletes the branch and the index and leaves no trace in any
tree or history. The model and its rules are
[git-model §9](../git-model.md#9-transactions-a-frozen-branch).

## What stands until then

The executable model in `crates/agentfs-model` runs transactions. A file open inside a transaction holds back every
take into its fork until it closes (ledger R9.18), and a transaction's commit lands in its fork under every landing
rule, for every file, open or not (ledger R10.5, R10.11). The tree library and the mounted tree views do not. A save lands whole or loses whole; there is no way to group several files.

## What is open

- **The integration's API is not defined.** A sketch, not a rule: `begin(tree)` returns an id and a staging
  directory, normal file I/O writes there, `commit(id)` answers `Committed`, `Lost{lines}` or `Empty`, and
  `rollback(id)` drops it. Something must end a transaction whose integration died, so the id needs a holder.
  Ledger R5 holds the guidance: a transaction stages file changes, and has no "who".
- **Programs sharing the tree during a transaction.** A program that knows nothing of transactions closes a file
  with a deferred call, reopens it and reads the old bytes.
- **Deferring other programs' calls** on a transaction's files is the current model. A separate staging directory
  would shrink the deferral to the moment of commit.
