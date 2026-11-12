# Module-cut step 6: transactions follow the normal rules, and the mirror leaves `World`

Design only. Nothing here is built. Read on `main` at 22d55f9. Layer: interfaces and implementation, inside
`docs/target-architecture/future/module-cut.md` (the frame) and `docs/target-architecture/future/transactions.md`.
Revised after review and the cross-step critic (section 12).

**Decision first.** Three slices, each green alone, each with its own model gate:

| # | Slice | Waits for | Digest |
|---|---|---|---|
| S1 (tracer) | The mirror leaves `World`: tree history publishes `TreeMoved`, a `Mirror` reads them, a `HistoryService` composes the two. Push timing gets a check. The model refuses the root name `HEAD` (step 5b's hand-off) | nothing | **Unchanged on all 2620 runs** (no re-baseline). One new corpus row |
| S2 | R10.5 (and R10.8 at a commit, if question 2 says so): the referee can no longer tell a landing from a transaction's commit. Finding 30's test runs, asserting R10.5 | **owner question 2** | Re-baselined on purpose: every changed run first differs at a transaction's commit |
| S3 | R9.18: a file open inside a transaction holds back takes into its fork | **owner question 1** | Re-baselined on purpose: every changed run contains a transaction |

Why this order: S1 is a pure move, proven by an unchanged digest while the golden still pins today's outcomes. S2
and S3 each wait for one answer and are re-baselined alone, so every changed digest traces to one rule.

## 1. Requirements and guidance

**Requirements**: none bears on this step's behaviour. The docs requirement applies (owner, 2026-11-09, asked as an
absolute requirement: "Yes, this is a good idea to record: docs should be written towards future readers").

**Guidance** (quoted, dated; revisable):

| ID | His words | Applies to |
|---|---|---|
| G1 | 2026-11-08, on a file open in a transaction: "Yes. “Waiting longer” is a feature of locks and transactions, not a bug" | R9.18 (S3) |
| G2 | 2026-11-08: "If the file is not open, then it follows the normal merge rules" | R10.11 (S2): files **not open** only |
| G3 | 2026-11-05, finding 30: "A" (ledger R10.5: the transaction's line collides at commit, the fork's line wins, the program is told) | R10.5 (S2) |
| G4 | 2026-11-05: "we can’t control the order that remotes synchronize ... Our rules are about them sharing the same reconciliation rules" (R3f) | design rule 1 |
| G5 | 2026-11-08: "GitHub is something closer to a peer than an upstream"; "a different model for mirror repos" | S1 |
| G6 | 2026-11-03: "yes, "losing" patches should still be part of the history. i want to know who changed what when" | S1's push timing |
| G7 | 2026-11-05, R5.3: a deferred call and a sync, "the sync waits until the transaction ends" (his pick) | unchanged by S3 |
| G8 | 2026-11-11, light and full gate tiers (VERIFYING.md) | gates |

**Found while reading** (spec problems, reported, not changed quietly):
- **R10.11 is tagged `owner`, but its R10.8 half is the agent's reading** (the ledger says so; R10.5's last
  sentence repeats it), and nothing says what a file still open at commit gets. Question 2; S2 waits for it.
- MODEL.md finding 30 still says "Open"; R10.5 records his "A". Its test asserts `TxnOutcome::Committed`, which
  R10.5 contradicts, so S2 changes the assertion, not only the `#[ignore]`.
- Step 5b hands step 6 "the model refuses `HEAD`" (probe 5); S1 takes it in `TreeNames::new`, the one name check.

## 2. Canonical shape

The domain model does not change. A mirror is outside the tree, and tree history publishes `TreeMoved`; both are
the frame's terms (module-cut.md, "The shape of the tree" and the Observer row; system.md's `TreeMoved{tree, from,
to, paths, collisions}`, less `paths`, which the model has none of). It is a model type, not a telemetry event:
recorder source B feeds `calls::Sink` calls, and the replayed model's `set_head` publishes the moves, so the engine
needs no move event; if core's telemetry later defines one, its name lives there and this type keeps its fields. A
transaction's commit is a pair: the transaction as fork, the fork as upstream.

