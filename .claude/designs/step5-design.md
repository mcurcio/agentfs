# Module cut, step 5: the engine's rules once, a typed version, then the split (rev. 3)

Design only, inside `docs/target-architecture/future/module-cut.md`, after `step5b-design.md`. Lines, probes and
goldens are 22d55f9's (pre-5b): the implementer re-runs probes 1-5 on main after 5b merges, before slice 1. No rule,
model file, conformance check or public `Volume` signature changes.

**Decision first.** **5b, then 5.** Three PRs (step 3's precedent), four slices, each a pushed commit green alone.
PR 5.1: (1) typed versions, `tree_reconcile` generic over the caller's version; (2) each rule once, the referee's
units in the frame's order, proved equal to the old code by a differential unit test. PR 5.2: (3) verbatim moves.
PR 5.3: (4) the modules take their own state. **No owner question**: every choice is mine and revisable.

## 1. Requirements, guidance, proposed

| # | Statement | Kind | Source |
|---|---|---|---|
| R1 | "a design doc exists only until its slice is built, and the commit that lands the slice deletes it" | **Requirement** (asked as absolute; "Yes, this is a good idea to record") | OWNER-GUIDANCE, 2026-11-09 |
| G1 | Six modules, the actor only composing (option "B") | Guidance | 2026-11-08 |
| G2 | "I generally approve. I want each phase of the work to start with an architect": module-cut.md, whose step-5 gate is 0 mismatched traces and "No rule changes" | Guidance | 2026-11-09 |
| G3 | "refs/heads/main should not be special or hardcoded. Consumers will configure the name of the root branch." | Guidance | 2026-11-11 |
| G4 | "drop support. we are pre-production and there is no need to worry about "supporting" old concepts" | Guidance | 2026-11-06 |
| G5 | "i would like for the git history to remain compatible across minor versions" | Guidance | 2026-11-02 |
| G6 | "2 PRs is fine"; "New pr is fine" (step 3) | Guidance | 2026-11-11 |
| P | All else; R1 deletes no page here (`module-cut.md` serves steps 6 on) | **Proposed** | this page |

## 2. Canonical shape

**Domain concepts touched** (existing terms): line and name versions, event number, ancestry, verdict (rule 3),
whole (R3e), undecided, the `Cut` (R9.3), lock keys, root, upstream, fork. **No domain model change.** A line version
(a `String` cut by `split_once(':')`, `mod.rs:1416`) and an event number (a bare `u64`) get types.

| Rule | Copies today |
|---|---|
| Ancestry walk | `tree_reconcile::verdict::descends`, `referee::line_descends`, `referee::name_descends` |
| Rule-3 verdict | `tree_reconcile::verdict` (lines), `referee::name_verdict` (names): the same five cases |
| Name combine | `referee::combine_names` and `sync::combine`, byte for byte |
| Line-graph union | `combined_graph`, inline in `unseen_parent_line`, an unchecked `extend` in `judge` |
| R3e | `land_save` loses a file whole; `judge` the save, calling `verdict` again per line |
| P2 prefix | `queue::prefix` (used only by its test) and the loop in `plan_sync` |
| Version allocator | `format!("{seq}:{index}")` closures in `build_close`, `build_rename` |
| Envelope test, messages | read in `snapshot_at` (1200), `checkout` (2245); four format strings (675, 1489, 1670, 2188) |

**Patterns.** Newtype (Evans, *DDD*; Rust API Guidelines C-NEWTYPE); a container generic over its key, like `BTreeMap`;
parse, don't validate (King, 2019); functional core, imperative shell (Bernhardt, 2012); ports and adapters
(Cockburn): `Landed`; unit of work over a write-ahead log (Fowler, *PoEAA*; ARIES); smart constructor (Minsky): `Cut`;
differential testing (McKeeman, 1998): slice 2; Move Function, Extract Class (Fowler, *Refactoring*): slices 3-4.

## 3. Design rules (design choices)

1. **One implementation per rule.** A grep outside `#[cfg(test)]` is the deletion test: `line_descends`,
   `name_descends`, `name_verdict`, `combine_names`, `combined_graph`, `land_save`, `split_once(':')`, and
   `queue.rs`'s cut-length `fn prefix`, find nothing; `"Agentfs-Format`, `"refs/queue/` and 5b's
   `queue_ref_prefix` find one home; `"main"` in `tree_actor/` and `tree_git.rs` finds nothing (5b removed it).
2. **The commit point stays in the actor, by construction.** Only `Volume` holds the `Journal`; `run_step` and
   `journal_record` are the only appends. Modules prepare (pure or object writes) and install (cannot fail).
   Failpoints, their order, stage timers and a step's `RefUpdate` order (branch, queue entry, collision ref, object
   table, enqueues) stay.
