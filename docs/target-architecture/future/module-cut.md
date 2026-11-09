# Cutting the tree actor into modules

**Proposed, not built.** This page proposes cutting the tree actor into six modules and leaving the actor to compose them. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What it proposes

The six modules are recording, queue and locks, referee, transactions, tree history and journal. The tree actor
only composes them: it owns no rule of its own. Each module gets its own interface and tests, so the referee can be
checked without a queue and the journal without a referee.

## What stands until then

`agentfs_core::tree_actor` holds all of it. Its files already separate the journal, the queue, the referee, the
sync and the page-cache logic, but they share one actor type and one lock.

## Why

The rules in the [rules ledger](../rules-ledger.md) are the specification each module implements.

## Open choices

- How the cut is verified. The replay in `crates/agentfs-conformance` is the existing check that behaviour did not change.

Transactions are a module here and also a slice of their own: [transactions](transactions.md).