| Pattern | Canonical source | Where |
|---|---|---|
| Observer / Domain Event | GoF Observer; Fowler, "Domain Event" | `World` publishes `TreeMoved`; `Mirror` reads them |
| Append-only log, consumer position | Kafka consumer offsets; EventStoreDB catch-up subscription | `World::moves()`; `Mirror` keeps how far it has read |
| Composition Root | Seemann, *Dependency Injection* | `HistoryService` composes `World` and `Mirror`, computes nothing |
| Make illegal states unrepresentable | Minsky, "Effective ML" | The referee has no `Direction` input; `set_head` takes the refs `record` returns |
| Mutation testing | DeMillo, Lipton, Sayward (1978) | Corpus rows per slice |

**New terms**

| Term | Means | Why needed | Existing term it could be |
|---|---|---|---|
| `HistoryService` (model) | The model's history service: the trees (`World`) and one mirror, composed, with the step list over both | Something above `World` must own the mirror and schedule its push | "history service" (system.md: one per volume, composes the domains). Same role; it composes only the two the model has. Not "Volume": that domain is lifecycle and release, and excludes remotes (domains/volume.md) |
| `Mirror` (model) | One remote copy of the root's branch and every collision ref, pushed by its own step | The shared remote leaves `World` | "mirror" (module-cut.md); "the shared remote" (git-model §4). Same meaning |
| `TreeStep` | `World`'s own steps: `Next`, `Drop` | `Step` now includes the mirror's push | today's `world::Step` without `Replicate` |
| `Step::Push` | The mirror's push, one scheduler step | Replaces `Step::Replicate` | "Replication's push" |
| due | The mirror has an unread move of the root, or one naming a collision ref | Was a `World` flag | `replication_due` |

## 3. Design rules (design choices)

1. **The referee cannot tell a landing from a transaction's commit** (with answer A to question 2). `referee::land`
   takes no `Direction`, so R3f holds by construction and `if dir == Direction::Landing` cannot be written in the
   referee again. With answer B it keeps one gate, on R10.8 only, and this rule is dropped (section 11).
2. **Tree history is the only publisher of moves, and a take's collisions travel by type.** `set_head`, the one
   place a head moves, appends one `TreeMoved`, the initial root commit in `World::new` included. `record` returns
   the collision refs it recorded (`#[must_use]`) and `set_head` takes them by value: dropping them is an unused
   value or variable, which `clippy -D warnings` refuses. A mirror's due state is its position in that log.
3. **`World` names no mirror, and a world with none is a whole system** (principle 6). Conformance and the phase-1
   oracle run `World` alone. `HistoryService::pending` lists `World`'s steps in their order, then `Push`, exactly
   where `Replicate` sits today, so random runs draw from the same list. **No method name is on both `World` and
   `HistoryService`**, so code holding a `&World` cannot call the service's `settle` by mistake: it does not compile.

## 4. Interfaces (frozen per slice; bodies are the implementer's)

### S1: `crates/agentfs-model/src/world.rs` (changed)