3. **Decision order is a key, not a push order.** `whole` sorts candidates by `Order` (names before lines, then file
   identity, then position in the incoming line map) before R3e reads the first loss. That is today's push order
   (probe 4), and it is written into `Agentfs-Collisions`. So the units may run in the frame's order ("undecided,
   verdicts, names (to a fixed point), placing, whole") without changing a byte.

## 4. Interfaces

### Slice 1: typed versions

```rust
// tree_reconcile.rs (public): generic over the caller's version; tests keep String ids ("r1", "e1").
/// What a caller numbers lines with (was `pub type Id = String`).
pub trait Id: Clone + Ord + fmt::Debug + fmt::Display + Serialize + DeserializeOwned {}
impl<T: Clone + Ord + fmt::Debug + fmt::Display + Serialize + DeserializeOwned> Id for T {}
pub struct Line<V> { pub identity: V, pub version: V, pub bytes: Option<Vec<u8>> }
pub struct LineMap<V> { pub lines: Vec<Line<V>>, pub ancestors: BTreeMap<V, Option<V>> }
impl<V: Id> LineMap<V> {
    pub fn append(&self, suffix: &[u8], next: impl FnMut() -> V) -> Result<Self, Error>;
    pub fn align(&self, image: &[u8], next: impl FnMut() -> V) -> Result<Self, Error>;
    // bytes, encode, decode, validate: same bodies, generic
}
pub enum Error { /* ... */ DuplicateId(String) /* the version's Display */ }

// tree_actor/tree.rs (crate-private; journal numbering stays out of the line-map library)
/// The journal number of the step that wrote a version. Name versions are event numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct EventNumber(u64);
impl EventNumber {
    pub(super) const fn new(seq: u64) -> Self;          // once per step, in `run_step`
    pub(super) const fn get(self) -> u64;
    /// An object is numbered by the event that made it: a create (mod.rs:2051, 2065) or a file a
    /// sync brings back (R11.3, sync.rs:131). The one place an event number becomes an object id.
    pub(super) const fn object(self) -> u64;
}
/// "<event>:<index>", index from 1. JSON is the same string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub(super) struct LineVersion { event: EventNumber, index: u64 }
impl LineVersion { pub(super) const fn event(self) -> EventNumber; }
impl fmt::Display for LineVersion; impl FromStr for LineVersion { type Err = Error; }   // Error::Format
/// One save's line versions in order: <event>:1, <event>:2, ...
pub(super) struct Versions { event: EventNumber, last: u64 }
impl Versions { pub(super) const fn of(event: EventNumber) -> Self; pub(super) fn next(&mut self) -> LineVersion; }
pub(super) type Lines = LineMap<LineVersion>;
```

The actor uses `Lines`; `NameHistory`'s head and keys, `NameVersion::over` and `owners` take `EventNumber`. **Still
`u64`, on purpose:** object and handle ids (public in `Volume` and `drain_changed`), journal `Record::seq` and page-write
numbers (not versions), and `MergeFileIdentity`'s inside. JSON values are unchanged (probe 1); new `ancestors` keys
sort numerically, and any reader accepts either order (G5). **Test cost:** `tests/tree_reconcile.rs`'s alignment
test keeps its `"r1"` ids (its other nine move in slice 2); `tests/tree_actor.rs:648-668` reads `LineMap<String>`;
the referee and sync unit tests (515 lines, about 40 literals) take a `v(event, index)` helper. About 300 lines.

