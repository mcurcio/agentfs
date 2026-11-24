# agentfs-core along domain lines: what the code allows, and in what order

**Draft, awaiting the owner's answers to its questions. Written before PR #43 merged the test programs and cut debug info; its timings are estimates.**

Layer: module (crate boundaries inside system.md's "git model"). Read at `claude/kind-fermi-vvxywi` 216c3a5; nothing built
or run. **[read]**: counted in code (`use` lines and paths outside test modules).
**[est]**: for PR 1's timing harness  to confirm. The owner, 2026-11-24, general guidance: "Just two?
Can we break it down more along domain lines?"

## 1. The tree engine's domains as the code has them [read]

| Domain | Files | Lines (prod) | Commits, 30 days | Module cut's name |
|---|---|---|---|---|
| Journal (the oplog) and durability | `journal/` (mod, segment, readback, fold, commit_point, durable, compaction) | 5.3k (4.1k) | 22 | journal |
| Saved state | `tree.rs`, `meta.rs`, `pointers.rs`, `rows.rs` | 3.2k (2.5k) | 10 | tree.rs |
| Recording | `recording/` (mod, pages, file_map, layout), `untracked.rs`, `untrack.rs` | 6.1k (4.8k) | 20 | recording |
| Referee, queue and locks | `referee/`, `queue.rs`, `history/upstreams.rs` | 3.1k (1.8k) | 2 | referee; queue and locks |
| History | `history/` builder, built, built_log, feed, lag, verify, format, message, refs, retired | 7.5k (6.4k) | 35 (whole dir) | tree history |
| History's followers | `history/compactor` (0.3k + 1.7k tests), `history/repack.rs` (2.0k) | 4.0k | in the 35 | tree history |
| Landing | `takes.rs`, `history/take.rs` | 1.3k | 3 | (in history and queue) |
| Actor and call path | `mod.rs` (`TreeActor`, the one `Error`), `call_path.rs` (`Recording`, the lock) | 4.9k (3.0k) | 34 | actor |
| Measurement and lock discipline | `stages.rs` | 0.8k | 5 | none |
| Client edge and adapters | `session.rs`, `store.rs`, `state_area.rs` (implement contracts' ports) | 1.3k | 13 | none |
| Settings, git plumbing, line maps | `operator_settings.rs`, `tree_git.rs`, `tree_reconcile.rs` | 0.7k, 1.4k, 1.4k | 5, 4, 3 | none |

Total 40.9k. Of 54 commits to `agentfs-core/src` in 30 days, 5 touched recording-engine files; 52 touched history,
recording, the actor or sessions; 24 touched the journal, saved state or referee.

**The cycles [read].** Counting everything, the 19 tree modules form one cycle: every module uses the actor's one
`Error` (`tree_actor/mod.rs:168`, 25 variants), and seven blocks elsewhere add methods to `TreeActor` itself
(`recording/mod.rs:519`, `recording/pages.rs:349`, `journal/commit_point.rs:13`, `takes.rs:195` and `:258`,
`untrack.rs:22`, `history/verify.rs:748`). Leaving out the actor, the call path, stages and failpoints still leaves a
cycle of nine: journal, durability, saved state, recording, referee, queue and landing, history, compactor, rows.

| Pair | One way | The other way |
|---|---|---|
| journal and recording | `journal/mod.rs:63`: records hold `Call`, `FileMap` | `recording/file_map.rs:32`: `FramePos`, `Relocations` |
| journal and saved state | `journal/fold.rs:23`: `SavedTree` | `tree.rs:17`: `Change`, `SaveOutcome`, `TakeRecord` |
| journal and history | the builder reads frames and folds | `journal/fold.rs:17` `history::Objects`; `tree.rs:16` `Retired` |
| saved state and referee | `tree.rs:20` | `referee/mod.rs:20`: the values live in `tree.rs` |
| durability, history to call path | `journal/durable.rs:18`, `history/built.rs:19`: `Then`, the callback the mark replaces | the call path holds both |
| history and compactor | `built.rs:764` `AtBuilt` | the compactor reads `History` |
| measurement to history | `stages.rs:516-518` reads `feed::fed` and `tree_git`'s counters | everything times through `stages` |

- **Already clean:** `tree_git` and `tree_reconcile` (shared leaves only); `operator_settings` (two small edges,
  `history/lag.rs:24`, `operator_settings.rs:184`); the adapters and the untracked area (only through the actor).
- **About 660 crate-private items** (`pub(super)`, `pub(in crate::tree_actor)`) carry the cycle's traffic: journal 166,
  history 227, recording 101, referee 58, `tree.rs` 110. A crate cut makes each a public-surface choice: design, not sed.
- **Existing domain crates.** agentfs-volume, -clients and -trees are used: the launcher (`agentfs/src/launcher/domains.rs`)
  composes them over core's adapters `TreeStore`, `TreeStateArea`, `Sessions`; core has them as dev-dependencies only.
  No engine module belongs in them: they own pure lifecycles, the engine is "the git model" (system.md, section 1). The
  git model has no contract in agentfs-contracts (fuse calls the concrete `Recording`), and its event `TreeMoved` is
  crate-private (`history/mod.rs:82`).
- **Duplicates.** A fork's upstream has two homes, the Trees row (`ForkFacts.upstream`, `contracts/src/trees.rs:46`) and
  `Upstreams`; "no forks of forks" is checked in both (`agentfs-trees/src/service.rs:180`, `check_recovered`). The
  recording engine's `volume.rs`, `fork/`, `rules.rs`, `journal.rs`, `history.rs`, `repack.rs` are its own versions of
  those domains (M9 deletes them). `stages.rs` mixes measurement with the lock discipline (`stages.rs:660-749`).

## 2. Target crate graph (acyclic; names are placeholders, Q2)

```
 agentfs-fuse     agentfs (binary)     agentfs-csi     agentfs-conformance
       \                 |                   |                 /
        +----------- agentfs-tree-engine ---+-----------------+     agentfs-core: the recording
          actor, call path, recording, landing, sessions,            engine, telemetry, config
          store and state-area adapters, operator settings           (until M9)
                 |                  \                                     |
                 |             agentfs-history                            |
                 |     builder, built, verify, compactor, repack          |
                 |                  /                                     |
              agentfs-journal: segments, records, durability, file       |
              maps, layouts, saved trees, pointers, rows, fold, stages   |
               /              |               \                           |
  agentfs-referee        agentfs-mark       agentfs-git (tree_git)        |
  values, queue and      marks, waits,          |                         |
  locks, referee (pure)  executors          agentfs-base  <---------------+
       |                                    paths fsutil file_lock failpoints git_cli confined init_libgit2
  agentfs-lines (line maps) -> agentfs-align        agentfs-contracts and agentfs-rules sit under every tree crate
```

| Crate | Public surface | Comes from |
|---|---|---|
| agentfs-base | what those six leaves expose today, plus `init_libgit2`, `PACK_CONFIG`, `parse_size` | plan v1 |
| agentfs-git | `TreeGit` (objects, the ref copy, checkout), its `Error`, two counters | `tree_git.rs` |
| agentfs-lines | `LineMap<V>`, `Image`, `Expanded`, `Line`, `Id`, `Error`, all already `pub` | `tree_reconcile.rs`; kept beside agentfs-align, since the model uses align and not line maps |
| agentfs-mark | `Waiters`, `OwnerGuard`, `Executor`, `Answer`, `WaitError`, `StopReason` ([waiting marks](waiting-marks.md)), the lock-discipline checks | new, plus `stages.rs:660-749` |
| agentfs-referee | `land` (a `Decision`) and `sync` over snapshots; `Cut`, `Key`, `Upstreams`; the values `EventNumber`, `LineVersion`, `MergeFileIdentity`, `NameHistory`, `Meta` | `referee/`, `queue.rs`, `upstreams.rs`, `meta.rs`, the value half of `tree.rs`. No git2, no `std::fs`: the referee's purity is checked by the compiler |
| agentfs-journal | `Journal` (append, read back, compaction's file side), `Durability` with the durable mark, records (`Entry`, `Change`, `Call`), `FileMap`, `Layout`, `SavedTree`, `fold`, stages | `journal/`, the state half of `tree.rs`, `pointers.rs`, `rows.rs`, `file_map.rs`, `layout.rs`, `stages.rs` |
| agentfs-history | `History` (the built point and `BuiltSnapshot`; the checkpointed and compacted marks), `Builder`, `Compactor`, `Repacker`, `TreeMoved` | `history/`, without `take.rs` and `upstreams.rs` |
| agentfs-tree-engine | `Recording` (the call path), `TreeActor`, `Sessions` and `TreeSession`, `TreeStore`, `TreeStateArea`, operator settings | the rest; the contracts adapters stay here, at the edge |

None merges into agentfs-volume, -clients or -trees; `TreeMoved` moves to agentfs-contracts when M4's mirror consumes it.

**Cheap now: moves along existing seams, checked by the compiler.**
- Plan v1's: `FifoGate` (`failpoints.rs:142-177`) to the engine; `sha256_at` (`confined.rs:29`) beside its one caller,
  `restore.rs:735`; `init_libgit2` (`history.rs:78`), `PACK_CONFIG`, `parse_size` (`config.rs:1904`) to base; the three
  tree observers (`telemetry.rs:648`, `:683`, `:799`) to the engine as functions over a `Meter`.
- `tree_git.rs:69`, `:75`: two counters become `pub`; `tree_reconcile` needs nothing. Optional: `config::validate_name`, `parse_duration`, `shared_fs` with `mountinfo`, `listening_socket`, `allocator`
  (0.8k) to base, so agentfs-csi stops depending on the recording engine.

**Needs a design change first.**
- **One error:** each crate gets its own enum; the engine's `Error` wraps them with `From` and keeps `errno()`, the one
  home of the errno map. **Methods on `TreeActor`:** `commit_point.rs:13` and `verify.rs:748` move up (actor work).
- **`Then`:** replaced by the mark's `Resume` on an `Executor`, the mark PR's own change.
- **Types out of `tree.rs`:** values down to the referee; `Call`, `FileMap`, `Layout`, `Objects` (`fold.rs:17`) and
  `Retired` (`tree.rs:16`) down to the journal. **Inversions:** `stages.rs:517`'s counter moves into stages and the feed
  bumps it; `lag.rs:24`'s constant moves to lag and settings reads it.
- **Tests:** the compactor's and repack's tests build a whole actor (`test_support::new_actor`); they move to the
  engine's tests or get a fake.

**Too entangled to split today.** Recording: three of its files are methods of `TreeActor` and its state is the actor's
fields (`mod.rs:413-426`); Rust allows those methods only in `TreeActor`'s crate. It leaves when it owns its state
(module cut: "actor: composition only"), with #2, saves as a journal follower. The actor lock: `Recording` holds
`Arc<Mutex<TreeActor>>` across recording, journal and landing (`call_path.rs:435-450`); no crate cut changes that, and
the core worker and the mark (2026-11-23) reshape who holds it.

## 3. Crates ranked by gain over effort and risk

| # | Crate | Lines | Effort | Risk | What stops rebuilding when it changes [est] |
|---|---|---|---|---|---|
| 1 | agentfs-base and agentfs-tree-engine (plan v1) | 2.1k, 41k | one session, about 85 moved files | low | a tree edit rebuilds none of the recording engine's 71k; a recording-engine edit rebuilds none of the tree engine or its ~20 test programs |
| 2 | agentfs-git and agentfs-lines | 1.4k, 1.4k | about 2 hours, in PR 2 | low | small: a tree edit stops recompiling 2.8k. Gain is mostly design (adapter and primitive at the lowest layer) |
| 3 | agentfs-mark | about 0.5k | none beyond the mark PR | low | the mark's tests link nothing of the engine; "no lock, thread or number of its own" becomes a compile-time fact |
| 4 | agentfs-journal and agentfs-referee | ~10k, ~4k | 3 to 4 days | medium (the commit point; one `phase-1.yml` dispatch, as module cut 5.3) | an edit above them (52 of 54 commits touched history, recording, the actor or sessions) stops recompiling these 14k; the referee's tests link only pure code |
| 5 | agentfs-history | ~11.5k | 2 to 3 days | medium | an engine-top edit compiles ~13k instead of 41k; a history edit ~24k instead of 41k |
| 6 | recording out of the engine | ~6k | with #2 | high | an actor edit stops recompiling recording |

- **Not recommended:** splitting the recording engine (Q1); a landing crate (1.3k, 3 commits); an adapters crate (0.8k,
  no gain once the engine is split); a stages crate (M5's); a replication crate (M4 re-homes the transports).
- **Caveat:** incremental compilation already reuses unchanged work inside a crate; a boundary saves mostly codegen, the
  frontend's serial pass over the crate, and relinking test programs. PR 1's harness measures how much.

## 4. Staged order

```
PR1 speed (agreed) -> PR2 base, tree-engine, git, lines -> PR3 the mark (agentfs-mark; Then removed)
  -> PR4 journal and referee -> PR5 history, the journal's follower -> #2 saves follow the journal (recording leaves)
  -> M4 replication crate from the recording engine's transports -> M9 deletes the recording engine
```

- **The mark first, in its own crate.** It needs only a record number (generic over the position, or `RecordNo` moves
  in: for the code architects). The journal cannot be cut before it, because `Then` ties durability to the call path.
  PR 2 precedes the mark so its files land in their final crates and its probing gets the faster rebuilds.
- **Gates.** PR 2 moves files only: ci and image, as module cut 5.2. PR 4 and PR 5: one `phase-1.yml` dispatch each.

## 5. Questions for the owner

**Q1 (general guidance): should the recording engine stay whole in agentfs-core until M9 deletes it?** It is 71k of
core's 114k lines. In the last 30 days, 5 of 54 core commits touched it. After PR 2, no tree edit rebuilds it.

| Option | Gives | Costs |
|---|---|---|
| A. Leave it whole (recommended) | no work spent on code that M9 deletes | its own rare edits still rebuild its 71k |
| B. Split it by its domains (push, patch, lfs, fork, history service) | faster builds while recording-engine work goes on | about a week of moves and import rewrites, all deleted at M9 |

The answer sets whether PR 2 touches recording-engine files beyond the shared leaves.
**Q2 (general guidance): which crate do consumers use as "the library", and under what names?** The owner, 2026-10-29
(AGENTS.md principle 10): "There is “the library” of agentfs, and “the filesystem” agentfs, and the kubernetes adapter
layer". AGENTS.md maps the library to agentfs-core, which keeps the recording engine until M9. Other names are welcome.

| Option | Gives | Costs |
|---|---|---|
| A. The names on this page; consumers use agentfs-tree-engine, which re-exports what they need; at M9 it takes the name agentfs-core (recommended) | one integration point now and after M9 | a rename at M9 across every consumer's imports |
| B. agentfs-core becomes the tree engine's re-exporting front now, and the recording engine moves to agentfs-recording | the library keeps its name throughout | moves 71k lines and about 290 imports in fuse, the binary and csi, for code M9 deletes |

The answer sets PR 2's crate names and the `docs/library/` pages.
| New term | Means | Why needed | Closest existing term |
|---|---|---|---|
| mark | a rising record number and its waits ([waiting marks](waiting-marks.md)) | the one waiting primitive | the durable mark (`journal/durable.rs`) |

**For the code architects (parked):** the `failpoints` feature forwarded through seven crates; the test-enforced lists
(`docs/architecture.md` sections, `docs_reference.rs`, the Dockerfile stubs, `failpoints.rs:698`); `phase-1.yml`'s `-p`
names and `alerts.yml`'s path filter; the home of `RecordNo`; each crate's public surface.
