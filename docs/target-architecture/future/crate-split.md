# agentfs-core along domain lines: the crates, and in what order

**Design, not built. The owner answered its questions on 2026-11-24 ([OWNER-GUIDANCE.md](../../../OWNER-GUIDANCE.md),
"Performance"); the front's FUSE question too (Q1: B).** Layer: module (crate boundaries inside system.md's "git model").
Counts read at main 7efd868 (after PR #43's build settings and merged test programs); nothing built or run.
**[read]**: counted in code (`use` lines and paths outside test modules). **[measured]**: `docs/development.md`,
"Build settings".

The owner's words, all general guidance: "Just two? Can we break it down more along domain lines?"; on keeping the
recording engine whole until M9, "ok"; on the name, "consumers should link against something named "agentfs", but im
not sure which crate technically matches that purpose", then, of a thin front crate named `agentfs` (option A below),
"A sounds right".

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

Total about 41k of core's 114k; the recording engine is the other 71k (2.1k of it shared leaves).

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
| durability, history to call path | `journal/durable.rs:18`, `history/built.rs:19`: `Then`, the callback the waiting mark replaces | the call path holds both |
| history and compactor | `built.rs:764` `AtBuilt` | the compactor reads `History` |
| measurement to history | `stages.rs:516-518` reads `feed::fed` and `tree_git`'s counters | everything times through `stages` |

- **Already clean:** `tree_git` and `tree_reconcile` (shared leaves only); `operator_settings` (two small edges,
  `history/lag.rs:24`, `operator_settings.rs:184`); the adapters and the untracked area (only through the actor).
- **About 660 crate-private items** (`pub(super)`, `pub(in crate::tree_actor)`) carry the cycle's traffic: journal 166,
  history 227, recording 101, referee 58, `tree.rs` 110. A crate cut makes each a public-surface choice: design, not sed.
- **Existing domain crates.** agentfs-volume, -clients and -trees own pure lifecycles; the launcher
  (`agentfs/src/launcher/domains.rs`) composes them over core's adapters. No engine module belongs in them: the engine
  is "the git model" (system.md, section 1), which has no contract in agentfs-contracts (fuse calls the concrete
  `Recording`), and its event `TreeMoved` is crate-private (`history/mod.rs:82`).
- **Duplicates.** A fork's upstream has two homes, the Trees row (`ForkFacts.upstream`, `contracts/src/trees.rs:46`) and
  `Upstreams`; "no forks of forks" is checked in both. `stages.rs` mixes measurement with the lock discipline
  (`stages.rs:660-749`).

## 2. The crates (acyclic)

```
            consumers outside the workspace
                         |
                      agentfs            the library's one name: re-exports only
                     /       \
 (feature fuse, Q1) /         \
       agentfs-fuse            \           agentfs-cli (binary `agentfs`)   agentfs-csi   agentfs-conformance
             \                  \               |   (each depends on the crates it uses, never on the front)
              +------------- agentfs-tree-engine ---------------------+
                actor, call path, recording, landing, sessions,        agentfs-core: the recording engine,
                store and state-area adapters, operator settings       telemetry, config, until M9 deletes it
                     |                 \                                         |
                     |            agentfs-history                                |
                     |     builder, built, verify, compactor, repack             |
                     |                 /                                         |
              agentfs-journal: segments, records, durability, file maps,         |
              layouts, saved trees, pointers, rows, fold, stages                 |
               /                |                 \                              |
  agentfs-referee         agentfs-mark          agentfs-git (tree_git)           |
  values, queue and       marks, waits,              |                           |
  locks, referee (pure)   executors             agentfs-base  <------------------+
       |                                        paths fsutil file_lock failpoints git_cli confined init_libgit2
  agentfs-lines (line maps) -> agentfs-align          agentfs-contracts and agentfs-rules sit under every tree crate
```

| Crate | Public surface | Comes from |
|---|---|---|
| agentfs | re-exports: the tree engine's consumer API at its root; `agentfs::fuse` behind a `fuse` feature once Q1 allows it. No code of its own but docs | new, `crates/agentfs` |
| agentfs-cli | the `agentfs` binary, unchanged: its name, subcommands and features | today's `crates/agentfs` package, renamed |
| agentfs-base | what those six leaves expose today, plus `init_libgit2`, `PACK_CONFIG`, `parse_size` | core's shared leaves |
| agentfs-git | `TreeGit` (objects, the ref copy, checkout), its `Error`, two counters | `tree_git.rs` |
| agentfs-lines | `LineMap<V>`, `Image`, `Expanded`, `Line`, `Id`, `Error`, all already `pub` | `tree_reconcile.rs`; beside agentfs-align, since the model uses align and not line maps |
| agentfs-mark | `Waiters`, `OwnerGuard`, `Executor`, `Answer`, `WaitError`, `StopReason` (the waiting-marks design). Not the lock discipline: `InnerLock`, `Unlocked` and `assert_unlocked` (`stages.rs:660-780`) name the actor's work and read stages' holds, so they stay with stages | new |
| agentfs-referee | `land` (a `Decision`) and `sync` over snapshots; `Cut`, `Key`, `Upstreams`; the values `EventNumber`, `LineVersion`, `MergeFileIdentity`, `NameHistory`, `Meta` | `referee/`, `queue.rs`, `upstreams.rs`, `meta.rs`, the value half of `tree.rs`. No git2 dependency, which the compiler checks; no `std::fs`, which the crate's `clippy.toml` (`disallowed_types`, `disallowed_methods`) checks under `-D warnings` |
| agentfs-journal | `Journal` (append, read back, compaction's file side), `Durability` with the durable mark, records (`Entry`, `Change`, `Call`), `FileMap`, `Layout`, `SavedTree`, `fold`, stages | `journal/`, the state half of `tree.rs`, `pointers.rs`, `rows.rs`, `file_map.rs`, `layout.rs`, `stages.rs` |
| agentfs-history | `History` (the built point and `BuiltSnapshot`; the checkpointed and compacted marks), `Builder`, `Compactor`, `Repacker`, `TreeMoved` | `history/`, without `take.rs` and `upstreams.rs` |
| agentfs-tree-engine | `Recording` (the call path), `TreeActor`, `Sessions` and `TreeSession`, `TreeStore`, `TreeStateArea`, operator settings | the rest; the contracts adapters stay here, at the edge |

**The front (design choices).**
- **Why a front, not a renamed engine crate.** Consumers write `agentfs` once, and the internal crates keep names that
  say what they hold (`agentfs-tree-engine` can become `agentfs-engine` once there is one engine) without touching a
  consumer's imports. The front is small: re-exports of the tree engine's consumer API, and docs. Prior art:
  `wasmtime` and `wasmtime-cli`, `gix` over gitoxide's crates, `bevy` over `bevy_*`.
- **Why not one package that is both library and binary.** Cargo sets dependencies per package: library consumers
  would pull clap, the OTLP exporters, tokio and rustls unless every one sat behind a feature.
- **Workspace crates never depend on the front**: it depends on agentfs-fuse, so fuse using it would be a cycle, and
  a workspace crate names the crate that owns what it uses.
- **The recording engine is not re-exported.** M9 deletes it; until then a consumer that needs `volume`, `push` or
  `restore` depends on `agentfs-core` by name. agentfs-fuse still links and exposes it (`boundary.rs:42-43` uses
  `record` and `volume`; `lib.rs` makes `boundary`, `ops` and `overlay` public), which is why Q1 matters. AGENTS.md principle 10's mapping and `docs/library/` change with PR 2:
  the library is `agentfs`, over the tree engine's crates.
- **crates.io.** The name `agentfs` there belongs to an unrelated project. Git and path dependencies are unaffected;
  publishing would need another registry name (no publishing is planned).
- None of the new crates merges into agentfs-volume, -clients or -trees; `TreeMoved` moves to agentfs-contracts when
  M4's mirror consumes it.

**Cheap now: moves along existing seams, checked by the compiler.**
- To base: `init_libgit2` (`history.rs:78`), `PACK_CONFIG`, `parse_size` (`config.rs:1904`). To the engine: `FifoGate`
  (`failpoints.rs:142-177`) and the three tree observers (`telemetry.rs:648`, `:683`, `:799`) as functions over a
  `Meter`. `sha256_at` (`confined.rs:29`) beside its one caller, `restore.rs:735`.
- `tree_git.rs:69`, `:75`: two counters become `pub`; `tree_reconcile` needs nothing. Optional: `config::validate_name`,
  `parse_duration`, `shared_fs` with `mountinfo`, `listening_socket`, `allocator` (0.8k) to base. This does not free
  agentfs-csi from the recording engine: it depends on agentfs-fuse, which links it, until M9.
- The CLI's rename: about 85 references (`-p agentfs` and `--features agentfs/...` in workflows, the Dockerfile,
  `ci/*.sh`, README, docs). `CARGO_BIN_EXE_agentfs` and the installed binary keep their name. **Order inside PR 2:**
  the rename lands in a commit where no package named `agentfs` exists, so a missed `-p agentfs` fails CI instead of
  quietly building the front (and `ci/check-deps.sh` checking the front's tree); the front comes in a later commit.
- `tree_actor` stays one module inside agentfs-tree-engine, so its ~660 `pub(in crate::tree_actor)` paths compile
  unchanged. That is what makes PR 2 moves only.
- **Workspace-wide tests leave core,** which M9 deletes: `docs_reference.rs` (it lists every crate and reads the
  Dockerfile) and the source scan at `failpoints.rs:698` move to agentfs-cli's tests, the crate at the top that M9 keeps.

**Needs a design change first.**
- **One error:** each crate gets its own enum; the engine's `Error` wraps them with `From` and keeps `errno()`, the one
  home of the errno map. **Methods on `TreeActor`:** `commit_point.rs:13` and `verify.rs:748` move up (actor work).
- **`Then`:** replaced by the mark's `Resume` on an `Executor`, the waiting-marks PR's own change.
- **Types out of `tree.rs`:** values down to the referee; `Call`, `FileMap`, `Layout`, `Objects` (`fold.rs:17`) and
  `Retired` (`tree.rs:16`) down to the journal. **Inversions:** `stages.rs:517`'s counter moves into stages and the feed
  bumps it; `lag.rs:24`'s constant moves to lag and settings reads it.
- **The referee's two upward edges:** `queue::held` takes `&Shape` (`queue.rs:7`, `:50`), from the state half of
  `tree.rs` bound for the journal; `Upstreams::merges` calls `refs::branch` (`upstreams.rs:69`), in history. `held`
  moves up to its caller, and ref naming moves down beside the values or `merges` moves up.
- **Tests:** the compactor's and repack's tests build a whole actor (`test_support::new_actor`); they move to the
  engine's tests or get a fake.

**Too entangled to split today.** Recording: three of its files are methods of `TreeActor` and its state is the actor's
fields (`mod.rs:413-426`); Rust allows those methods only in `TreeActor`'s crate. It leaves when it owns its state
(module cut: "actor: composition only"), with #2, saves as a journal follower. The actor lock: `Recording` holds
`Arc<Mutex<TreeActor>>` across recording, journal and landing (`call_path.rs:435-450`); no crate cut changes that.

## 3. What each crate gains

PR #43 cut most of the rebuild time [measured]: one edit in `tree_actor/recording/` or `volume.rs`, then
`cargo test -p agentfs-core --no-run`, takes 9 s (27 s before). A crate boundary now saves little edit-to-test time;
the gains are mostly the compiler checking the layering, and a consumer building only the engine it uses.

**Test-run time is not this design's to cut.** The owner's goal (2026-11-24: agents spend about 30% of their time in
cargo test) is now mostly the run: `cargo test -p agentfs-core` runs 218 s against a 9 s rebuild, and two programs,
`tree_batch_alloc` (71 s) and `tree_stages` (about 60 s), are most of it. They run on every tree edit before and after
every PR here. What would deliver it (cheaper fixtures for those two, or running them in CI only) is a separate
piece of work, not a crate boundary; PR 2 reports each crate's run time beside its build time.

| # | Crates | Lines | Effort | Risk | Gain |
|---|---|---|---|---|---|
| 1 | agentfs, agentfs-cli, agentfs-base, agentfs-tree-engine, agentfs-git, agentfs-lines (PR 2) | 0.1k, rename, 2.1k, 38k, 1.4k, 1.4k | one session, about 90 moved files and 85 renamed references | low | consumers' one name; neither engine links the other's code; core's tree tests (about 20 programs' worth, now merged) leave core |
| 2 | agentfs-mark | about 0.5k | none beyond the waiting-marks PR | low | depends on nothing in the engine, which the compiler checks; its tests link none of it. Fails the deletion test if that is all: it may stay in agentfs-journal (for the waiting-marks code architect) |
| 3 | agentfs-journal and agentfs-referee | ~10k, ~4k | 4 to 5 days (the referee's two upward edges) | medium (the commit point; one `phase-1.yml` dispatch, as module cut 5.3; without release `lto`, calls across the new boundary on the hot path are no longer inlined unless generic or `#[inline]`, so PR 4 re-runs the release figures) | the referee's tests link only pure code; journal and referee stop rebuilding on edits above them |
| 4 | agentfs-history | ~11.5k | 2 to 3 days | medium | history behind its own surface |
| 5 | recording out of the engine | ~6k | with #2 | high | an actor edit stops recompiling recording |

PR 2 re-runs the "Build settings" table, so a figure claimed is a figure measured.

- **Not delivered here:** the two duplicates of section 1 (a fork's upstream in two homes; "no forks of forks"
  checked twice). They cross the Trees domain and the engine, not a crate line; they are their own work item.
- **Not done:** splitting the recording engine (owner: "ok" to keeping it whole until M9); a landing crate (1.3k,
  3 commits); an adapters crate (0.8k); a stages crate (M5's); a replication crate (M4 re-homes the transports).

## 4. Order

```
PR 1 speed (#43, merged) -> PR 2 the front, the CLI rename, base, tree-engine, git, lines
  -> waiting-marks PR (agentfs-mark; Then removed) -> PR 4 journal and referee -> PR 5 history, the journal's follower
  -> #2 saves follow the journal (recording leaves) -> M4 replication from the recording engine's transports
  -> M9 deletes the recording engine and agentfs-core
```

- **PR 2 first.** It moves files only, so the mark's files land in their final crates. Gate: ci and image, as module
  cut 5.2.
- **The mark before the journal.** `Then` ties durability to the call path; the journal cannot be cut until the mark
  replaces it. PR 4 and PR 5: one `phase-1.yml` dispatch each.

## 5. The front and the filesystem layer (answered: B)

**Q1 (general guidance): does `agentfs` also front the filesystem layer?** The owner, 2026-10-29 (AGENTS.md principle
10): "There is “the library” of agentfs, and “the filesystem” agentfs, and the kubernetes adapter layer".

| Option | Gives | Costs |
|---|---|---|
| A. `agentfs::fuse` behind a `fuse` feature now | one name for both layers a consumer links; the default build has no FUSE | agentfs-fuse links and exposes the recording engine (`boundary.rs:42-43`), so with the feature on the front re-exports `record` and `volume` types and links agentfs-core, until M9 |
| B. The library only until M9, then the `fuse` feature (chosen) | the front never carries the engine M9 deletes | a consumer that mounts names agentfs-fuse until M9 |
| C. As A, after splitting agentfs-fuse's recording-engine front out first | A without the recording engine | moves in agentfs-fuse for code M9 deletes |

The CSI driver stays its own crate either way: it is deployed, not linked. The answer sets the front's features.

| New term | Means | Why needed | Closest existing term |
|---|---|---|---|
| front | a crate of re-exports only, giving consumers one name over internal crates | the owner's "agentfs" | the library (principle 10) |
| mark | a rising record number and its waits (the waiting-marks design) | the one waiting primitive | the durable mark (`journal/durable.rs`) |

**For the code architect of PR 2 (parked):** the front's exact re-exports; the `failpoints` feature forwarded through
the crates; the test-enforced lists (`docs/architecture.md` sections, `docs_reference.rs`, the Dockerfile stubs,
`failpoints.rs:698`); `phase-1.yml`'s `-p` names and `alerts.yml`'s path filter; `ci/check-deps.sh`; the home of
`RecordNo`; each crate's public surface.