### Slice 2: rules once (`tree_actor/referee/`, crate-private)

```rust
// referee/ancestry.rs: the port the other units ask
pub(super) trait Graph { type Version: Copy + Ord;
    /// Ok(None) for a first version; Err(Broken::Missing) when the graph lacks `v`.
    fn over(&self, v: Self::Version) -> Result<Option<Self::Version>, Broken>; }
impl Graph for BTreeMap<LineVersion, Option<LineVersion>> { type Version = LineVersion; }
impl Graph for NameHistory { type Version = EventNumber; }
#[derive(Debug, thiserror::Error)] pub(super) enum Broken { #[error("cyclic ancestry")] Cycle, #[error("missing ancestor")] Missing }
impl From<Broken> for Error;     // Error::Format, EIO; lines were Reconcile(InvalidMap), also EIO
pub(super) fn descends<G: Graph + ?Sized>(g: &G, v: G::Version, ancestor: G::Version) -> Result<bool, Broken>;
/// Error::Format when the two disagree on what a version was written over.
pub(super) fn union(a: &Ancestors, b: &Ancestors) -> Result<Ancestors>;
pub(super) type Ancestors = BTreeMap<LineVersion, Option<LineVersion>>;
/// Commit reachability: the referee's one read of history (tree history adapts it).
pub(super) trait Landed { fn landed(&mut self, save: EventNumber) -> Result<bool>; }

// tree.rs: NameHistory moves here
impl NameHistory { /// Both histories' versions; Error::Format when one differs. Keeps self.head.
    pub(super) fn combine(&self, other: &NameHistory) -> Result<NameHistory>; }

// referee/verdicts.rs: rule 3 for names and lines; the unseen-line and ended-file rules
pub(super) struct Held<V> { pub version: V, pub live: bool }
pub(super) enum Verdict<V> { Apply, Drop, Lose { winner: V } }
pub(super) fn verdict<G: Graph + ?Sized>(g: &G, upstream: Option<Held<G::Version>>, incoming: Held<G::Version>)
    -> Result<Verdict<G::Version>, Broken>;
/// Where a candidate sits in the decision (design rule 3). Derived Ord: names, then lines.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Order { pub line: bool, pub file: MergeFileIdentity, pub at: usize }
pub(super) struct Candidate { pub order: Order, pub lost: Version, pub loss: Option<(Version, CollisionKind)> }
pub(super) fn verdicts(upstream: &Snapshot, base: &Snapshot, incoming: &Snapshot, prior: &PriorDecisions)
    -> Result<Vec<Candidate>>;
// referee/names.rs: rename-over displacement (R11.2), to a fixed point; the sync's clash fallback
pub(super) fn displace(upstream: &Snapshot, base: &Snapshot, incoming: &Snapshot, c: &mut [Candidate]) -> Result<()>;
// referee/placing.rs: one file's applied lines placed, and its joins (R10.4, R10.8); sync's placing (R10.6, R10.9)
/// `verdicts`: the file's from verdicts(), so no verdict is computed twice. `land` folds each join (lost, winner)
/// into the candidates: it sets that line's loss, adding the candidate if verdicts dropped the line.
pub(super) fn place(upstream: &Lines, base: &Lines, incoming: &Lines,
                    verdicts: &BTreeMap<LineVersion, Verdict<LineVersion>>) -> Result<Placed>;
pub(super) struct Placed { pub joins: Vec<(LineVersion, LineVersion)>, pub lines: Lines }
pub(super) fn sync_file(upstream: &Lines, fork: &Lines, undecided: &BTreeSet<LineVersion>) -> Result<Lines>;
// referee/whole.rs: R3e, its one home
/// Sorts by Order; a loss anywhere makes every candidate a collision (Whole, named for the first loss's
/// winner, where it did not lose on its own) and the snapshot is the upstream's; else `applied`.
pub(super) fn whole(upstream: &Snapshot, applied: Snapshot, candidates: Vec<Candidate>) -> Decision;

// referee/mod.rs
pub(super) struct Undecided { pub lines: BTreeSet<LineVersion>, pub names: BTreeSet<(MergeFileIdentity, EventNumber)> }
pub(super) type PriorDecisions = BTreeMap<Version, Version>;
/// verdicts, displace, place, whole: the frame's order.
pub(super) fn land(upstream: &Snapshot, base: &Snapshot, incoming: &Snapshot, prior: &PriorDecisions) -> Result<Decision>;
pub(super) fn undecided<L: Landed + ?Sized>(fork: &Snapshot, landed: &mut L) -> Result<Undecided>;
pub(super) fn ended_files(upstream: &Snapshot, undecided: &Undecided) -> Result<BTreeSet<MergeFileIdentity>>;
/// undecided (given), placing over every file either side holds, then names to a fixed point over the placed
/// files; a live name left with no placed file is Error::Format, as today's `(None, None)`.
pub(super) fn sync(upstream: &Snapshot, fork: &Snapshot, undecided: &Undecided,
                   last_lines: &BTreeMap<MergeFileIdentity, Lines>) -> Result<Snapshot>;
/// A step's lock keys (R9.3: the referee owns what a commit touches).
pub(super) fn touches(before: &Snapshot, after: &Snapshot) -> BTreeSet<queue::Key>;
```

