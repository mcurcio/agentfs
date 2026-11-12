# Cutting the tree engine into modules

**Partly built: in the model, the referee units, the tree identity, recording, queue and locks, and the root's name as a value are built; in the model and conformance, the recorder (step 4).** This page proposes one home per rule: six modules, with the tree actor only composing them,
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
| root | The native tree with no upstream. It holds the canonical history. History knows it by its identity, `root`; the branch name it is published under is a consumer's setting, changeable ([root branch name](root-branch-name.md)) | Guidance, owner, 2026-11-08: "I like the idea of agentfs having a native root to its tree which holds the canonical history, and then having a different model for mirror repos" |
| upstream | The one tree a fork syncs from and lands into. It is itself a tree, and may be a fork of another | Guidance, owner, 2026-11-08: "from the perspective of f2, f1 is its upstream. remember, there is no "main" in this new model of upstreams and forks" |
| fork | A tree with exactly one upstream | Same |
| mirror | A remote copy, such as GitHub. It is not an upstream and is not part of the tree | Guidance, owner, 2026-11-08: "GitHub is something closer to a peer than an upstream" |

What this changes in the code:

- The model has one tree identity, `calls::TreeId`: a number, the root 0 and each fork the next from 1. A tree's
  upstream is the world's answer (`World::upstream_of`), and the root is simply the tree without one. No module
  compares a tree's name to find the root: its name is configured (`world::TreeNames`).
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

Inside the referee, private units each own a few rules. Land runs them in a fixed order over one set of per-line
verdicts: undecided, verdicts, names (to a fixed point), placing, whole. Sync runs undecided, then placing, then names
(to a fixed point over the placed files). Ancestry is the port: the other units ask it as they need an answer.

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
| R9, R9.1 lock keys | queue and locks | Tree history gives each commit's keys; land gives its written keys. Deriving a key (`NAME_KEY` is queue and locks's) reads files and line maps, so in the model it stays with tree history in `World` (`changed_keys`, `touches_of`, `delivery_keys`) until tree history is cut |
| R11.2 at a landing | referee: names | A rename-over's own unlink frees its path, even when the change loses whole |
| R11.3 at a sync | referee gives it, recording takes it | The sync result lists files that came back; they get new objects, so old writers stay orphaned |
| R6.1 | tree history | The landing-pending flag comes from the queue |

## Design rules (design choices)

1. **One owner per rule.** Where one clause of a rule lives elsewhere, the collaborator is named with that clause.
2. **The referee is pure, and only land builds a `Decision`.** It takes snapshots and gives back plans. In the model
   it reads git lazily through a port of four reads (`Lineage`), and land gives back the lost versions; the world
   adds the collision ref and builds the `Decision`.
3. **The checker stays independent.** It keeps its own ancestry record, re-derives every claim from git, and shares
   neither this map nor this code.

After step 1 the checker owns its state, reads its version ancestry from git's blobs and notices a moved ref from
git's ref files. It does not yet re-derive the rest from git; step 7 does, and changes the matrix:

| Not yet re-derived | Read through `World` | Read by |
|---|---|---|
| The model's records | `landings`, `syncs`, `entered`, `collisions`, `ended`, `discarded`, `save_calls`, `last_take` | most checks |
| Each change's edits | `World::edits` | `changes_whole`, `placed_and_unlinked`, `taken_only_past` |
| Each tree's head | `World::head` and the fork list, not git's refs | `views`, the queue step's checks, `prefix_cut`, `delivered`, `entry_files`, `trees_equal_root` and others |
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
- Each engine step needs one `phase-1.yml` dispatch, the full tree-view CI run with its 500-seed shards: about
  73 min wall and about 300 runner-min (run 43146523186). A full model gate takes about 41 min on a 4-CPU
  container, outside CI. [One engine](one-engine.md), "Costs", totals the dispatches and gates before and after
  this cut.