```rust
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TreeStep { Next(TreeId), Drop(TreeId) }
/// Tree history's notice that a tree's head moved (system.md's `TreeMoved`, less `paths`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeMoved {
    pub tree: TreeId,
    pub from: Option<String>,        // None for a tree's first head
    pub to: String,
    pub collisions: Vec<String>,     // the refs (`refs/collisions/<n>`) the take that moved it recorded, in order
}
/// `TreeNames::new` also panics on a name `git check-ref-format --branch` refuses (`HEAD`): `RootName`'s rule.
impl World {
    pub fn moves(&self) -> &[TreeMoved];                    // append-only, oldest first
    pub fn tree_steps(&self) -> Vec<TreeStep>;              // was `pending`; Replicate gone
    pub fn take_step(&mut self, s: &TreeStep) -> usize;     // was `step`
    pub fn settle_trees(&mut self);                         // was `settle`; never pushes
    pub fn note_step(&mut self, s: &dyn std::fmt::Debug);   // one logger for both step types
    /// The run's name for its trace and digest files: computed at its first call (lazily, in a
    /// `OnceCell`), then kept. Both writers call it only at drop, after `label_trace`.
    pub fn run_name(&self) -> String;
    #[must_use] fn record(&mut self, /* as today */) -> Vec<String>;
    fn set_head(&mut self, t: TreeId, c: &str, collisions: Vec<String>);
}
// Removed: `remote`, `replication_due`, `replicate`, `Step::Replicate`, `pending`, `step`, `settle`, the bare
// repository. `World::drop` writes the trace only.
```

### S1: `crates/agentfs-model/src/mirror.rs` and `service.rs` (new)

```rust
//! A mirror: a remote copy of the root's history, outside the tree (Replication's domain page).
pub struct Mirror { /* dir, repo, root branch, read: usize */ }
impl Mirror {
    pub fn new(names: &TreeNames) -> Self;                 // empty bare repo in its own temp dir
    pub fn repo(&self) -> &Path;
    /// A move it has not read moved the root or names a collision ref.
    pub fn due(&self, moves: &[TreeMoved]) -> bool;
    /// One atomic push from `from`, never forced: the root's branch and every collision ref the unread
    /// moves name; then all of `moves` count as read. Panics on a git failure, as the model's `git` does.
    pub fn push(&mut self, from: &Path, moves: &[TreeMoved]);
}
impl Drop for Mirror { /* removes its directory unless AGENTFS_MODEL_KEEP */ }

// service.rs (new): the model's composition root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step { Next(TreeId), Drop(TreeId), Push }
impl From<TreeStep> for Step;
pub struct HistoryService { world: World, mirror: Mirror }   // field order: world first
impl HistoryService {
    pub fn new(policies: Policies, names: TreeNames) -> Self;
    pub fn mirror(&self) -> &Mirror;
    pub fn pending(&self) -> Vec<Step>;          // tree_steps in order, then Push when due
    pub fn step(&mut self, s: &Step) -> usize;   // Next/Drop to the world, Push to the mirror (0)
    pub fn settle(&mut self);
}
impl Deref for HistoryService { type Target = World; }  impl DerefMut for HistoryService;
impl Drop for HistoryService { /* digest under world.run_name(), before the fields drop */ }
```

### S1: `check.rs`, `digest.rs` (changed)

```rust
pub struct Checked { service: HistoryService, checker: Checker }   // Deref<Target = HistoryService>
pub fn step_checked(&mut self, s: &service::Step) -> Result<usize, String>;  // after a Push: mirror lacks nothing
fn drained(s: &HistoryService) -> Result<(), String>;               // no Next or Push pending
fn losers_pushed(w: &World, m: &Mirror) -> Result<(), String>;      // reads m.repo()
/// Truth from git (`for-each-ref`, both sides): if the mirror lacks the local root's tip or a
/// collision ref, the service must list `Push`. It judges the service's claim; it does not ask `Mirror::due`.
fn mirror_current(s: &HistoryService) -> Result<(), String>;
// digest.rs: text unchanged; `canonical_text`, `digest`, `write` take `&HistoryService`.
```

### S2: referee and world (changed; answer A to question 2)

```rust
// referee/mod.rs: no `Direction` in referee/. `Lines` loses `dir`.
pub(crate) fn land<L: Lineage + ?Sized>(git: &L, ours: &str, incoming: &[Incoming], tip: &str) -> Plan;
pub(super) fn undecided(incoming: &'i [Incoming]) -> Lines<'i>;          // referee/undecided.rs
// referee/placing.rs: `joins` runs at every take. world.rs: `Direction` stays for `Landed`, `Collision`, checks.
fn resolve(&self, ours: &str, incoming: &[Incoming], tip: &str) -> Merged;
// With answer B, `land` keeps `dir: Direction` and `joins` gates only its R10.8 clause.
```