`tree_reconcile` loses its public `verdict`, `Verdict`, `land_save`, `Landing`, `Collision`, `CollisionKind`, `sync`
(G4). Their users are `referee.rs`, `sync.rs` and `tests/tree_reconcile.rs`, whose nine tests of them move to the
`referee/` units, same cases and assertions, through `verdict`, `land` and `sync_file`. **Proof: a differential
test.** Slice 2 keeps, verbatim in `#[cfg(test)] referee/oracle.rs`, slice 1's `judge` and `sync::sync` and all they
call that slice 2 deletes: the seven items above, `tree_reconcile`'s five private helpers (`combined_graph` to
`place`), and `referee.rs`'s and `sync.rs`'s private helpers (`name_descends` ... `ended`): about 900 lines.
`land_and_sync_equal_the_oracle` draws 20,000 seeded cases (a 20-line splitmix64): 1-4 files built through `align`
and `Versions`, then an upstream and an incoming save of 0-3 edits each (edit, insert, delete, create, rename,
unlink, rename-over), sometimes over an earlier losing save. It asserts equal `Decision`s (collision order included)
and sync snapshots, prints the failing seed, and **fails unless Apply, Drop, Lose, each `CollisionKind`, a join, a
rename-over and an ended-file edit each occur at least 50 times**. PR 5.1's last commit deletes oracle and test after
their green run on slice 2's SHA: a second copy on `main` is a dual path. `undecided` reads git; conformance covers it.

### Slice 4: the modules take their own state