- Built model step 3 keeps these, each a later step's to remove:
  - Queue and locks asks the referee through `Plans`: a take needs a landing's written keys and the tree its `Cut`
    carries in the same step. An engine queue must be given both.
  - Whether a call is deferred is two records: `Handle::deferred` in recording and the order in queue and locks,
    kept in step only by `World::defer`. Deriving one from the other removes the corpus bug `deferred_flag_dropped`,
    which plants a bug in the flag alone, and so changes the matrix.
  - `Recording::replayed` is `remove` for a crash's replay, kept apart because the corpus bug
    `crash_relinks_a_dead_open` edits the replay alone; folding it into `remove` would plant that bug at every close.

## Steps

Built: the safety net (the golden digest, the planted-bug corpus and the guarded runner) and the checker's own state,
`Checked` and `Checker` in `crates/agentfs-model/src/check.rs`. The model's referee is in
`crates/agentfs-model/src/referee/`: one file per unit, with `land` and `sync` its only entry points. The world builds
the trees from their plans. Recording is `crates/agentfs-model/src/recording.rs`: opens, their byte calls, objects,
orphans and each close's save, read through a `Files` port; the world commits the save. Queue and locks is
`crates/agentfs-model/src/queue.rs`: the queues, P1's scan, the locks a take holds, the `Cut` a sync delivers to,
and the order of deferred calls, read through two ports the world implements: `Keys` (lock keys and lineage, all P1's
scan reads) and `Plans` (a landing's plan and written keys, a sync's tree). Every tree's git name comes from one
value, `world::TreeNames`, which the world is given: the model's runs name the root `MODEL_ROOT` (`root`), and
conformance names it as the engine does (`ENGINE_ROOT`). The world composes them;
it still holds tree history, transactions, the journal's replay, replication and the trace, which no step here
cuts. How to use the net is in the model's `VERIFYING.md`
("Refactoring without changing outcomes") and the repo skill `.claude/skills/model-runs`.

**The model gate.** It holds for every model step below unless the row says otherwise. Every run goes through
`ci/model-docker.sh`, with no other model container running:

- `ci/model-digest.sh check` matches all 2620 runs;
- the corpus in fast mode reproduces `planted/matrix.tsv`;
- both stay within `planted/budget.json`;
- `run.py --anchors-only` reports no STALE;
- the model, conformance and safety-net tests pass.

A step whose diff meets the light tier's test in `VERIFYING.md` ("Refactoring without changing outcomes") runs that
tier instead (guidance, owner, 2026-11-11). Measured on step 1: digest 627 s, corpus 235 s. A matrix row or digest that changes is a finding to explain, never a
re-record, except where a row says the digest is re-baselined on purpose.

**How each step runs.** Each step is its own session and its own PR to the repository's `main` branch, in this
order. Step 3 is three PRs, 3a to 3c (guidance, owner, 2026-11-11: "2 PRs is fine"; then "New pr is fine"), and step 5
is three, 5.1 to 5.3. Step 7 runs after the baseline of [one engine](one-engine.md), beside its M8:

1. **Architect.** It designs the step and probes its risky seams, before anyone implements it. Guidance, owner,
   2026-11-09: "I want each phase of the work to start with an architect".
2. **Implementer.** It builds the step in a few large slices, each green alone.
3. **Review.** One adversarial review attacks what the gate cannot see.
4. **Fix round.** One round of fixes, checked by the gate again.
5. **CI.** The ci and image runs on the PR must be green.

A change to the safety net itself runs its quick tier and a review before any full run.