### S3: recording and world (changed; answer A to question 1)

```rust
// recording.rs: the opens of one target (a tree, or a fork's transaction), read at `at`, its head.
pub(crate) fn open_files(&self, target: Target, at: &str, git: &impl Files) -> BTreeSet<String>;
// world.rs: opens for writing in `t` or in `t`'s open transaction (R9.18), each read in its own target
// with its name there, plus what deferred calls will save. `queue::Keys` is unchanged.
pub fn write_open(&self, t: TreeId) -> BTreeSet<String>;
```

With answer B, `Keys` gains `fn sync_open(&self, t: TreeId) -> BTreeSet<String>`, asked only by P2's prefix cut
and a sync's take. Either way `txn_files` and finding 34 (R5.3's deferral) are untouched.

**Module map after step 6** (`src/`): today's modules, plus `mirror.rs` and `service.rs` (the composition root);
`world.rs` publishes `TreeMoved` and holds no remote; `Checked` wraps `HistoryService`.

## 5. What moves where, and every crossing

| Rule or duty | Before | After | Crossing |
|---|---|---|---|
| The shared remote, its push, "due" | `World` | `Mirror` | `World::moves()` (read-only); the service passes `world.repo()` to `push` |
| Scheduling the push | `World::pending` | `HistoryService::pending` (last) | none new |
| Digest's mirror refs (`P` lines), the digest file | `World` | `HistoryService` | `digest::*(&HistoryService)` |
| Root name check | `TreeNames::new` (reserved names) | the same, plus git's branch rule | none |
| R10.4 at a commit (R10.5); R10.8 at a commit if Q2 says A | gated off in `joins` | every take | `land` loses `Direction` |
| R10.4 in the checker | `placed_and_unlinked`, landings only | every take | none |
| R9.18 | not built | `World::write_open` includes the transaction's opens | `Recording::open_files(Target, ..)` |

**Push timing, settled (a design choice, as git-model §4 says).** Due when the root moves and when a take in any tree
records a collision; pushed in its own step, never inside a take (principle 5). A fork's collision ref must reach the
mirror even if that fork never lands (G6; git-model §4). No check sees this today (every run drains forks into the
root before `losers_pushed`); `mirror_current` checks it after every step. No rule's meaning changes.

## 6. Risks, and how the design avoids each

