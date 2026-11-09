# Cutting the tree engine into modules

**Proposed, not built.** This page proposes one home per rule: six modules, with the tree actor only composing them,
in both the model and the engine. The commit that lands this slice deletes this page and moves the behaviour into the
library docs.

No rule changes. The [rules ledger](../rules-ledger.md) and [agentfs in git terms](../git-model.md) stay the
specification and the baseline proof.

```text
                 actor: composition only (one step is one unit of work)
      ┌───────────┬──────────────┼──────────────┬──────────────┐
      ▼           ▼              ▼              ▼              ▼
  recording   queue and       referee      tree history     journal
              locks            (pure)      ──TreeMoved──▶ mirrors (a separate model, not in this cut)
                  │               ▲
                  └──── Cut ──────┘         transactions: model only, until built
```

## The shape of the tree

Every statement here is about one fork and its one upstream.

| Term | Means | Source |
|---|---|---|
| root | The native tree with no upstream. It holds the canonical history | Guidance, owner, 2026-11-08: "I like the idea of agentfs having a native root to its tree which holds the canonical history, and then having a different model for mirror repos" |
| upstream | The one tree a fork syncs from and lands into. It is itself a tree, and may be a fork of another | Guidance, owner, 2026-11-08: "from the perspective of f2, f1 is its upstream. remember, there is no "main" in this new model of upstreams and forks" |
| fork | A tree with exactly one upstream | Same |
| mirror | A remote copy, such as GitHub. It is not an upstream and is not part of the tree | Guidance, owner, 2026-11-08: "GitHub is something closer to a peer than an upstream" |

What this changes in the code:

- `Tree::Main` and `Tree::Fork` in the model become one tree identity with an optional upstream. The root is simply
  the tree without one. No module asks whether a tree is "main".
- The referee takes one pair at a time: land is a fork into its upstream, sync is an upstream into a fork.
- The model's built-in `remote` leaves `World`. Tree history publishes that a tree moved; mirrors subscribe.
  What a mirror may send back, pull requests included, belongs to the mirror's own design
  ([Replication](../domains/replication.md)).

## Modules

| Module | Owns | Takes | Gives | Never |
|---|---|---|---|---|
| recording | close-to-open, pages, paths, file identities, opens, orphans, saves | kernel calls; files a sync brought back | a save, or "no change" | judges or merges |
| queue and locks | order per source, write locks, the `Cut`, deferral | each commit's keys; a plan's written keys | the next eligible take; a `Cut` | reads line content |
| referee | every line and name rule; only land decides | trees of one pair, undecided names, last lines, a `Cut` | land: a `Decision` and its written keys; sync: a sync result | does I/O |
| transactions | frozen branch and its lifecycle | begin, commit, rollback | a landing whose upstream is the fork | holds locks |
| tree history | commits, trailers, fast-forward or merge, lineage, collision refs, decision index | a `Decision` or a save | commits, refs, `TreeMoved` | judges |
| journal | the commit point, recovery, compaction | records | a folded state | decides |
| actor | composition only | everything, through ports | nothing of its own | holds rule logic |

Inside the referee, private units run in a fixed order over one set of per-line verdicts: ancestry, undecided, verdicts,
names (to a fixed point), placing, whole (land only). Land and sync run the same units.

| New term | Means |
|---|---|
| `Cut` | The last upstream commit a prefix sync takes (R9.3). Only queue and locks can make one |
| unit | A private part of the referee that owns a few rules |
| join | An unfinished line with a live line placed after it, which would show bytes nobody wrote (R10.4, R10.5, R10.6, R10.8, R10.9) |
| sync result | A sync's tree, what it hid, and the files that came back as new objects (R11.3) |

## Where a rule crosses a boundary

| Rules | Owner | How it crosses |
|---|---|---|
| R9.3, R9.9, R3d | queue and locks owns the `Cut`; the referee owns what each commit touches | Queue and locks walks the upstream's commits one at a time and stops at the first that touches an open file. Sync accepts only a `Cut` |
| R9, R9.1 lock keys | queue and locks | Tree history gives each commit's keys; land gives its written keys |
| R11.2 at a landing | referee: names | A rename-over's own unlink frees its path, even when the change loses whole |
| R11.3 at a sync | referee gives it, recording takes it | The sync result lists files that came back; they get new objects, so old writers stay orphaned |
| R6.1 | tree history | The landing-pending flag comes from the queue |

## Design rules (design choices)

1. **One owner per rule.** Where one clause of a rule lives elsewhere, the collaborator is named with that clause.
2. **The referee is pure, and only land builds a `Decision`.** It takes snapshots and gives back plans.
3. **The checker stays independent.** It keeps its own ancestry record, re-derives every claim from git, and shares
   neither this map nor this code.

## Patterns

| Pattern | Where |
|---|---|
| Functional core, imperative shell | The referee and save are pure; the actor, tree history and journal do the I/O |
| Ports and adapters | Ancestry and commit reachability: over git in the model, over line maps and commits in the engine |
| Unit of work with a write-ahead log | One actor step is one unit; the journal is the commit point |
| Observer | Tree history publishes `TreeMoved`; mirrors subscribe |

## Costs

- The longest function falls from 248 lines to about 85 (estimate), but rule branches and the checker's 24 excuses
  move rather than disappear. The rules set the number of edge cases.
- The model and the engine share one decomposition, so a wrong grouping shows up in both. The checker shares none of it.
- Rule work pauses while the split runs: about 6 to 8 days in all (estimate).
- Each engine step needs the full tree-view CI run, about 2 to 3 hours.

## Steps

| # | Step | Gate |
|---|---|---|
| 0 | Safety baseline: a golden digest of final refs and commit ids over every scenario and seeds 1 to 500; the planted-bug corpus as patches, with a runner and a baseline matrix | Digest reproduces; every planted bug is caught |
| 1 | Model: checker state out of `World` | Digest, scenarios, 500 seeds and the bug matrix unchanged |
| 2 | Model: referee units out of `resolve`, `adopt` and `splice` | Same |
| 3 | Model: queue and locks, and recording, split out; one tree identity with an optional upstream; `World` is the composition root | Same. Names may change, outcomes may not |
| 4 | Recorder: an engine call sequence becomes a model trace, so a regression found on a mount replays in the model. Guidance, owner, 2026-11-07: "if new regressions are found in the implementation, then those regressions can be verified and fixed via the git model" | One known regression replays |
| 5 | Engine: remove the duplicated ancestry check, name combining and R3e; a typed version; then split `tree_actor/mod.rs` | Conformance traces on every scenario and seed stay at 0 mismatched |
| 6 | Model: R9.18, R10.5 and R10.11 (see [transactions](transactions.md)) | The digest is re-baselined on purpose; finding 30's test is no longer ignored |

Not in this slice: the older engine (`volume.rs`, `push.rs`), and building mirrors on the tree model.