```rust
// queue.rs (private to tree_actor)
pub(super) enum Key { File(MergeFileIdentity), Name(String) }   // was File(u64): same order
pub(super) fn eligible(plans: &[Plan], open: &BTreeSet<Key>) -> Option<usize>;      // P1, unchanged
/// Fields private: only `cut` makes one, and only a Cut builds a sync (R9.3, R9.9).
pub(super) struct Cut<T> { taken: Option<T>, remaining: bool, keys: BTreeSet<Key> }
impl<T> Cut<T> { pub fn taken(&self) -> Option<&T>; pub fn remaining(&self) -> bool;
                 pub fn keys(&self) -> &BTreeSet<Key>; pub fn into_taken(self) -> Option<T>; }
/// Pulls `steps` oldest first, none after the first whose keys meet `open`. The caller's `scan` over
/// `History::ahead_of` carries the previous snapshot for `referee::touches(prev, next)`.
pub(super) fn cut<T, E>(steps: impl IntoIterator<Item = Result<(T, BTreeSet<Key>), E>>, open: &BTreeSet<Key>)
    -> Result<Cut<T>, E>;
pub(super) fn held(tree: &Tree, open_objects: impl IntoIterator<Item = u64>) -> BTreeSet<Key>;

// history/upstreams.rs: 5b's root record and helpers, moved, not widened
/// Each tree's upstream, as data of any depth (a transaction's upstream will be its fork). One level is a limit
/// of the public API, not of this type: `Volume::fork()` forks only the root, so no call writes a fork of a fork.
pub(super) struct Upstreams { root: RootName, upstreams: BTreeMap<String, String> }
impl Upstreams {
    /// 5b's open checks, unchanged: Error::Format unless the root's branch exists and it has no upstream.
    pub(super) fn recovered(root: RootName, upstreams: BTreeMap<String, String>, pointers: &Pointers) -> Result<Self>;
    pub(super) fn root(&self) -> &RootName;           // Volume::root() (5b, public) delegates here
    pub(super) fn is_root(&self, tree: &str) -> bool;
    pub(super) fn upstream_of(&self, tree: &str) -> Option<&str>;
    pub(super) fn forks_of<'a>(&'a self, tree: &'a str) -> impl Iterator<Item = &'a str> + 'a;  // 5b's
    /// The journal's `fork: (name, upstream)` as written; recovery's fold and `install` both call it.
    pub(super) fn add(&mut self, fork: String, upstream: String);
    pub(super) fn as_map(&self) -> &BTreeMap<String, String>;     // checkpoint, git config
}
// history/refs.rs: one fn each: branch, queue_ref_prefix (5b's), queue entry, collision ref, object table
// history/message.rs: every commit message; golden test of each variant's bytes against 22d55f9's strings
pub(super) enum Message<'a> {
    /// "Root", writer agentfs, event 0, and NO Agentfs-Format line: its absence marks the Root commit.
    Root { tree: &'a str },
    /// Subject "{action} by {writer}" (Create, Rename, Unlink, Direct); Agentfs-Cause: Direct; Agentfs-Open if any.
    Direct { action: &'a str, writer: &'a str, tree: &'a str, event: EventNumber, open: Option<u64> },
    /// Agentfs-Collisions (JSON, in order) when any.
    Landing { tree: &'a str, event: EventNumber, collisions: &'a [Collision] },
    Sync { tree: &'a str, event: EventNumber },
}
impl Message<'_> { pub(super) fn render(&self) -> Result<String>; }   // Err only from serialising collisions
pub(super) fn is_envelope(message: &[u8]) -> bool;                    // snapshot_at and checkout
// history/mod.rs
pub(super) struct History { repo: TreeGit, owners: BTreeMap<EventNumber, Oid>, upstreams: Upstreams }
impl History {
    pub(super) fn landed_into(&self, upstream: Oid) -> impl Landed + '_;   // cached per commit
    pub(super) fn prior_decisions(&self, head: Oid) -> Result<PriorDecisions>;
    pub(super) fn last_lines(&self, at: Oid, wanted: BTreeSet<MergeFileIdentity>) -> Result<BTreeMap<MergeFileIdentity, Lines>>;
    pub(super) fn ahead_of(&self, upstream_head: Oid, fork_head: Oid) -> Result<Vec<Oid>>;  // oldest first
    pub(super) fn pending_from(&self, upstream_queue: &[(String, Oid)], tree: &str) -> Result<bool>; // R6.1
    /// Reads the root's queue only (5b): with `fork()`, the one other place one level is assumed.
    pub(super) fn rebuild_owners(&mut self, tips: &[Oid]) -> Result<()>;
    pub(super) fn saved(&mut self, event: EventNumber, head: Oid);          // install, cannot fail
    pub(super) fn upstreams(&self) -> &Upstreams;
}

// journal/{mod,fold,pointers}.rs: `Folded` keeps today's fields plus 5b's `root`; its opens, page_writes and
//   saved_through travel together as `FoldedRecording`

// recording/{mod,pages}.rs: holds no Journal (design rule 2)
pub(super) struct Recording { handles: BTreeMap<u64, Handle>, pending_closes: Vec<u64>,
                              pages: BTreeMap<(String, u64), ObjectPages>, changed: Vec<(String, u64)> }
impl Recording {
    /// Recovery (open(), 785-813): the fold's opens and page writes, before their closes run; marks anonymous
    /// opens in their trees; Error::Format for an unknown tree.
    pub(super) fn recovered(folded: FoldedRecording, trees: &mut BTreeMap<String, Tree>) -> Result<Self>;
    /// Compaction (compact(), 1994-2028): the checkpoint's opens and page marks; which records are kept.
    pub(super) fn checkpoint(&self, trees: &BTreeMap<String, Tree>) -> (Vec<OpenRecord>, Vec<(String, u64, u64)>);
    pub(super) fn retains(&self, entry: &Entry, seq: u64) -> bool;
    /// After a take installs (was `pages_take`'s tail; the actor's `take` wraps `step`, then calls this).
    pub(super) fn note_changed(&mut self, tree: &str, before: &Bytes, after: &Bytes);  // Tree::bytes' map
    pub(super) fn drain_changed(&mut self) -> Vec<(String, u64)>;
    /// Checks `call` on the writer's view and the saved bytes; changes nothing.
    pub(super) fn prepare_call(&self, handle: u64, call: &Call, saved: &[u8]) -> Result<(CallHead, Vec<u8>)>;
    /// The call is in the journal as `seq`; memory follows. Cannot fail.
    pub(super) fn called(&mut self, handle: u64, seq: u64, call: Call, view: Vec<u8>);
    pub(super) fn open_objects<'a>(&'a self, tree: &'a str) -> impl Iterator<Item = u64> + 'a;
    // Edits on a copy of a Tree, each giving the actor's Draft its tree: create, close (Changed | Unchanged |
    // Orphan | Anonymous), rename, unlink, first_link; each takes `versions: &mut Versions`.
}
```

