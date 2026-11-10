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
- The model's built-in `remote` leaves `World` (step 6). Tree history publishes that a tree moved; mirrors subscribe.
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

After step 1 the checker owns its state, reads its version ancestry from git's blobs and notices a moved ref from
git's ref files. It does not yet re-derive the rest from git; a later step does, and that step changes the matrix:

| Not yet re-derived | Read through `World` | Read by |
|---|---|---|
| The model's records | `landings`, `syncs`, `entered`, `collisions`, `ended`, `discarded`, `save_calls`, `last_take` | most checks |
| Each change's edits | `World::edits` | `changes_whole`, `placed_and_unlinked`, `taken_only_past` |
| Each tree's head | `World::head`, `World::main_head` and the fork list, not git's refs | `views`, the queue step's checks, `prefix_cut`, `delivered`, `entry_files`, `trees_equal_main` and others |
| A file at a commit, and ancestry | `World::file` and `World::is_ancestor`, over `World`'s caches of git objects | almost every check |
| Queues, waits, opens and handles | `queue_numbered`, `waits`, `write_open`, `handles` | `locks`, `p1_first`, the queue step's checks |

## Patterns

| Pattern | Where |
|---|---|
| Functional core, imperative shell | The referee and save are pure; the actor, tree history and journal do the I/O |
| Ports and adapters | Ancestry and commit reachability: over git in the model, over line maps and commits in the engine |
| Unit of work with a write-ahead log | One actor step is one unit; the journal is the commit point |
| Observer | Tree history publishes `TreeMoved`; mirrors subscribe |

## Costs

- The longest functions get shorter, but rule branches and the checker's 24 excuses move rather than disappear. The
  rules set the number of edge cases.
- The model and the engine share one decomposition, so a wrong grouping shows up in both. The checker shares none of it.
- Rule work pauses while the split runs: about 6 to 8 days in all (estimate).
- Each engine step needs the full tree-view CI run, about 2 to 3 hours.

## Steps

Built: the safety net (the golden digest, the planted-bug corpus and the guarded runner) and the checker's own state,
`Checked` and `Checker` in `crates/agentfs-model/src/check.rs`. How to use the net is in the model's `VERIFYING.md`
("Refactoring without changing outcomes") and the repo skill `.claude/skills/model-runs`.

**The model gate.** It holds for every model step below unless the row says otherwise. Every run goes through
`ci/model-docker.sh`, with no other model container running:

- `ci/model-digest.sh check` matches all 2620 runs;
- the corpus in fast mode reproduces `planted/matrix.tsv`;
- both stay within `planted/budget.json`;
- `run.py --anchors-only` reports no STALE;
- the model, conformance and safety-net tests pass.

Measured on step 1: digest 627 s, corpus 235 s. A matrix row or digest that changes is a finding to explain, never a
re-record, except where a row says the digest is re-baselined on purpose.

**How each step runs.** Each step is its own session and its own PR to `main`, in this order:

1. **Architect.** It designs the step and probes its risky seams, before anyone implements it. Guidance, owner,
   2026-11-09: "I want each phase of the work to start with an architect".
2. **Implementer.** It builds the step in a few large slices, each green alone.
3. **Review.** One adversarial review attacks what the gate cannot see.
4. **Fix round.** One round of fixes, checked by the gate again.
5. **CI.** The ci and image runs on the PR must be green.

A change to the safety net itself runs its quick tier and a review before any full run.

| # | Step | Gate |
|---|---|---|
| 2 | Model: referee units out of `resolve`, `adopt` and `splice` | The model gate |
| 3 | Model: queue and locks, and recording, split out. One tree identity with an optional upstream, keeping today's order (root first, forks ascending). `World` is the composition root. The replicate step keeps its place among the pending steps, because random runs draw from that list. The digest's tree labels come from `World::name`, `World::branch`, `World::txn_name` and `World::txn_branch`, so this step edits them, and `canonical_text_is_pinned` must stay unchanged | The model gate. Names may change, outcomes may not |
| 4 | Recorder: an engine call sequence becomes a model trace, so a regression found on a mount replays in the model. Guidance, owner, 2026-11-07: "if new regressions are found in the implementation, then those regressions can be verified and fixed via the git model" | One known regression replays |
| 5 | Engine: remove the duplicated ancestry check, name combining and R3e; a typed version; then split `tree_actor/mod.rs` | Conformance traces on every scenario and seed stay at 0 mismatched |
| 6 | Model: R9.18, R10.5 and R10.11 (see [transactions](transactions.md)); the built-in `remote` and its replicate step leave `World`. `MODEL.md` says Replication is "due on every move of main and every collision", which is stricter than any check: a planted bug that dropped the flag at a collision changed no digest and tripped no check, because the next move of main pushes anyway. This step settles what a mirror's push timing must be | The digest is re-baselined on purpose; finding 30's test is no longer ignored |
| later | Checker: re-derive from git what it still reads through `World` (the table under design rule 3) | The matrix changes on purpose, with every planted bug still caught |

Not in this slice: the older engine (`volume.rs`, `push.rs`), and building mirrors on the tree model.
