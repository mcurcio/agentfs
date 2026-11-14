# Brief: tree views speed, before M1 S3 (2026-11-13)

Repo /home/user/wt-step6, branch trees/module-cut-step-6 (PR #39, the one PR). Read AGENTS.md first. Design agents: read-only, no builds, no docker (a fast-suite agent owns the one build container now).

## Owner answers (2026-11-13, general guidance, picked from options)
- Speed is fixed before M1 S3: "Speed before S3 (Recommended)": make a save's cost grow with the change, not the tree.
- The bar: "Match recording engine": release build, 4 KB save p50 <= 3.5 ms, 100,000 creates in one directory <= 600 s, 1 GiB to history in 7-12 s (the recording engine's budgets, `ci/bench-budget.json`, docs/performance.md).
- Principle 1 is the baseline and is never traded for speed: a change that reached the filesystem is recorded before the call returns, or fails loudly.

## Evidence (S2's report, release image on GitHub runners)
- close_4k p50 75.3 ms (ext4) / 88.6 ms (XFS), p99 91-107 ms.
- create_1000: unfinished after ~44 min on each fs. Debug local: 482 files in ~19 min. Every save rewrites the whole tree: cost grows with the tree, total roughly quadratic.
- Guarantee gap: the view saves on FUSE `release`, which the kernel sends after close(2) returns. Only "committed before the view answers any later request" holds. The fix belongs in this work (flush is synchronous in close(2); fsync too).
- S2's work is on branch wip/m1-s2; erratum in .claude/errata.md (2026-11-13, S2).

## Code
- Engine: crates/agentfs-core/src/tree_actor/ (recording/, history/, journal/, tree.rs, referee/). Views: crates/agentfs-fuse (tree view ops). The recording engine for comparison: how it meets 3.5 ms (journal before close, commit afterwards: `create.drain_s`).
- M1 contracts landed in 6c7a936/6d14e2c (crate agentfs-contracts and domain crates); M1 pages: /tmp/claude-0/-home-user-agentfs/41ac0cfe-83c8-57b0-9f09-59a6e4c29f38/scratchpad/next/m1-pages.md (S3-S6 build on whatever this changes).
- The model (crates/agentfs-model) is the spec of landing rules; speed work must not change what history says (digests unchanged unless a finding is explained).

## Owner direction, 2026-11-13 night (supersedes the synchronous-commit premise)
"only the diff/patch needs to be syncrhonosly collected and queued, while the git history persistence could be an asynchronous trailing process." Power-cut safety: "not power safe until the process closes and flushes (or the queue catches up)". So: writes are collected and queued (survive an agentfs crash) without a per-write fsync; close/flush (and fsync) make them power-cut safe; git commits are built by a trailing process from the queue.

## Owner answer, 2026-11-13 night: Q1 = A, "Hashed sub-folders" (B4 uses it).