| # | Step | Gate |
|---|---|---|
| 2 | Built. Model: referee units out of `resolve`, `adopt` and `splice` | The model gate |
| 3a | Built. Model: one tree identity, `calls::TreeId`, with its upstream from `World::upstream_of`, keeping today's order (root first, forks ascending); the digest's tree labels come from the one place that names trees (now `world::TreeNames`), and `canonical_text_is_pinned` is unchanged | The model gate. Names may change, outcomes may not |
| 3b | Built. Model: recording, and queue and locks, split out, with `World` composing them. The replicate step keeps its place among the pending steps, because random runs draw from that list | The model gate |
| 3c | Built. Model and conformance: no tree is named "main". `Caller::Root` and `Client::Root`, the trace's root caller `r:<hex>`, the root's name a value (`world::TreeNames`, given to `World::new`; `MODEL_ROOT` is `root`), and conformance's one engine name, `ENGINE_ROOT`. Five scenario tests renamed, with their digest keys and byte-identical values (guidance, owner, 2026-11-11, asked whether they may be renamed with identical values: "Yes"). Writer labels (`m1`, ...) stay: they are data the digest pins. `digest`'s unit tests run one run under four root names and get one text, and check the names `TreeNames::new` refuses | The light tier of the model gate (`VERIFYING.md`, "Refactoring without changing outcomes"), by the owner's exception for this step (guidance, 2026-11-11: "That gate process seems too heavy for this"). By the tier's own test its diff needs the full gate (a new type, changed signatures, a computed ref in `check.rs`), so the seeds' digests (`check --all`) and the whole corpus are not shown for it. Plus one `phase-1.yml` dispatch, the only CI that runs the oracle and the new `r:` traces |
| 4 | Built. Recorder: an engine call sequence becomes a model trace, so a regression found on the library or a mount replays in the model. Guidance, owner, 2026-11-07: "if new regressions are found in the implementation, then those regressions can be verified and fixed via the git model". One recorder, `calls::Recorder`, behind one entry, `calls::Sink`, which takes each call with its result as it happens, failed calls included (` !<errno>` in the trace), and drives no system. Source A, the harness, feeds it: `calls::replay` records as it replays, on the model, the library and mounts, so there is one driver. Source B, the engine's own events, feeds the same entry in [one engine](one-engine.md)'s M5. Guidance, owner, 2026-11-11: "I agree with the approach of “design for both, build the A solution first”." What the trees do not show is a port, `calls::History`: `world::ModelHistory` and `agentfs_conformance::EngineHistory`, which reads git. Conformance names the engine's trees through `EngineNames`, the root's from `ENGINE_ROOT` alone. The regression introduced in `6049b1b` and fixed in `e9ab747` is a fixture in `crates/agentfs-conformance/tests/regressions/`, beside the patch that removes its fix (`VERIFYING.md`, "A regression found on the engine"). Costs: a record carries whole file bytes (each tree's files, a write's bytes), which source B, giving content hashes, cannot fill as it stands, so "one sink both sources feed" is untested until B's adapter gives `Recorded` a hashed form; and only the harness feeds the sink today | One known regression replays |
| 6 | Model: R9.18, R10.5 and R10.11 (see [transactions](transactions.md)); the built-in `remote` and its replicate step leave `World`. `MODEL.md` says Replication is "due on every move of the root and every collision", which is stricter than any check: a planted bug that dropped the flag at a collision changed no digest and tripped no check, because the next move of the root pushes anyway. This step settles what a mirror's push timing must be | The digest is re-baselined on purpose; finding 30's test is no longer ignored |
| 5b | Engine and model: every tree by its identity, `root` or `fork/<n>`, in refs, queues, collision refs, checkouts and the `Agentfs-Tree` trailer; no `main` anywhere; `ENGINE_ROOT` deleted and `TreeNames` takes no root; tree state format 3. Three slices: the engine (S1), the model (S2), the baseline pages (S3) | The light tier on S1 and S3; the full model gate on S2; one `phase-1.yml` dispatch |
| 5.1 | Engine: typed line versions and event numbers; each rule once (the ancestry check, the rule-3 verdict, name combining, R3e), proved equal to the old code by a differential test that its last commit deletes | One `phase-1.yml` dispatch; conformance traces on every scenario and seed stay at 0 mismatched |
| 5.2 | Engine: verbatim moves of `tree_actor/mod.rs` into `recording/`, `history/`, `journal/` and `tree.rs` | ci and image; moves only |
| 5.3 | Engine: the modules take their own state: the `Cut`, `Upstreams`, the ref layout (`history/refs.rs`) and every trailer (`history/message.rs`) in one home each | One `phase-1.yml` dispatch, the step's gate; 0 mismatched traces |
| 7 | Model: the checker re-derives from git what it still reads through `World` (the table under design rule 3). The last step of this cut: its commit deletes this page and repoints the links to it ([one engine](one-engine.md)) | The full model gate; the matrix changes on purpose, with every planted bug still caught |

Not in this slice: the older engine (`volume.rs`, `push.rs`), mirrors on the tree model and the root's published
name. Each is a milestone of [one engine](one-engine.md), which starts after step 5; step 7 runs after its baseline.
