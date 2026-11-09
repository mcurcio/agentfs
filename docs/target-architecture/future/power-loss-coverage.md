# Wider power-loss coverage

**Proposed, not built.** This page proposes testing more of what a power cut can do. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

The `power-loss` workflow replays `dm-log-writes` logs in write order and checks at every FUA (or every flush, on
demand). A disk may also reorder the writes between two flushes, which in-order replay never shows. The commit
path's fsync order does not rely on order within an epoch, but only reviews say so.

## What it proposes

- Replaying random subsets of each epoch's writes.
- A power cut through a FUSE mount, not only through the CLI's `record`.