| Risk | Avoidance |
|---|---|
| S1 changes the order of random draws (`rng.pick` over `pending`) | `Push` is last when due, as `Replicate` was. Due is set by the same events: every `set_head` (root) and every recorded collision, paired by type (rule 2). The initial root commit is a move, so the mirror is due before the first step, as `replication_due` is today (world.rs:560→868). Proven by `check --all` on 2620 runs |
| S1 shifts digest keys. Today `run_name` runs once per drop (world.rs:2694), counting per call; the split adds a second caller | `World::run_name` computes once, at its first call, and keeps it. Each writer calls it only at drop, under today's conditions (not panicking; its directory set), so each world counts once, after `label_trace` (world.rs:2300), as today |
| A World-alone run stops writing a digest; the digest reads a deleted mirror | None is in the golden (`ci/model-digest.sh` collects only `scenarios` and `interleavings`, which build `Checked`). `HistoryService::drop`'s body runs before its fields drop |
| The push carries named refs, not `refs/collisions/*` | The same set: every collision ref is made in `decide` and returned by `record` within one step. A ref the event misses leaves the mirror lagging, which `mirror_current` reports |
| Corpus: two clean-lines mutants use `lines.dir`; `line_follows_the_upstreams_unfinished_last_line` goes STALE | S2 retires `losing_save_applies_its_clean_lines` and `losing_transaction_applies_its_clean_lines` (their meaning, one direction, no longer exists) and adds `losing_change_applies_its_clean_lines` (R3e) in the referee's whole unit, which owns R3e. The STALE row is re-anchored; its meaning is unchanged |
| R10.5 has no one-site mutant once the referee is blind to direction | Rule 1 makes the class impossible. Finding 30's test (now asserting R10.5) and `no_joined_lines` judge it; a hand probe re-adding the gate is recorded in the PR |
| No check sees R10.8 in random runs | S2 adds `no_joined_lines`: after every step, in every tree and open transaction, only a file's last live line may lack `\n`, read from git. Plus `unfinished_line_lands_before_a_kept_line` (R10.8). If it fires on a seed without a transaction commit, stop and report a finding; never loosen it |
| R9.18's checker agrees with a bug in `write_open` | S3 extends the harness's own reading (`interleavings.rs` `open_before`, from the oracle's object table) to the transaction's opens at its head, and adds `oracle_locks`. Corpus bug `transaction_open_holds_nothing` |
| Rule slices lower tallies or move `random_interleavings_git_merge_base`'s count (15 of 64) | A tripped floor is a generator finding, reported, not tuned; a moved count is stated in the PR |
| A re-baseline hides an unrelated change | For **every** changed run, a script diffs old and new canonical text (`AGENTFS_MODEL_DIGEST_TEXT=1`; old texts from one run of the base commit on the changed units only): the first differing commit must be a `Transaction t` commit (S2), or a sync into a fork whose transaction holds a file open (S3). Runs without a transaction must stay byte-identical. Residual: a second, unrelated change behind the first difference in the same run is not seen |
| Engine or recording (principle 1) | None. Model only; phase-1 traces have no transactions |

## 7. Costs

- **S1 adds two types and a Deref chain** (`Checked → HistoryService → World`), safe only while no method name is
  on two of them: a convention nothing tests. World-alone callers rename (`phase1-oracle.rs` ×4, `check.rs:448`,
  `digest.rs`'s unit test, which needs the service for its `P` lines). One more temp directory per run (unmeasured).
- **Alternative: `Checked { world, mirror, checker }`, no service.** One type and one Deref level fewer. Declined:
  the checker would schedule the system's steps and write its digest, inside the module the frame keeps independent
  (module-cut design rule 3), and the digest unit test would need a checker.
- **S2** loses whole some commits that commit today (finding 30's; with A, R10.8's too). **S3** with A makes a nested
  fork's landings wait on a transaction's open file. Deferred: shadowing, `paths` on `TreeMoved`, finding 34.

## 8. Slices and gates

Every slice changes `src/**` types or conditions, so the light tier's test fails: each runs the **full model gate**
through `ci/model-docker.sh` (step 1: digest about 627 s, corpus about 235 s). The step ends with one adversarial
review, one fix round, green **ci** (~5 min) and **image** (~5–9 min), and one **phase-1.yml dispatch**
(`phase1-oracle.rs` changes; duration unmeasured). No tree-view CI: no engine code changes.

| Slice | Files | Done when |
|---|---|---|
| **S1 tracer** | new `src/mirror.rs`, `src/service.rs`; `lib.rs`, `world.rs`, `check.rs`, `digest.rs`, `bin/phase1-oracle.rs`; `tests/scenarios.rs` (`Step` import, `Push`, `w.mirror().repo()` at :376-379, :509, :2267), `tests/interleavings.rs` (:560, imports), `tests/gitledger/mod.rs`; `digest.rs` test: `HEAD` refused; corpus + `mirror_ignores_a_forks_collision` (R4) | `check --all` equal on 2620/2620; corpus reproduces the matrix plus the new row, caught by `mirror_current`; anchors clean |
| **S2** (after Q2) | `src/referee/{mod,undecided,placing}.rs`, `world.rs` (`resolve`), `check.rs` (`placed_and_unlinked` at every take, `no_joined_lines`); `tests/scenarios.rs`: finding 30's test un-ignored, renamed `r10_a_transactions_line_after_the_forks_unfinished_line_loses_whole`, asserting `TxnOutcome::Lost` naming `n9`'s line and the fork keeping `}`. Corpus: −2, +2, 1 re-anchor | Golden and matrix re-recorded in one commit, the bound of section 6 shown for every changed run |
| **S3** (after Q1) | `src/recording.rs`, `world.rs`, `tests/interleavings.rs` (`open_before`, `oracle_locks`), `tests/scenarios.rs` (`r9_a_file_open_in_a_transaction_holds_back_the_forks_sync`; with A also `..._a_landing_into_its_fork`); corpus +1 | As S2 |

## 9. Docs (each with its slice)

- **MODEL.md**: rules 5 and 9; the Replication check row (mirror, `Push`); rows for `mirror_current`, `no_joined_lines`,
  `oracle_locks`; finding 30 resolved. **VERIFYING.md**: finding 30 leaves the ignored table; "shared remote" → mirror.
- **Rules ledger**: R9.18, R10.5 and R10.11 lose "Not built" and gain tests; R10.11's text and tag follow his answer
  to question 2 (R10.5's last sentence with it); R9.18's "syncs" follows question 1.
- **git-model.md** §4 (`Step::Replicate` becomes `Push`), §9, §15; **domains/replication.md**; **transactions.md**: its
  R9.18 and R10.11 "What is open" bullets go (the page stays: the engine runs no transactions).
- **module-cut.md**: row 6 becomes "Built. Model: R9.18, R10.5, R10.11; the mirror leaves `World` (`Mirror`, composed
  by `HistoryService`, pushed in its own step when the root moves or a take records a collision); the model's
  `TreeNames::new` refuses `HEAD`, as the engine's `RootName` does" (5b adds that clause to the open row; if 6 lands
  first, 6 writes it). Line 37 present tense; lines 130-131 lose replication. Design-rule-3 table (86-92), for the
  later checker step: new row "Pending steps | `World::tree_steps`, `HistoryService::pending` | `drained`, the queue
  step's check, `mirror_current`" (missing today); `write_open` covers a transaction's opens (S3, judged by
  `oracle_locks` against the harness's opens); "Each tree's head" adds `no_joined_lines`; the mirror is read from git.
- **world.rs** module doc; `OWNER-GUIDANCE.md` (his answers, dated). No design page is deleted: module-cut.md stays
  for its two later rows; the step building the last deletes it (R1).

## 10. Probes (real output; scratch copy `probe-6`, its target directory deleted after)

1. **The referee without `Direction` type-checks**: `cargo check --locked -p agentfs-model --all-targets` →
   `Finished ... in 5.48s`, exit 0; `cargo clippy --all-targets -- -D warnings` → `Finished`, exit 0.
2. **Corpus impact of S2** (anchor rule: exactly once across `src/**`): `REPLACE-USES-DIR losing_save_applies_its_clean_lines`,
   `REPLACE-USES-DIR losing_transaction_applies_its_clean_lines`, `STALE line_follows_the_upstreams_unfinished_last_line 0`.
3. **S3's `write_open` over both targets compiles, clippy-clean** with `open_files(target: Target, ..)`: `Finished`.
4. **Draw order and due** (read): `pending()` pushes `Replicate` last (world.rs:1967); `random` draws
   `rng.pick(&pending_of(w, g))` (interleavings.rs:1655); due is set in `set_head` for the root (:868, first at
   :560) and per collision in `record` (:1146); `record` precedes `set_head` at :1713→:1723 and :2166→:2169.
5. **`HEAD` as a root name** (git 2.43.0): `git check-ref-format --branch HEAD` → exit 128; `git init -q -b HEAD` →
   exit 0, `symbolic-ref HEAD` → `refs/heads/HEAD`; then `git commit` → `fatal: ambiguous object name: 'HEAD'`.

**NOT RUN**: any model run (`ci/model-digest.sh`, `planted/run.py`, `agentfs-model` tests), so S1's digest equality,
every corpus outcome, `no_joined_lines` on today's seeds and which checks catch the new mutants are unverified. Not
prototyped: `HistoryService`, `Mirror`, `TreeMoved`, `record`→`set_head`, the new mutant. Nor conformance, CI, docker.

## 11. Owner questions

**1. Does a file open inside a transaction hold back every take into its fork, or only syncs?** *General guidance.*

Background: you answered that it holds back the fork's **syncs** (R9.18, "Waiting longer is a feature"). A fork also
receives **landings** from its own forks, and R9.1 says a landing locks its files "the same way" a sync does. Your
words do not say whether an open inside a transaction counts for landings. Today the model counts neither. Your
pick that a sync waits for a transaction with deferred calls (R5.3) is unchanged either way.

```text
        upstream ──sync──▶  fork F  ◀──landing── fork G (F's own fork)
                              │
                       transaction T: a program has file X open
       A: both arrows wait for X to close     B: only the sync waits
```

| Option | Gives | Costs | What you would see |
|---|---|---|---|
| **A (recommended)**: every take into F waits | One meaning of "open" in R9, R9.1 and R9.18 | G's landings into F wait as long as T keeps X open | A sub-sandbox's work lands later while a transaction holds that file |
| B: syncs only | The narrowest reading of your words | A second lock question in the queue; a landing can write X under T's program (what its later close then does is unprobed) | Landings never wait on a transaction's files |

Binding sentence (guidance unless you call it a rule): "A file open inside a transaction holds back every take into its fork until it closes."

**2. Does a transaction's commit follow every merge rule a landing follows, for every file it carries?** *General guidance.*

Background: a commit puts a transaction's saved changes into its fork. You answered that a line the transaction
added after the fork's unfinished last line collides (R10.5, "A"), and that a file not open "follows the normal
merge rules". The ledger then applied one more rule, R10.8, at commits: that was our reading, not your words. And
nothing says what a file still open at the commit gets (it saves again at its close, R5.2).

```text
  begin:  x⏎ w⏎         transaction saves:  x⏎ y   (w rewritten as y, no newline)
  meanwhile a sync brings the fork:  x⏎ w⏎ z⏎   (z added after w)
  A: y collides with z; the fork wins; the transaction loses whole and is told
  B: the commit can write "x⏎ yz⏎": one line of bytes nobody wrote (as finding 32's "n6n5"; unprobed at a commit)
```

| Option | Gives | Costs | What you would see |
|---|---|---|---|
| **A (recommended)**: yes, every rule, open or not | One rule set for every take (R3f); no file shows bytes nobody wrote | Some commits that commit today lose whole | A transaction told it lost, naming its line |
| B: only R10.4 (your R10.5) at commits | Your words, read narrowly | Commits can join two lines (the mechanism of finding 30's ignored test, "two lines read as one"; unprobed for R10.8). The model's `land` keeps a direction the engine's (no transactions) lacks, so the "one decomposition" module-cut promises splits there, and engine transactions must later copy the gate | Rare joined lines after a commit |

Binding sentence: "A transaction's commit follows every merge rule a landing follows, for every file, open or not."

## 12. Review disposition

**Review**, **changed**: 1 R10.11 (Q2; S2 waits). 2 `HEAD` in `TreeNames::new`. 3 `HistoryService`; `Mirror` tabled.
4 refs paired by type. 5 no shared method names; costs, alternative. 6 `run_name` lazy; initial move published. 7 Q1
on R9.1. 8 mutants retired, one added. 9 every changed run bounded. 10, 11 `from`; module-cut :37, :130. S git judges
`pending` (**changed**); stop and report (**kept**). P naming, guards, error modes: **parked** for the implementer.

**Critic**: 2 **changed**: section 9 gives row 6's built text with the `HEAD` refusal. 7 **changed**: section 9 updates
the design-rule-3 table. 8 **refuted, one sentence** (section 2): source B feeds calls, not moves. 11 **cost stated**
in question 2, option B. The rest concern other steps.