`Volume` keeps its API and holds `journal`, `pointers`, `trees`, `recording`, `history`, `metrics`, the copy flags.
A step builds a `Draft` (an edit or a decision, plus history's commit and refs), records it, installs it.

| Module after the step | Owns (rules) |
|---|---|
| `tree_reconcile` | line maps, alignment |
| `tree_actor/tree.rs` | shared types, `NameHistory`, `EventNumber`, `LineVersion`, `Versions` |
| `recording/{mod,pages}` | close-to-open, calls, pending closes, orphans, R1.4, name and writer checks |
| `queue.rs` | P1 eligibility, the `Cut` (R9.3, R9.9), held keys |
| `referee/{mod,ancestry,verdicts,names,placing,whole}` | rule 3, R3a-e, R10, R11, touches |
| `history/{mod,upstreams,refs,message,envelope}` | commits, messages, fast-forward (R6.1), lineage, root and upstreams, refs, decision index |
| `journal/{mod,fold,pointers}` | commit point format, recovery fold, compaction, pointers |
| `mod.rs` | composition: `Volume`, `Draft`, step, install, take, copy-out, progress order |

**Crossings.** R9.3: history walks, the referee says what each commit touches, queue cuts. R6.1: history decides
from the upstream's queue (`queue_ref_prefix`). R11.3: the referee's sync numbers returned files
(`EventNumber::object`); recording takes them. Undecided: the referee asks `Landed`, history answers. Recovery:
journal folds; history takes pointers and upstreams, recording opens and page writes; the actor closes abandoned
handles. Compaction: the checkpoint from `Upstreams` and `Recording::checkpoint`; `retains` picks kept records.
Takes: `take` wraps `step`, then `note_changed`. **Later engine source** (module-cut's later row; no code here): it
would emit at `install` (each recorded `Draft`), `History::saved` (a save's commit), and the actor's error returns
from `run_step` and `prepare_call` (a failed call, ` !<errno>`, never journalled).

## 5. Risks

| Risk | Met by |
|---|---|
| Recording (principle 1): an append moves after a memory change | Design rule 2; byte calls keep check, journal, memory (1066-1080) as `prepare_call`, `journal_record`, `called`; crash, recording-fault and power-loss jobs in the dispatch |
| R3e's winner or order changes; conformance skips whole winners (`invariants.rs:433`) | Rule 3; probe 4; the differential test compares `Decision`s |
| A message changes bytes; the Root stops reading as Root (it is known by its missing `Agentfs-Format`) | `Message::Root` has none; golden bytes per variant, rendered with tree `main` against 22d55f9's strings |
| Typed versions change history (G5) | Probe 1; golden fixtures are blobs (`metadata.json`, `namespace.json`) and `Agentfs-Collisions` trailers from a 22d55f9 build, checked in as bytes, never a whole state area (5b refuses format 2) |
| Frame order places a file sync then hides; `Held::live` read before `Drop` | Equal results (differential test); only corrupt data fails earlier, EIO either way |
| `Cut` reads more than today | A counting-iterator unit test |
| `one_spawn_site` fails on a subdirectory (probe 3) | Slice 2, which makes the first, makes it recurse |
| finding-51's header says "agentfs-core unchanged since" | Slice 3 drops the clause; the patch applies at its base 7474470 |

## 6. Slices, PRs and gates

**Order: 5b, then 5.** 5b gives the root a record and runs conformance under `dev/trunk`, so step 5's split cannot
re-create a `"main"` unseen and only moves 5b's helpers. Each PR starts from main after the one before merges.

| PR | # | Slice (one commit each, pushed; ci and image green on each SHA) | Size (estimate) |
|---|---|---|---|
| 5.1 | 1 | **Tracer: typed versions** through JSON, `tree_reconcile`, referee, sync, actor; `Versions`; golden format test | ~300 lines |
| 5.1 | 2 | Rules once, `referee/` units in frame order; `sync.rs` folds in; nine tests moved; oracle and differential test, then a commit deleting both; `one_spawn_site` recurses; rows (`docs_reference`) | ~800, net fewer |
| 5.2 | 3 | Verbatim moves to `recording/`, `history/`, `journal/`, `tree.rs` (visibility may widen); rows; finding-51 header | ~2,400 moved |
| 5.3 | 4 | Modules own their state: `Cut`, `held`, `Upstreams`, `Message`, `is_envelope`, refs, `Folded`, recording crossings | ~800 lines |

Each slice: fmt, `clippy -D warnings`, `cargo test --workspace` locally; ci (~5 min) and image (~5-9 min).
**PR gates.** 5.1 and 5.3 each get one `phase-1.yml` dispatch on the PR head (replay at floors 74/94, mounts 71/16,
seeds 1-500 in four shards, page cache, recording faults, power loss, 0 mismatched; step 4's run 43146523186: 73 min
wall): 5.1's proves the rewrite, 5.3's covers 5.2 and is the step gate (G2). 5.2 only moves code: ci, image, and
`git diff --color-moved=dimmed-zebra` showing no changed line beyond `use`, `mod` and visibility. **Model gate:
none**: nothing under `crates/agentfs-model/` changes. bench and power-loss run after landing.

**Costs.** Three reviews (~1,100, ~2,400 moved, ~800 lines) instead of one of ~4,300 cost a second dispatch (73 min)
and three serial owner merges. If step 6's Q2 is B, the model's `land` keeps `dir` and the engine's has none, so the
one decomposition differs there until A converges them. Step 6 shares only `module-cut.md` (each its own row) with
step 5; only 5b-before-5 is serial.

## 7. Docs

`docs/architecture.md` (rows; `tree_reconcile.rs` becomes "line maps and alignment, generic over the caller's
version"); `docs/library/core.md`'s tree section (one sentence: `tree_reconcile` is the public line-map library);
`docs/trees.md` "The library" (versions); rules-ledger R12 (the test's file); `future/shared-mappings.md`
(`recording/pages.rs`); `future/module-cut.md`: row 5 built, costs, and "a landing's collision order is a key, not
the order the units run". No page is deleted (R1).

## 8. Probes (real output, on 22d55f9; patches under `.../scratchpad/next/probe-5/`)

1. **Typed JSON.** A typed `LineMap` reread `align`'s bytes: values equal, keys now `{"9:1",..,"10:1",..}`; `r1`
   refused (hence the generic library). (2) **R3e once.** Rebuilt candidates equal `judge`'s, generic `verdict`
   equals `name_verdict`, over the core tests (28 files, 19 name verdicts).
3. **`one_spawn_site`** with a subdirectory: `panicked ... Os { code: 21, kind: IsADirectory }`.
4. **Key order is push order** (`probe4-key-order.diff`): `judge` asserting its candidates sorted by `Order` held on
   every call; `--lib tree_actor::` 32, `--test tree_actor` 41, `tree_line_rules` 4, `tree_pages` 16 passed; 38
   decisions, 48 candidates, 16 with a loss, 6 with several. (5) **Messages (read).** Root (675) lacks
   `Agentfs-Format`; 1489, 1670, 2188 have it; Direct's subject is `{cause} by {writer}`, causes Create, Rename,
   Unlink, Direct. Probes 2 and 4 agree but cover little; the differential test carries the proof.

**NOT RUN:** the differential test (unwritten; 20,000-seed runtime unmeasured); probes 1-5 after 5b (unbuilt); any
model run; conformance, `phase1-replay`; FUSE, mounts, crash, power-loss; clippy, fmt on probes; git 2.43, not 2.47.3.

## 9. Owner questions

None. Every choice above is decided and revisable; numeric key order in new `metadata.json` rests on G4 and G5.

| New term | Means | Why needed | Existing term it could be |
|---|---|---|---|
| `EventNumber` | A step's journal number; numbers versions and objects | One `u64`, five roles | "event number" |
| `LineVersion`, `Versions` | One line version; a save's allocator | Strings parsed late; two closures | "line version"; `next_id` |
| `Graph`, `descends`, `Broken` | Ancestry, its walk, its failures | Three walks | "ancestry" |
| `Held`, `Verdict<V>`, `Candidate`, `Order` | Rule 3's input and answer; a version, its loss, its place | Two verdicts; R3e twice | judge's `candidates` |
| `Landed` | The referee's one read of history | Pure referee | the model's `Lineage` |
| `Upstreams` | The root and each tree's upstream | 5b's helpers' home | the journal's `upstreams` |
| `Message` | Every commit message | Four strings | the trailers |
| oracle | Old code in one commit's tests, compared with the new | Slice 2's proof | none |

## Review disposition

**Review.** 1-10 **changed** (5b first, grep gate, slice 2 alone, differential test, `Message`, order a key, generic
library, crossings, `Upstreams`, length). `Held::live`, slice 4's hot path **refuted**; `Landed`'s error **parked**.

**Critic.** 3 `Upstreams` one level by construction: **changed**: data of any depth; one level is a limit of
`Volume::fork()` (no upstream argument) and `rebuild_owners`. Cost: a journal holding a nested fork, which no call
writes, would open unrefused. 4 `tree_reconcile` deletion: **changed** (nine tests moved; the oracle keeps every
deleted callee, gone by PR 5.1's end); its `docs/library/core.md` half **refuted**: the page names only
`tree_actor::Volume` (lines 644-660); `architecture.md:47` is the one row. 5 **changed** (re-probe after 5b; golden
blobs). 8 **changed** (section 4). 9 **changed** (three PRs, gates per PR). 10 **changed**. 11, 12 **stated as costs**.
